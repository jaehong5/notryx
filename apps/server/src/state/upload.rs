use super::{State, db_error};
use crate::{Error, UploadLimits};
use notryx_domain::OperationId;
use notryx_server_storage::StoredUpload;
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct UploadRecord {
    pub id: Uuid,
    pub device_id: Uuid,
    pub status: String,
    pub expected_hash: String,
    pub declared_size: Option<u64>,
    pub reserved_bytes: u64,
    pub started_at: i64,
    pub expires_at: i64,
    pub content_hash: Option<String>,
    pub size: Option<u64>,
    pub pending_operation_id: Option<OperationId>,
}
impl UploadRecord {
    pub fn stored(&self) -> Result<StoredUpload, Error> {
        Ok(StoredUpload {
            id: self.id,
            hash: self.content_hash.clone().ok_or(Error::InvalidFormat)?,
            size: self.size.ok_or(Error::InvalidFormat)?,
        })
    }
}

pub(super) async fn initialize(connection: &mut SqliteConnection) -> Result<(), Error> {
    sqlx::raw_sql("CREATE TABLE uploads (
        upload_id TEXT PRIMARY KEY NOT NULL, device_id TEXT NOT NULL REFERENCES devices(device_id),
        status TEXT NOT NULL CHECK(status IN('receiving','ready','consumed')),
        expected_hash TEXT NOT NULL, declared_size INTEGER, reserved_bytes INTEGER NOT NULL,
        started_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, content_hash TEXT, size INTEGER,
        pending_operation_id TEXT UNIQUE REFERENCES write_intents(operation_id) ON DELETE SET NULL);")
        .execute(connection).await.map_err(db_error)?;
    Ok(())
}

fn decode(row: SqliteRow) -> Result<UploadRecord, Error> {
    fn number(row: &SqliteRow, key: &str) -> Result<Option<u64>, Error> {
        row.try_get::<Option<i64>, _>(key)
            .map_err(db_error)?
            .map(|value| value.try_into().map_err(|_| Error::InvalidFormat))
            .transpose()
    }
    let id: String = row.try_get("upload_id").map_err(db_error)?;
    let device: String = row.try_get("device_id").map_err(db_error)?;
    let record = UploadRecord {
        id: id.parse().map_err(|_| Error::InvalidFormat)?,
        device_id: device.parse().map_err(|_| Error::InvalidFormat)?,
        status: row.try_get("status").map_err(db_error)?,
        expected_hash: row.try_get("expected_hash").map_err(db_error)?,
        declared_size: number(&row, "declared_size")?,
        reserved_bytes: number(&row, "reserved_bytes")?.ok_or(Error::InvalidFormat)?,
        started_at: row.try_get("started_at").map_err(db_error)?,
        expires_at: row.try_get("expires_at").map_err(db_error)?,
        content_hash: row.try_get("content_hash").map_err(db_error)?,
        size: number(&row, "size")?,
        pending_operation_id: row
            .try_get::<Option<String>, _>("pending_operation_id")
            .map_err(db_error)?
            .map(|s| s.parse().map_err(|_| Error::InvalidFormat))
            .transpose()?,
    };
    let max = notryx_server_storage::MAX_FILE_BYTES as u64;
    if record.id.to_string() != id
        || record.id.get_version_num() != 4
        || record.id.get_variant() != uuid::Variant::RFC4122
        || record.device_id.to_string() != device
        || !crate::upload::valid_hash(&record.expected_hash)
        || record.declared_size.is_some_and(|v| v > max)
        || record.reserved_bytes > max
        || record.started_at < 0
        || record.expires_at <= record.started_at
    {
        return Err(Error::InvalidFormat);
    }
    match record.status.as_str() {
        "receiving"
            if record.pending_operation_id.is_none()
                && record.content_hash.is_none()
                && record.size.is_none()
                && record.reserved_bytes == record.declared_size.unwrap_or(max)
                && record
                    .started_at
                    .checked_add(crate::upload::RECEIVE_SECONDS)
                    == Some(record.expires_at) => {}
        "ready" | "consumed"
            if record.content_hash.as_deref() == Some(record.expected_hash.as_str())
                && record.size.is_some_and(|size| {
                    size <= max
                        && size == record.reserved_bytes
                        && record.declared_size.is_none_or(|declared| declared == size)
                })
                && (record.status != "consumed" || record.pending_operation_id.is_none()) => {}
        _ => return Err(Error::InvalidFormat),
    }
    Ok(record)
}

