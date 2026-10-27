use super::{
    APPLICATION_ID, PersistedDraft, RegistrationBinding, StoreError, decode_draft, registration,
    storage_error, validate_document, verify_locked_path,
};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

const RECOVERY_PAGE_SIZE: usize = 100;

#[derive(Debug, PartialEq, Eq)]
pub struct RecoveryDraftInfo {
    pub storage_id: String,
    pub document_id: String,
    pub registration: Option<RegistrationBinding>,
    pub registration_status: Option<String>,
    pub generation: Option<u64>,
    pub has_checkpoint: bool,
    pub has_base: bool,
    pub metadata_error: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RecoveryOperationInfo {
    pub operation_id: String,
    pub document_id: String,
    pub registration: Option<RegistrationBinding>,
    pub sequence: Option<u64>,
    pub generation: Option<u64>,
    pub status: String,
    pub terminal_kind: Option<String>,
    pub registration_status: Option<String>,
    pub metadata_error: bool,
}

pub struct RecoveryDraftPage {
    pub items: Vec<RecoveryDraftInfo>,
    pub next_after_id: Option<String>,
}

pub struct RecoveryOperationPage {
    pub items: Vec<RecoveryOperationInfo>,
    pub next_after_id: Option<String>,
}

pub struct RecoveryStore {
    connection: SqliteConnection,
    schema_version: i64,
    _lock: File,
}

fn valid_cursor(after: Option<&str>) -> Result<&str, StoreError> {
    let value = after.unwrap_or("");
    if value.len() > 128 || value.chars().any(char::is_control) {
        return Err(StoreError::InvalidRequest);
    }
    Ok(value)
}

impl RecoveryStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let lock = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(storage_error)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::StoreLocked),
            Err(TryLockError::Error(error)) => return Err(storage_error(error)),
        }
        verify_locked_path(&lock, path)?;
        if !lock.metadata().map_err(storage_error)?.is_file() {
            return Err(StoreError::UnsupportedFormat);
        }
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(path)
                .read_only(true)
                .busy_timeout(Duration::from_secs(2)),
        )
        .await
        .map_err(storage_error)?;
        verify_locked_path(&lock, path)?;
        sqlx::query("PRAGMA trusted_schema=OFF")
            .execute(&mut connection)
            .await
            .map_err(storage_error)?;
        let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
            .fetch_one(&mut connection)
            .await
            .map_err(storage_error)?;
        if application_id != APPLICATION_ID {
            return Err(StoreError::UnsupportedFormat);
        }
        let schema_version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await
            .map_err(storage_error)?;
        Ok(Self {
            connection,
            schema_version,
            _lock: lock,
        })
    }

    fn known_schema(&self) -> Result<(), StoreError> {
        if !(1..=super::SCHEMA_VERSION).contains(&self.schema_version) {
            return Err(StoreError::UnsupportedFormat);
        }
        Ok(())
    }

    pub async fn drafts(&mut self, after: Option<&str>) -> Result<RecoveryDraftPage, StoreError> {
        self.known_schema()?;
        let after = valid_cursor(after)?;
        let query = if self.schema_version == 1 {
            "SELECT s.document_id AS storage_id, s.document_id AS document_id, c.generation, c.document_id IS NOT NULL AS has_checkpoint, b.document_id IS NOT NULL AS has_base FROM (SELECT document_id FROM draft_checkpoints UNION SELECT document_id FROM draft_bases) s LEFT JOIN draft_checkpoints c ON c.document_id=s.document_id LEFT JOIN draft_bases b ON b.document_id=s.document_id WHERE s.document_id>? ORDER BY s.document_id LIMIT 101"
        } else {
            "SELECT s.document_id AS storage_id, COALESCE(d.document_id, s.document_id) AS document_id, c.generation, c.document_id IS NOT NULL AS has_checkpoint, b.document_id IS NOT NULL AS has_base, d.registration_id, r.status AS registration_status, r.server_origin, r.workspace_id, r.workspace_epoch, r.device_id, r.device_stream_id FROM (SELECT document_id FROM draft_checkpoints UNION SELECT document_id FROM draft_bases) s LEFT JOIN draft_checkpoints c ON c.document_id=s.document_id LEFT JOIN draft_bases b ON b.document_id=s.document_id LEFT JOIN registered_documents d ON d.storage_id=s.document_id LEFT JOIN client_registrations r ON r.id=d.registration_id WHERE s.document_id>? ORDER BY s.document_id LIMIT 101"
        };
        let mut rows = sqlx::query(query)
            .bind(after)
            .fetch_all(&mut self.connection)
            .await
            .map_err(storage_error)?;
        let has_more = rows.len() > RECOVERY_PAGE_SIZE;
        if has_more {
            rows.pop();
        }
        let mut drafts = Vec::with_capacity(rows.len());
        for row in rows {
            let mut metadata_error = false;
            let has_checkpoint: bool = row.try_get("has_checkpoint").map_err(storage_error)?;
            let has_base: bool = row.try_get("has_base").map_err(storage_error)?;
            let generation = match row.try_get::<Option<i64>, _>("generation") {
                Ok(Some(value)) if value > 0 => Some(value as u64),
                Ok(None) if !has_checkpoint => None,
                _ => {
                    metadata_error = true;
                    None
                }
            };
            let registration = if self.schema_version > 1 {
                let registered: Option<i64> =
                    row.try_get("registration_id").map_err(storage_error)?;
                if registered.is_some() {
                    match registration::binding_from_row(&row) {
                        Ok(binding) => Some(binding),
                        Err(_) => {
                            metadata_error = true;
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let registration_status = if registration.is_some() {
                match row.try_get::<String, _>("registration_status") {
                    Ok(status) if matches!(status.as_str(), "active" | "frozen") => Some(status),
                    _ => {
                        metadata_error = true;
                        None
                    }
                }
            } else {
                None
            };
            let storage_id: String = row.try_get("storage_id").map_err(storage_error)?;
            let mut document_id: String = row.try_get("document_id").map_err(storage_error)?;
            if storage_id.len() > 128 || storage_id.is_empty() {
                return Err(StoreError::UnsupportedFormat);
            }
            if document_id.len() > 128 || document_id.is_empty() {
                document_id = "확인 불가".into();
                metadata_error = true;
            }
            drafts.push(RecoveryDraftInfo {
                storage_id,
                document_id,
                registration,
                registration_status,
                generation,
                has_checkpoint,
                has_base,
                metadata_error,
            });
        }
        let next_after_id = if has_more {
            drafts.last().map(|draft| draft.storage_id.clone())
        } else {
            None
        };
        Ok(RecoveryDraftPage {
            items: drafts,
            next_after_id,
        })
    }

    pub async fn draft(&mut self, storage_id: &str) -> Result<PersistedDraft, StoreError> {
        self.known_schema()?;
        let row = sqlx::query("SELECT c.format_version, c.generation, c.bytes, b.bytes AS base_bytes, b.revision FROM draft_checkpoints c JOIN draft_bases b ON b.document_id=c.document_id WHERE c.document_id=? AND length(c.bytes)<=5242880 AND length(b.bytes)<=5242880")
            .bind(storage_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?
            .ok_or(StoreError::MissingDraft)?;
        decode_draft(&row, storage_id)
    }

    pub async fn checkpoint_bytes(&mut self, storage_id: &str) -> Result<Vec<u8>, StoreError> {
        self.known_schema()?;
        let row = sqlx::query("SELECT format_version, bytes FROM draft_checkpoints WHERE document_id=? AND length(bytes)<=5242880")
            .bind(storage_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?
            .ok_or(StoreError::MissingDraft)?;
        let version: i64 = row.try_get("format_version").map_err(storage_error)?;
        let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
        if version != 1 {
            return Err(StoreError::UnsupportedFormat);
        }
        validate_document(storage_id, &bytes, "recovery")?;
        Ok(bytes)
    }

    pub async fn base_bytes(&mut self, storage_id: &str) -> Result<Vec<u8>, StoreError> {
        self.known_schema()?;
        let row = sqlx::query(
            "SELECT bytes FROM draft_bases WHERE document_id=? AND length(bytes)<=5242880",
        )
        .bind(storage_id)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::MissingDraft)?;
        let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
        validate_document(storage_id, &bytes, "recovery")?;
        Ok(bytes)
    }

    pub async fn operations(
        &mut self,
        after: Option<&str>,
    ) -> Result<RecoveryOperationPage, StoreError> {
        self.known_schema()?;
        let after = valid_cursor(after)?;
        if self.schema_version < 3 {
            return Ok(RecoveryOperationPage {
                items: Vec::new(),
                next_after_id: None,
            });
        }
        let query = if self.schema_version >= 5 {
            "SELECT o.operation_id, o.document_id, o.operation_seq, o.generation, o.status, o.terminal_kind, r.status AS registration_status, r.server_origin, r.workspace_id, r.workspace_epoch, r.device_id, r.device_stream_id FROM client_outbox o LEFT JOIN client_registrations r ON r.id=o.registration_id WHERE o.payload_retired=0 AND o.operation_id>? ORDER BY o.operation_id LIMIT 101"
        } else if self.schema_version == 4 {
            "SELECT o.operation_id, o.document_id, o.operation_seq, o.generation, o.status, o.terminal_kind, r.status AS registration_status, r.server_origin, r.workspace_id, r.workspace_epoch, r.device_id, r.device_stream_id FROM client_outbox o LEFT JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id>? ORDER BY o.operation_id LIMIT 101"
        } else {
            "SELECT o.operation_id, o.document_id, o.operation_seq, o.generation, o.status, NULL AS terminal_kind, r.status AS registration_status, r.server_origin, r.workspace_id, r.workspace_epoch, r.device_id, r.device_stream_id FROM client_outbox o LEFT JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id>? ORDER BY o.operation_id LIMIT 101"
        };
        let mut rows = sqlx::query(query)
            .bind(after)
            .fetch_all(&mut self.connection)
            .await
            .map_err(storage_error)?;
        let has_more = rows.len() > RECOVERY_PAGE_SIZE;
        if has_more {
            rows.pop();
        }
        let mut operations = Vec::with_capacity(rows.len());
        for row in rows {
            let mut metadata_error = false;
            let sequence = match row.try_get::<i64, _>("operation_seq") {
                Ok(value) if value > 0 => Some(value as u64),
                _ => {
                    metadata_error = true;
                    None
                }
            };
            let generation = match row.try_get::<i64, _>("generation") {
                Ok(value) if value > 0 => Some(value as u64),
                _ => {
                    metadata_error = true;
                    None
                }
            };
            let operation_id: String = row.try_get("operation_id").map_err(storage_error)?;
            if operation_id.len() > 128 || operation_id.is_empty() {
                return Err(StoreError::UnsupportedFormat);
            }
            let document_id = match row.try_get::<String, _>("document_id") {
                Ok(value) if !value.is_empty() && value.len() <= 128 => value,
                _ => {
                    metadata_error = true;
                    "확인 불가".into()
                }
            };
            let status = match row.try_get::<String, _>("status") {
                Ok(value) if value.len() <= 32 => value,
                _ => {
                    metadata_error = true;
                    "확인 불가".into()
                }
            };
            let terminal_kind = match row.try_get::<Option<String>, _>("terminal_kind") {
                Ok(value) if value.as_ref().is_none_or(|kind| kind.len() <= 32) => value,
                _ => {
                    metadata_error = true;
                    None
                }
            };
            let registration_status = match row.try_get::<String, _>("registration_status") {
                Ok(status) if matches!(status.as_str(), "active" | "frozen") => Some(status),
                _ => {
                    metadata_error = true;
                    None
                }
            };
            let registration = match registration::binding_from_row(&row) {
                Ok(binding) => Some(binding),
                Err(_) => {
                    metadata_error = true;
                    None
                }
            };
            operations.push(RecoveryOperationInfo {
                operation_id,
                document_id,
                registration,
                sequence,
                generation,
                status,
                terminal_kind,
                registration_status,
                metadata_error,
            });
        }
        let next_after_id = if has_more {
            operations
                .last()
                .map(|operation| operation.operation_id.clone())
        } else {
            None
        };
        Ok(RecoveryOperationPage {
            items: operations,
            next_after_id,
        })
    }

    pub async fn operation_bytes(&mut self, operation_id: &str) -> Result<Vec<u8>, StoreError> {
        self.known_schema()?;
        if self.schema_version < 3 {
            return Err(StoreError::MissingDraft);
        }
        let query = if self.schema_version >= 5 {
            "SELECT bytes FROM client_outbox WHERE operation_id=? AND payload_retired=0 AND length(bytes)<=5242880"
        } else {
            "SELECT bytes FROM client_outbox WHERE operation_id=? AND length(bytes)<=5242880"
        };
        sqlx::query_scalar(query)
            .bind(operation_id)
            .fetch_optional(&mut self.connection)
            .await
            .map_err(storage_error)?
            .ok_or(StoreError::MissingDraft)
    }

    pub async fn export_snapshot(
        &mut self,
        destination: impl AsRef<Path>,
    ) -> Result<(), StoreError> {
        let destination = destination.as_ref();
        let parent = destination.parent().ok_or(StoreError::Storage)?;
        let temporary = tempfile::NamedTempFile::new_in(parent).map_err(storage_error)?;
        let mut output =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(temporary.path()))
                .await
                .map_err(storage_error)?;
        let backup_result: Result<(), StoreError> = async {
            let mut source_handle = self.connection.lock_handle().await.map_err(storage_error)?;
            let mut output_handle = output.lock_handle().await.map_err(storage_error)?;
            // Both SQLx workers are locked until backup_finish releases the SQLite handles.
            let (step, finish) = unsafe {
                let backup = libsqlite3_sys::sqlite3_backup_init(
                    output_handle.as_raw_handle().as_ptr(),
                    c"main".as_ptr(),
                    source_handle.as_raw_handle().as_ptr(),
                    c"main".as_ptr(),
                );
                if backup.is_null() {
                    return Err(StoreError::Storage);
                }
                let step = libsqlite3_sys::sqlite3_backup_step(backup, -1);
                let finish = libsqlite3_sys::sqlite3_backup_finish(backup);
                (step, finish)
            };
            if step != libsqlite3_sys::SQLITE_DONE || finish != libsqlite3_sys::SQLITE_OK {
                return Err(StoreError::Storage);
            }
            Ok(())
        }
        .await;
        let close_result = output.close().await.map_err(storage_error);
        backup_result?;
        close_result?;
        temporary.as_file().sync_all().map_err(storage_error)?;
        temporary.persist_noclobber(destination).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                StoreError::ExportExists
            } else {
                StoreError::Storage
            }
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(storage_error)?;
        Ok(())
    }
}
