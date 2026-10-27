use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

mod outbox;
mod recovery;
mod registration;
pub use outbox::{
    OutboxAbandonState, OutboxAbandoned, OutboxCommit, OutboxConflict, OutboxDocumentState,
    OutboxFailure, OutboxFailureKind, OutboxOperation, OutboxPage, OutboxStatus, OutboxSummary,
};
pub use recovery::{
    RecoveryDraftInfo, RecoveryDraftPage, RecoveryOperationInfo, RecoveryOperationPage,
    RecoveryStore,
};
pub use registration::{RegisteredDraftInfo, RegistrationBinding};

pub const MAX_DOCUMENT_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Storage,
    ExportExists,
    StoreLocked,
    StorePathChanged,
    UnsupportedFormat,
    InvalidDocument,
    DocumentTooLarge,
    SessionAlreadyOpen,
    StaleSession,
    StaleGeneration,
    GenerationMismatch,
    InvalidGeneration,
    InvalidRegistration,
    StaleRegistration,
    OpenEditSessions,
    MissingDraft,
    InvalidReceipt,
    NoPendingChanges,
    OutboxBlocked,
    InvalidRequest,
    ReconciliationRequired,
    PayloadRetired,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn detects_database_path_replaced_after_inode_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drafts.db");
        std::fs::write(&path, b"original").unwrap();
        let lock = File::open(&path).unwrap();
        lock.try_lock().unwrap();
        assert!(verify_locked_path(&lock, &path).is_ok());
        std::fs::rename(&path, directory.path().join("previous.db")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert_eq!(
            verify_locked_path(&lock, &path),
            Err(StoreError::StorePathChanged)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }

    #[tokio::test]
    async fn writer_connection_uses_full_durability_and_foreign_keys() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = ClientStore::open(directory.path().join("drafts.db"))
            .await
            .unwrap();
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&mut store.connection)
            .await
            .unwrap();
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut store.connection)
            .await
            .unwrap();
        assert_eq!(synchronous, 2);
        assert_eq!(foreign_keys, 1);
        store.close().await.unwrap();
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for StoreError {}

#[derive(Debug, PartialEq, Eq)]
pub struct OpenedDocument {
    pub document_id: String,
    pub edit_session_id: String,
    pub generation: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SavedCheckpoint {
    pub edit_session_id: String,
    pub generation: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PersistedDraft {
    pub generation: u64,
    pub bytes: Vec<u8>,
    pub base_bytes: Vec<u8>,
    pub base_revision: String,
}

const APPLICATION_ID: i64 = 0x4e545258;
const SCHEMA_VERSION: i64 = 5;

struct EditSession {
    id: String,
    base: Vec<u8>,
    base_revision: String,
}

pub struct ClientStore {
    connection: SqliteConnection,
    sessions: HashMap<String, EditSession>,
    _lock: File,
}

fn storage_error(_: impl std::fmt::Debug) -> StoreError {
    StoreError::Storage
}

fn verify_locked_path(lock: &File, path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let locked = lock.metadata().map_err(storage_error)?;
        let current = std::fs::metadata(path).map_err(storage_error)?;
        if locked.dev() != current.dev() || locked.ino() != current.ino() {
            return Err(StoreError::StorePathChanged);
        }
    }
    #[cfg(not(unix))]
    let _ = (lock, path);
    Ok(())
}

fn validate_document(document_id: &str, bytes: &[u8], revision: &str) -> Result<(), StoreError> {
    if document_id.is_empty()
        || document_id.len() > 128
        || !document_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        || revision.is_empty()
        || revision.len() > 256
        || revision.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidDocument);
    }
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(StoreError::DocumentTooLarge);
    }
    Ok(())
}

fn decode_draft(
    row: &sqlx::sqlite::SqliteRow,
    document_id: &str,
) -> Result<PersistedDraft, StoreError> {
    let format_version: i64 = row.try_get("format_version").map_err(storage_error)?;
    let generation: i64 = row.try_get("generation").map_err(storage_error)?;
    if format_version != 1 || generation <= 0 {
        return Err(StoreError::UnsupportedFormat);
    }
    let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
    let base_bytes: Vec<u8> = row.try_get("base_bytes").map_err(storage_error)?;
    let base_revision: String = row.try_get("revision").map_err(storage_error)?;
    validate_document(document_id, &bytes, &base_revision)?;
    validate_document(document_id, &base_bytes, &base_revision)?;
    Ok(PersistedDraft {
        generation: generation as u64,
        bytes,
        base_bytes,
        base_revision,
    })
}