pub(super) async fn validate(connection: &mut SqliteConnection) -> Result<(), Error> {
    for row in sqlx::query("SELECT * FROM uploads")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?
    {
        let record = decode(row)?;
        if let Some(operation_id) = record.pending_operation_id {
            let text: String =
                sqlx::query_scalar("SELECT intent FROM write_intents WHERE operation_id=?")
                    .bind(operation_id.to_string())
                    .fetch_one(&mut *connection)
                    .await
                    .map_err(db_error)?;
            let intent: super::create::CreateIntent = super::decode(&text)?;
            let stream: String =
                sqlx::query_scalar("SELECT stream_id FROM operation_receipts WHERE operation_id=?")
                    .bind(operation_id.to_string())
                    .fetch_one(&mut *connection)
                    .await
                    .map_err(db_error)?;
            let device_stream: String =
                sqlx::query_scalar("SELECT stream_id FROM devices WHERE device_id=?")
                    .bind(record.device_id.to_string())
                    .fetch_one(&mut *connection)
                    .await
                    .map_err(db_error)?;
            if intent.upload_id != Some(record.id)
                || intent.operation_id != operation_id
                || intent.created.content_hash != record.content_hash
                || intent.created.size != record.size
                || stream != device_stream
            {
                return Err(Error::InvalidFormat);
            }
        }
    }
    let intents = sqlx::query_scalar::<_, String>("SELECT intent FROM write_intents")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    for text in intents {
        if let super::PendingIntent::Create(intent) = super::decode(&text)?
            && let Some(id) = intent.upload_id
        {
            let operation: Option<String> =
                sqlx::query_scalar("SELECT pending_operation_id FROM uploads WHERE upload_id=?")
                    .bind(id.to_string())
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(db_error)?
                    .flatten();
            if operation.as_deref() != Some(intent.operation_id.to_string().as_str()) {
                return Err(Error::InvalidFormat);
            }
        }
    }
    Ok(())
}

impl State {
    pub(crate) async fn uploads(&mut self) -> Result<Vec<UploadRecord>, Error> {
        sqlx::query("SELECT * FROM uploads")
            .fetch_all(&mut self.connection)
            .await
            .map_err(db_error)?
            .into_iter()
            .map(decode)
            .collect()
    }
    pub(crate) async fn upload(&mut self, id: Uuid) -> Result<Option<UploadRecord>, Error> {
        sqlx::query("SELECT * FROM uploads WHERE upload_id=?")
            .bind(id.to_string())
            .fetch_optional(&mut self.connection)
            .await
            .map_err(db_error)?
            .map(decode)
            .transpose()
    }
    pub(crate) async fn reserve_upload(
        &mut self,
        record: &UploadRecord,
        limits: &UploadLimits,
    ) -> Result<(), Error> {
        let rows = self.uploads().await?;
        let mut device_bytes = 0u64;
        let mut total_bytes = 0u64;
        let mut device_count = 0u64;
        let mut total_count = 0u64;
        for row in rows.iter().filter(|row| row.status != "consumed") {
            total_bytes = total_bytes
                .checked_add(row.reserved_bytes)
                .ok_or(Error::InvalidFormat)?;
            total_count += 1;
            if row.device_id == record.device_id {
                device_bytes = device_bytes
                    .checked_add(row.reserved_bytes)
                    .ok_or(Error::InvalidFormat)?;
                device_count += 1;
            }
        }
        if device_bytes + record.reserved_bytes > limits.max_device_bytes
            || total_bytes + record.reserved_bytes > limits.max_workspace_bytes
            || device_count >= limits.max_device_uploads as u64
            || total_count >= limits.max_workspace_uploads as u64
        {
            return Err(Error::UploadQuota);
        }
        sqlx::query("INSERT INTO uploads(upload_id,device_id,status,expected_hash,declared_size,reserved_bytes,started_at,expires_at) VALUES(?,?,'receiving',?,?,?,?,?)")
            .bind(record.id.to_string()).bind(record.device_id.to_string()).bind(&record.expected_hash).bind(record.declared_size.map(|v|v as i64)).bind(record.reserved_bytes as i64).bind(record.started_at).bind(record.expires_at)
            .execute(&mut self.connection).await.map_err(db_error)?;
        Ok(())
    }
    pub(crate) async fn complete_upload(
        &mut self,
        id: Uuid,
        hash: &str,
        size: u64,
        expires_at: i64,
    ) -> Result<(), Error> {
        let changed=sqlx::query("UPDATE uploads SET status='ready',content_hash=?,size=?,reserved_bytes=?,expires_at=? WHERE upload_id=? AND status='receiving'")
            .bind(hash).bind(size as i64).bind(size as i64).bind(expires_at).bind(id.to_string()).execute(&mut self.connection).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::UploadUnavailable);
        }
        Ok(())
    }
    pub(crate) async fn delete_upload(
        &mut self,
        id: Uuid,
        receiving_only: bool,
    ) -> Result<(), Error> {
        sqlx::query("DELETE FROM uploads WHERE upload_id=? AND pending_operation_id IS NULL AND (?=0 OR status='receiving')").bind(id.to_string()).bind(receiving_only).execute(&mut self.connection).await.map_err(db_error)?;
        Ok(())
    }
}
