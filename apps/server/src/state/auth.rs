use super::{State, db_error, parse};
use crate::{
    Error,
    auth::{AuthError, AuthIdentity},
};
use notryx_domain::WorkspaceId;
use notryx_protocol::WorkspaceRole;
use sqlx::{Connection, Row, SqliteConnection, TypeInfo, ValueRef, sqlite::SqliteRow};
use uuid::Uuid;

pub(super) async fn initialize(
    connection: &mut SqliteConnection,
    workspace_id: WorkspaceId,
) -> Result<(), Error> {
    sqlx::raw_sql(
        "CREATE UNIQUE INDEX auth_workspace_id ON workspaces(workspace_id);
         CREATE TABLE principals (
            principal_id TEXT PRIMARY KEY NOT NULL,
            workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
            workspace_role TEXT NOT NULL CHECK(workspace_role IN ('reader','editor','owner')));
         CREATE UNIQUE INDEX auth_one_owner ON principals(workspace_id) WHERE workspace_role='owner';
         CREATE TABLE devices (
            device_id TEXT PRIMARY KEY NOT NULL,
            principal_id TEXT NOT NULL REFERENCES principals(principal_id),
            stream_id TEXT UNIQUE NOT NULL REFERENCES operation_streams(stream_id),
            token_id TEXT UNIQUE NOT NULL,
            token_hash BLOB NOT NULL CHECK(typeof(token_hash)='blob' AND length(token_hash)=32),
            issued_at INTEGER NOT NULL CHECK(typeof(issued_at)='integer' AND issued_at>=0),
            idle_expires_at INTEGER NOT NULL CHECK(typeof(idle_expires_at)='integer' AND idle_expires_at>issued_at),
            absolute_expires_at INTEGER NOT NULL CHECK(typeof(absolute_expires_at)='integer' AND absolute_expires_at>=idle_expires_at),
            revoked_at INTEGER CHECK(revoked_at IS NULL OR (typeof(revoked_at)='integer' AND revoked_at>=issued_at)));
         CREATE TABLE pairing_secrets (
            pairing_id TEXT PRIMARY KEY NOT NULL,
            principal_id TEXT NOT NULL REFERENCES principals(principal_id),
            workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
            issued_role TEXT NOT NULL CHECK(issued_role IN ('reader','editor','owner')),
            secret_hash BLOB NOT NULL CHECK(typeof(secret_hash)='blob' AND length(secret_hash)=32),
            issued_at INTEGER NOT NULL CHECK(typeof(issued_at)='integer' AND issued_at>=0),
            expires_at INTEGER NOT NULL CHECK(typeof(expires_at)='integer' AND expires_at>issued_at),
            consumed_at INTEGER CHECK(consumed_at IS NULL OR (typeof(consumed_at)='integer' AND consumed_at>=issued_at AND consumed_at<expires_at)));",
    )
    .execute(&mut *connection)
    .await
    .map_err(db_error)?;
    sqlx::query("INSERT INTO principals VALUES(?,?,'owner')")
        .bind(Uuid::new_v4().to_string())
        .bind(workspace_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(db_error)?;
    sqlx::raw_sql("PRAGMA user_version=2")
        .execute(connection)
        .await
        .map_err(db_error)?;
    Ok(())
}

fn require_type(row: &SqliteRow, column: &str, expected: &str) -> Result<(), Error> {
    if row
        .try_get_raw(column)
        .map_err(db_error)?
        .type_info()
        .name()
        != expected
    {
        return Err(Error::InvalidFormat);
    }
    Ok(())
}

fn stored_uuid(row: &SqliteRow, column: &str) -> Result<Uuid, Error> {
    require_type(row, column, "TEXT")?;
    let text: String = row.try_get(column).map_err(db_error)?;
    let id: Uuid = parse(&text)?;
    if id.get_version_num() != 4
        || id.get_variant() != uuid::Variant::RFC4122
        || id.to_string() != text
    {
        return Err(Error::InvalidFormat);
    }
    Ok(id)
}

fn stored_time(row: &SqliteRow, column: &str) -> Result<i64, Error> {
    require_type(row, column, "INTEGER")?;
    let value: i64 = row.try_get(column).map_err(db_error)?;
    if value < 0 {
        return Err(Error::InvalidFormat);
    }
    Ok(value)
}

fn optional_time(row: &SqliteRow, column: &str) -> Result<Option<i64>, Error> {
    if row.try_get_raw(column).map_err(db_error)?.is_null() {
        return Ok(None);
    }
    stored_time(row, column).map(Some)
}

fn stored_hash(row: &SqliteRow, column: &str) -> Result<[u8; 32], Error> {
    require_type(row, column, "BLOB")?;
    row.try_get::<Vec<u8>, _>(column)
        .map_err(db_error)?
        .try_into()
        .map_err(|_| Error::InvalidFormat)
}

fn stored_role(row: &SqliteRow, column: &str) -> Result<WorkspaceRole, Error> {
    require_type(row, column, "TEXT")?;
    match row.try_get::<&str, _>(column).map_err(db_error)? {
        "reader" => Ok(WorkspaceRole::Reader),
        "editor" => Ok(WorkspaceRole::Editor),
        "owner" => Ok(WorkspaceRole::Owner),
        _ => Err(Error::InvalidFormat),
    }
}

pub(super) async fn validate(
    connection: &mut SqliteConnection,
    workspace_id: WorkspaceId,
) -> Result<(), Error> {
    let principals = sqlx::query("SELECT * FROM principals")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    let mut owners = 0;
    for row in principals {
        stored_uuid(&row, "principal_id")?;
        if stored_uuid(&row, "workspace_id")?.to_string() != workspace_id.to_string() {
            return Err(Error::InvalidFormat);
        }
        if stored_role(&row, "workspace_role")? == WorkspaceRole::Owner {
            owners += 1;
        }
    }
    if owners != 1 {
        return Err(Error::InvalidFormat);
    }
    let devices = sqlx::query("SELECT * FROM devices")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    for row in devices {
        for column in ["device_id", "principal_id", "stream_id", "token_id"] {
            stored_uuid(&row, column)?;
        }
        stored_hash(&row, "token_hash")?;
        let issued = stored_time(&row, "issued_at")?;
        let idle = stored_time(&row, "idle_expires_at")?;
        let absolute = stored_time(&row, "absolute_expires_at")?;
        let revoked = optional_time(&row, "revoked_at")?;
        if idle <= issued || absolute < idle || revoked.is_some_and(|at| at < issued) {
            return Err(Error::InvalidFormat);
        }
    }
    let pairings = sqlx::query("SELECT * FROM pairing_secrets")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    for row in pairings {
        stored_uuid(&row, "pairing_id")?;
        stored_uuid(&row, "principal_id")?;
        if stored_uuid(&row, "workspace_id")?.to_string() != workspace_id.to_string() {
            return Err(Error::InvalidFormat);
        }
        stored_role(&row, "issued_role")?;
        stored_hash(&row, "secret_hash")?;
        let issued = stored_time(&row, "issued_at")?;
        let expires = stored_time(&row, "expires_at")?;
        let consumed = optional_time(&row, "consumed_at")?;
        if expires <= issued || consumed.is_some_and(|at| at < issued || at >= expires) {
            return Err(Error::InvalidFormat);
        }
    }
    Ok(())
}

pub(crate) struct PairingRecord {
    pub pairing_id: Uuid,
    pub principal_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub issued_role: WorkspaceRole,
    pub secret_hash: [u8; 32],
    pub issued_at: i64,
    pub expires_at: i64,
    pub consumed_at: Option<i64>,
}

pub(crate) struct DeviceRecord {
    pub identity: AuthIdentity,
    pub token_id: Uuid,
    pub token_hash: [u8; 32],
    pub issued_at: i64,
    pub idle_expires_at: i64,
    pub absolute_expires_at: i64,
    pub revoked_at: Option<i64>,
}

fn auth_error(error: Error) -> AuthError {
    if error == Error::NoSpace {
        AuthError::NoSpace
    } else {
        AuthError::Storage
    }
}
fn auth_db_error(error: sqlx::Error) -> AuthError {
    auth_error(db_error(error))
}

fn role_name(role: WorkspaceRole) -> &'static str {
    match role {
        WorkspaceRole::Reader => "reader",
        WorkspaceRole::Editor => "editor",
        WorkspaceRole::Owner => "owner",
    }
}

