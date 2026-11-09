use crate::{ClientStore, CreateOutboxOperation, RegistrationBinding, StoreError, storage_error};
use notryx_protocol::{CreateNodeRequest, NodeKind, OperationReceipt, UploadResponse};
use sqlx::{Connection, Row, SqliteConnection};

pub(crate) const UPGRADE_V28: &str = "
    CREATE TABLE diagram_creations (
        operation_id TEXT PRIMARY KEY REFERENCES client_outbox(operation_id),
        upload_json TEXT NOT NULL CHECK(length(upload_json) BETWEEN 1 AND 4096),
        upload_unavailable INTEGER NOT NULL DEFAULT 0 CHECK(upload_unavailable IN (0,1))
    );
    PRAGMA user_version=28;
";

pub(crate) async fn validate_upload(
    connection: &mut SqliteConnection,
    item: &CreateOutboxOperation,
    receipt: Option<&OperationReceipt>,
) -> Result<bool, StoreError> {
    let saved: Option<(String, i64)> = sqlx::query_as(
        "SELECT upload_json,upload_unavailable FROM diagram_creations WHERE operation_id=?",
    )
    .bind(&item.operation_id)
    .fetch_optional(connection)
    .await
    .map_err(storage_error)?;
    match (item.request.upload_id, saved) {
        (None, None) => Ok(false),
        (Some(id), Some((json, unavailable))) => {
            let upload: UploadResponse =
                serde_json::from_str(&json).map_err(|_| StoreError::UnsupportedFormat)?;
            validate_request(&item.request, &upload).map_err(|_| StoreError::UnsupportedFormat)?;
            if id.to_string() != upload.upload_id || !matches!(unavailable, 0 | 1) {
                return Err(StoreError::UnsupportedFormat);
            }
            if let Some(OperationReceipt::Created { node, .. }) = receipt
                && (node.content_hash.as_ref() != Some(&upload.content_hash)
                    || node
                        .size
                        .as_deref()
                        .and_then(|size| size.parse::<u64>().ok())
                        != upload.size.parse::<u64>().ok())
            {
                return Err(StoreError::InvalidReceipt);
            }
            Ok(unavailable == 1)
        }
        _ => Err(StoreError::UnsupportedFormat),
    }
}

fn validate_request(
    request: &CreateNodeRequest,
    upload: &UploadResponse,
) -> Result<(), StoreError> {
    let json = serde_json::to_vec(upload).map_err(|_| StoreError::InvalidRequest)?;
    let checked: UploadResponse =
        serde_json::from_slice(&json).map_err(|_| StoreError::InvalidRequest)?;
    if checked != *upload
        || request.upload_id.map(|id| id.to_string()).as_ref() != Some(&upload.upload_id)
        || request.kind != NodeKind::File
        || !request.name.to_ascii_lowercase().ends_with(".drawio.png")
        || upload
            .size
            .parse::<u64>()
            .ok()
            .is_none_or(|size| size == 0 || size > 25 * 1024 * 1024)
        || upload
            .expires_at
            .parse::<u64>()
            .ok()
            .is_none_or(|expiry| expiry == 0)
    {
        return Err(StoreError::InvalidRequest);
    }
    Ok(())
}

impl ClientStore {
    pub async fn queue_diagram_create(
        &mut self,
        binding: &RegistrationBinding,
        request: &CreateNodeRequest,
        upload: &UploadResponse,
    ) -> Result<CreateOutboxOperation, StoreError> {
        validate_request(request, upload)?;
        let registration = self.registration_id(binding, true).await?;
        let id = uuid::Uuid::now_v7().to_string();
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let operation =
            crate::outbox::queue_create_on_connection(&mut tx, registration, binding, request, &id)
                .await?;
        sqlx::query("INSERT INTO diagram_creations(operation_id,upload_json) VALUES (?,?)")
            .bind(&id)
            .bind(serde_json::to_string(upload).map_err(|_| StoreError::InvalidRequest)?)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(operation)
    }

    pub async fn abandon_unavailable_diagram_upload(
        &mut self,
        binding: &RegistrationBinding,
        item: &CreateOutboxOperation,
    ) -> Result<(), StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row=sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=? AND operation_kind='create'")
            .bind(registration).bind(&item.operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let receipt: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
        if receipt.is_some() {
            return Err(StoreError::InvalidRequest);
        }
        let current = crate::outbox::pending_create_operation(&row, binding)?;
        if current.operation_seq != item.operation_seq
            || current.request != item.request
            || current.binding != item.binding
            || current.request.upload_id.is_none()
            || !matches!(
                current.status,
                crate::OutboxStatus::Sent | crate::OutboxStatus::Abandoning
            )
        {
            return Err(StoreError::InvalidRequest);
        }
        validate_upload(&mut tx, &current, None).await?;
        // Only an admission-time upload rejection can schedule this terminal no-op.
        sqlx::query("UPDATE diagram_creations SET upload_unavailable=1 WHERE operation_id=?")
            .bind(&item.operation_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        sqlx::query("UPDATE client_outbox SET abandon_state='requested' WHERE operation_id=? AND abandon_state='none'")
            .bind(&item.operation_id).execute(&mut *tx).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }
}