impl ClientStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let mut file_options = OpenOptions::new();
        file_options
            .read(true)
            .write(true)
            .create_new(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(0o600);
        }
        let (lock, initialize) = match file_options.open(path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                file_options.create_new(false);
                (file_options.open(path).map_err(storage_error)?, false)
            }
            Err(error) => return Err(storage_error(error)),
        };
        // Lock the database inode so path aliases cannot create a second writer.
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::StoreLocked),
            Err(TryLockError::Error(error)) => return Err(storage_error(error)),
        }
        verify_locked_path(&lock, path)?;

        let existing_version = if !initialize {
            // An empty existing file may be a damaged store, not a fresh installation.
            if lock.metadata().map_err(storage_error)?.len() == 0 {
                return Err(StoreError::UnsupportedFormat);
            }
            // Inspect unknown formats without changing their journal mode or contents.
            let mut probe = SqliteConnection::connect_with(
                &SqliteConnectOptions::new().filename(path).read_only(true),
            )
            .await
            .map_err(storage_error)?;
            verify_locked_path(&lock, path)?;
            let version: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&mut probe)
                .await
                .map_err(storage_error)?;
            let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
                .fetch_one(&mut probe)
                .await
                .map_err(storage_error)?;
            probe.close().await.map_err(storage_error)?;
            if !matches!(version, 1 | 2 | 3 | 4 | SCHEMA_VERSION)
                || application_id != APPLICATION_ID
            {
                return Err(StoreError::UnsupportedFormat);
            }
            version
        } else {
            0
        };

        let options = SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(2));
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .map_err(storage_error)?;
        verify_locked_path(&lock, path)?;
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode=WAL")
            .fetch_one(&mut connection)
            .await
            .map_err(storage_error)?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Storage);
        }
        sqlx::query("PRAGMA synchronous=FULL")
            .execute(&mut connection)
            .await
            .map_err(storage_error)?;
        if initialize {
            let mut transaction = connection.begin().await.map_err(storage_error)?;
            sqlx::raw_sql(
                "CREATE TABLE draft_checkpoints (
                    document_id TEXT PRIMARY KEY NOT NULL,
                    generation INTEGER NOT NULL CHECK (generation > 0),
                    format_version INTEGER NOT NULL CHECK (format_version = 1),
                    bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880)
                );
                CREATE TABLE draft_bases (
                    document_id TEXT PRIMARY KEY NOT NULL REFERENCES draft_checkpoints(document_id),
                    bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
                    revision TEXT NOT NULL
                );
                PRAGMA user_version = 1;
                PRAGMA application_id = 1314148952;",
            )
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
            transaction.commit().await.map_err(storage_error)?;
        }
        if initialize
            || existing_version == 1
            || existing_version == 2
            || existing_version == 3
            || existing_version == 4
        {
            let mut transaction = connection.begin().await.map_err(storage_error)?;
            if initialize || existing_version == 1 {
                sqlx::raw_sql(registration::CREATE_SCHEMA)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version == 1 || existing_version == 2 {
                sqlx::raw_sql(outbox::CREATE_SCHEMA)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 3 {
                sqlx::raw_sql(outbox::UPGRADE_V4)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            sqlx::raw_sql(outbox::UPGRADE_V5)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
            transaction.commit().await.map_err(storage_error)?;
        }
        Ok(Self {
            connection,
            sessions: HashMap::new(),
            _lock: lock,
        })
    }

    pub async fn open_document(
        &mut self,
        document_id: &str,
        base: &[u8],
        base_revision: &str,
    ) -> Result<OpenedDocument, StoreError> {
        validate_document(document_id, base, base_revision)?;
        if self.sessions.contains_key(document_id) {
            return Err(StoreError::SessionAlreadyOpen);
        }
        let stored = self.read_checkpoint(document_id).await?;
        let (generation, bytes, base, base_revision) = match stored {
            Some(draft) => (
                draft.generation,
                draft.bytes,
                draft.base_bytes,
                draft.base_revision,
            ),
            None => (0, base.to_vec(), base.to_vec(), base_revision.to_owned()),
        };
        Ok(self.install_edit_session(document_id, generation, bytes, base, base_revision))
    }

    fn install_edit_session(
        &mut self,
        document_id: &str,
        generation: u64,
        bytes: Vec<u8>,
        base: Vec<u8>,
        base_revision: String,
    ) -> OpenedDocument {
        let edit_session_id = uuid::Uuid::new_v4().to_string();
        self.sessions.insert(
            document_id.to_owned(),
            EditSession {
                id: edit_session_id.clone(),
                base,
                base_revision,
            },
        );
        OpenedDocument {
            document_id: document_id.to_owned(),
            edit_session_id,
            generation,
            bytes,
        }
    }

    pub async fn save_checkpoint(
        &mut self,
        document_id: &str,
        edit_session_id: &str,
        generation: u64,
        bytes: &[u8],
    ) -> Result<SavedCheckpoint, StoreError> {
        let session = self
            .sessions
            .get(document_id)
            .ok_or(StoreError::StaleSession)?;
        if session.id != edit_session_id {
            return Err(StoreError::StaleSession);
        }
        validate_document(document_id, bytes, &session.base_revision)?;
        let generation_i64 =
            i64::try_from(generation).map_err(|_| StoreError::InvalidGeneration)?;
        if generation_i64 == 0 {
            return Err(StoreError::InvalidGeneration);
        }
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        if let Some(current) = sqlx::query(
            "SELECT generation, format_version FROM draft_checkpoints WHERE document_id = ?",
        )
        .bind(document_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        {
            let durable_generation: i64 = current.try_get("generation").map_err(storage_error)?;
            let format_version: i64 = current.try_get("format_version").map_err(storage_error)?;
            if format_version != 1 || durable_generation <= 0 {
                return Err(StoreError::UnsupportedFormat);
            }
            if generation_i64 < durable_generation {
                return Err(StoreError::StaleGeneration);
            }
            if generation_i64 == durable_generation {
                let durable_bytes: Vec<u8> =
                    sqlx::query_scalar("SELECT bytes FROM draft_checkpoints WHERE document_id = ?")
                        .bind(document_id)
                        .fetch_one(&mut *transaction)
                        .await
                        .map_err(storage_error)?;
                transaction.rollback().await.map_err(storage_error)?;
                return if durable_bytes == bytes {
                    Ok(SavedCheckpoint {
                        edit_session_id: edit_session_id.to_owned(),
                        generation,
                    })
                } else {
                    Err(StoreError::GenerationMismatch)
                };
            }
        }
        sqlx::query("INSERT INTO draft_checkpoints (document_id, generation, format_version, bytes) VALUES (?, ?, 1, ?) ON CONFLICT(document_id) DO UPDATE SET generation=excluded.generation, bytes=excluded.bytes")
            .bind(document_id).bind(generation_i64).bind(bytes)
            .execute(&mut *transaction).await.map_err(storage_error)?;
        let base_exists: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM draft_bases WHERE document_id = ?)")
                .bind(document_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        if base_exists == 0 {
            sqlx::query("INSERT INTO draft_bases (document_id, bytes, revision) VALUES (?, ?, ?) ON CONFLICT(document_id) DO NOTHING")
                .bind(document_id).bind(&session.base).bind(&session.base_revision)
                .execute(&mut *transaction).await.map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(SavedCheckpoint {
            edit_session_id: edit_session_id.to_owned(),
            generation,
        })
    }

    /// Invalidate in-memory owners when the renderer is replaced; durable drafts remain untouched.
    pub fn reset_edit_sessions(&mut self) {
        self.sessions.clear();
    }

    /// Call only after the editor's final pending generation has committed.
    pub fn close_document(
        &mut self,
        document_id: &str,
        edit_session_id: &str,
    ) -> Result<(), StoreError> {
        let session = self
            .sessions
            .get(document_id)
            .ok_or(StoreError::StaleSession)?;
        if session.id != edit_session_id {
            return Err(StoreError::StaleSession);
        }
        self.sessions.remove(document_id);
        Ok(())
    }

    pub async fn read_checkpoint(
        &mut self,
        document_id: &str,
    ) -> Result<Option<PersistedDraft>, StoreError> {
        let row = sqlx::query("SELECT c.generation, c.format_version, c.bytes, b.bytes AS base_bytes, b.revision FROM draft_checkpoints c LEFT JOIN draft_bases b USING(document_id) WHERE c.document_id = ?")
            .bind(document_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        row.as_ref()
            .map(|row| decode_draft(row, document_id))
            .transpose()
    }

    pub async fn close(self) -> Result<(), StoreError> {
        self.connection.close().await.map_err(storage_error)
    }
}