impl State {
    pub async fn owner_principal(&mut self) -> Result<Uuid, AuthError> {
        let row = sqlx::query(
            "SELECT principal_id FROM principals WHERE workspace_id=? AND workspace_role='owner'",
        )
        .bind(self.workspace_id.to_string())
        .fetch_one(&mut self.connection)
        .await
        .map_err(|_| AuthError::Storage)?;
        stored_uuid(&row, "principal_id").map_err(|_| AuthError::Storage)
    }

    pub async fn principal_role(&mut self, id: Uuid) -> Result<Option<WorkspaceRole>, AuthError> {
        sqlx::query("SELECT workspace_role FROM principals WHERE principal_id=? AND workspace_id=?")
            .bind(id.to_string())
            .bind(self.workspace_id.to_string())
            .fetch_optional(&mut self.connection)
            .await
            .map_err(auth_db_error)?
            .map(|row| stored_role(&row, "workspace_role").map_err(auth_error))
            .transpose()
    }

    pub async fn insert_principal(
        &mut self,
        id: Uuid,
        role: WorkspaceRole,
    ) -> Result<(), AuthError> {
        sqlx::query("INSERT INTO principals VALUES(?,?,?)")
            .bind(id.to_string())
            .bind(self.workspace_id.to_string())
            .bind(role_name(role))
            .execute(&mut self.connection)
            .await
            .map_err(auth_db_error)?;
        Ok(())
    }

    pub async fn update_role(&mut self, id: Uuid, role: WorkspaceRole) -> Result<(), AuthError> {
        let changed = sqlx::query("UPDATE principals SET workspace_role=? WHERE principal_id=? AND workspace_id=? AND workspace_role!='owner'")
            .bind(role_name(role)).bind(id.to_string()).bind(self.workspace_id.to_string())
            .execute(&mut self.connection).await.map_err(auth_db_error)?.rows_affected();
        if changed != 1 {
            return Err(AuthError::Forbidden);
        }
        Ok(())
    }

    pub async fn transfer_owner(
        &mut self,
        old_owner: Uuid,
        new_owner: Uuid,
        previous_role: WorkspaceRole,
    ) -> Result<(), AuthError> {
        let mut tx = self.connection.begin().await.map_err(auth_db_error)?;
        let changed = sqlx::query("UPDATE principals SET workspace_role=? WHERE principal_id=? AND workspace_id=? AND workspace_role='owner'")
            .bind(role_name(previous_role)).bind(old_owner.to_string()).bind(self.workspace_id.to_string())
            .execute(&mut *tx).await.map_err(auth_db_error)?.rows_affected();
        if changed != 1 {
            return Err(AuthError::Storage);
        }
        let changed = sqlx::query("UPDATE principals SET workspace_role='owner' WHERE principal_id=? AND workspace_id=? AND workspace_role!='owner'")
            .bind(new_owner.to_string()).bind(self.workspace_id.to_string())
            .execute(&mut *tx).await.map_err(auth_db_error)?.rows_affected();
        if changed != 1 {
            return Err(AuthError::Storage);
        }
        tx.commit().await.map_err(auth_db_error)
    }

    pub async fn insert_pairing(&mut self, record: &PairingRecord) -> Result<(), AuthError> {
        let changed = sqlx::query("INSERT INTO pairing_secrets(pairing_id,principal_id,workspace_id,issued_role,secret_hash,issued_at,expires_at)
            SELECT ?,principal_id,workspace_id,workspace_role,?,?,? FROM principals WHERE principal_id=? AND workspace_id=? AND workspace_role=?")
            .bind(record.pairing_id.to_string()).bind(record.secret_hash.as_slice())
            .bind(record.issued_at).bind(record.expires_at).bind(record.principal_id.to_string())
            .bind(self.workspace_id.to_string()).bind(role_name(record.issued_role))
            .execute(&mut self.connection).await.map_err(auth_db_error)?.rows_affected();
        if changed != 1 {
            return Err(AuthError::NotFound);
        }
        Ok(())
    }

    pub async fn pairing(&mut self, id: Uuid) -> Result<Option<PairingRecord>, AuthError> {
        let row =
            sqlx::query("SELECT * FROM pairing_secrets WHERE pairing_id=? AND workspace_id=?")
                .bind(id.to_string())
                .bind(self.workspace_id.to_string())
                .fetch_optional(&mut self.connection)
                .await
                .map_err(auth_db_error)?;
        row.map(|row| {
            Ok::<_, Error>(PairingRecord {
                pairing_id: stored_uuid(&row, "pairing_id")?,
                principal_id: stored_uuid(&row, "principal_id")?,
                workspace_id: parse(row.try_get("workspace_id").map_err(db_error)?)?,
                issued_role: stored_role(&row, "issued_role")?,
                secret_hash: stored_hash(&row, "secret_hash")?,
                issued_at: stored_time(&row, "issued_at")?,
                expires_at: stored_time(&row, "expires_at")?,
                consumed_at: optional_time(&row, "consumed_at")?,
            })
        })
        .transpose()
        .map_err(auth_error)
    }

    pub async fn consume_pairing(
        &mut self,
        pairing: &PairingRecord,
        device: &DeviceRecord,
    ) -> Result<(), AuthError> {
        let mut tx = self.connection.begin().await.map_err(auth_db_error)?;
        let changed = sqlx::query("UPDATE pairing_secrets SET consumed_at=? WHERE pairing_id=? AND secret_hash=?
            AND principal_id=? AND workspace_id=? AND issued_role=? AND consumed_at IS NULL AND issued_at<=? AND expires_at>?
            AND EXISTS(SELECT 1 FROM principals WHERE principal_id=pairing_secrets.principal_id AND workspace_id=pairing_secrets.workspace_id AND workspace_role=pairing_secrets.issued_role)")
            .bind(device.issued_at).bind(pairing.pairing_id.to_string()).bind(pairing.secret_hash.as_slice())
            .bind(pairing.principal_id.to_string()).bind(self.workspace_id.to_string()).bind(role_name(pairing.issued_role))
            .bind(device.issued_at).bind(device.issued_at)
            .execute(&mut *tx).await.map_err(auth_db_error)?.rows_affected();
        if changed != 1 {
            return Err(AuthError::Unauthorized);
        }
        sqlx::query("INSERT INTO operation_streams(stream_id,last_seq) VALUES(?,'0')")
            .bind(device.identity.stream_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(auth_db_error)?;
        sqlx::query("INSERT INTO devices(device_id,principal_id,stream_id,token_id,token_hash,issued_at,idle_expires_at,absolute_expires_at)
            VALUES(?,?,?,?,?,?,?,?)")
            .bind(device.identity.device_id.to_string()).bind(device.identity.principal_id.to_string())
            .bind(device.identity.stream_id.to_string()).bind(device.token_id.to_string()).bind(device.token_hash.as_slice())
            .bind(device.issued_at).bind(device.idle_expires_at).bind(device.absolute_expires_at)
            .execute(&mut *tx).await.map_err(auth_db_error)?;
        tx.commit().await.map_err(auth_db_error)
    }

    pub async fn device(&mut self, token_id: Uuid) -> Result<Option<DeviceRecord>, AuthError> {
        let row = sqlx::query("SELECT d.*,p.workspace_role FROM devices d JOIN principals p USING(principal_id) WHERE d.token_id=? AND p.workspace_id=?")
            .bind(token_id.to_string()).bind(self.workspace_id.to_string())
            .fetch_optional(&mut self.connection).await.map_err(auth_db_error)?;
        row.map(|row| {
            Ok::<_, Error>(DeviceRecord {
                identity: AuthIdentity {
                    principal_id: stored_uuid(&row, "principal_id")?,
                    device_id: stored_uuid(&row, "device_id")?,
                    stream_id: stored_uuid(&row, "stream_id")?,
                    role: stored_role(&row, "workspace_role")?,
                },
                token_id: stored_uuid(&row, "token_id")?,
                token_hash: stored_hash(&row, "token_hash")?,
                issued_at: stored_time(&row, "issued_at")?,
                idle_expires_at: stored_time(&row, "idle_expires_at")?,
                absolute_expires_at: stored_time(&row, "absolute_expires_at")?,
                revoked_at: optional_time(&row, "revoked_at")?,
            })
        })
        .transpose()
        .map_err(auth_error)
    }

    pub async fn revoke_device(&mut self, device: Uuid, now: i64) -> Result<(), AuthError> {
        let row = sqlx::query("SELECT d.issued_at FROM devices d JOIN principals p USING(principal_id) WHERE d.device_id=? AND p.workspace_id=?")
            .bind(device.to_string()).bind(self.workspace_id.to_string())
            .fetch_optional(&mut self.connection).await.map_err(auth_db_error)?
            .ok_or(AuthError::NotFound)?;
        if now < stored_time(&row, "issued_at").map_err(auth_error)? {
            return Err(AuthError::InvalidRequest);
        }
        sqlx::query("UPDATE devices SET revoked_at=COALESCE(revoked_at,?) WHERE device_id=?")
            .bind(now)
            .bind(device.to_string())
            .execute(&mut self.connection)
            .await
            .map_err(auth_db_error)?;
        Ok(())
    }
}
