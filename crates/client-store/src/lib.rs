use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

mod bootstrap;
mod content_cache;
mod content_conflict;
mod credential;
mod deleted_draft_copy;
mod outbox;
mod recovery;
mod registration;
mod search;
pub use content_cache::{CachedContent, RemoteFileObservation};
pub use content_conflict::{
    ContentConflictChoice, ContentConflictChoiceKind, ContentConflictChooseRemoteResult,
    ContentConflictCopyContentState, ContentConflictCopyIntent, ContentConflictIntent,
    ContentConflictIntentRequest, ContentConflictRef, ContentConflictResolutionState,
    ContentConflictSnapshot,
};
pub use credential::CredentialPending;
pub use deleted_draft_copy::{
    DeletedDraftCopyBlockingFollowup, DeletedDraftCopyCreateState, DeletedDraftCopyEvidence,
    DeletedDraftCopyIntent, DeletedDraftCopyRequest, DeletedDraftCopyStatus,
};
pub use outbox::{
    CreateOutboxOperation, CreateOutboxPage, CreateOutboxState, DeleteOutboxOperation,
    DeleteOutboxPage, DeleteOutboxState, FrozenContentApplyApproval,
    FrozenContentCrossEpochConfirmation, FrozenContentDecision, FrozenContentDecisionStatus,
    FrozenContentDismissApproval, FrozenCreateChoice, FrozenCreatePage, FrozenDeleteChoice,
    FrozenDeletePage, FrozenDocumentOperationKind, FrozenDocumentOperationSummary,
    FrozenDocumentReviewPage, FrozenMoveChoice, FrozenMovePage, MoveOutboxOperation,
    MoveOutboxPage, MoveOutboxState, OutboxAbandonState, OutboxAbandoned, OutboxCommit,
    OutboxConflict, OutboxDocumentState, OutboxFailure, OutboxFailureKind, OutboxOperation,
    OutboxPage, OutboxStatus, OutboxSummary, PendingOutboxOperation, RestoreOutboxOperation,
    RestoreOutboxPage, RestoreOutboxState,
};
pub use recovery::{
    RecoveryDraftInfo, RecoveryDraftPage, RecoveryOperationInfo, RecoveryOperationKind,
    RecoveryOperationPage, RecoveryStore,
};
pub use registration::{RegisteredDraftInfo, RegistrationBinding};
pub use search::{
    SearchCandidate, SearchObserver, SearchPage, SearchScope, SearchSource, search_contains,
    search_contains_cancellable,
};

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
    BootstrapHistoryExpired,
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
const SCHEMA_VERSION: i64 = 24;

struct EditSession {
    // A remote refresh preserves the old checkpoint until the next local save.
    refreshed_base: bool,
    id: String,
    base: Vec<u8>,
    base_revision: String,
}

pub struct ClientStore {
    connection: SqliteConnection,
    sessions: HashMap<String, EditSession>,
    _lock: File,
    database_path: std::path::PathBuf,
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
        let database_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_err(storage_error)?.join(path)
        };
        let path = database_path.as_path();
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
            if !matches!(
                version,
                1 | 2
                    | 3
                    | 4
                    | 5
                    | 6
                    | 7
                    | 8
                    | 9
                    | 10
                    | 11
                    | 12
                    | 13
                    | 14
                    | 15
                    | 16
                    | 17
                    | 18
                    | 19
                    | 20
                    | 21
                    | 22
                    | 23
                    | SCHEMA_VERSION
            ) || application_id != APPLICATION_ID
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
            || existing_version == 5
            || existing_version == 6
            || existing_version == 7
            || existing_version == 8
            || existing_version == 9
            || existing_version == 10
            || existing_version == 11
            || existing_version == 12
            || existing_version == 13
            || existing_version == 14
            || existing_version == 15
            || existing_version == 16
            || existing_version == 17
            || existing_version == 18
            || existing_version == 19
            || existing_version == 20
            || existing_version == 21
            || existing_version == 22
            || existing_version == 23
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
            if initialize || existing_version <= 4 {
                sqlx::raw_sql(outbox::UPGRADE_V5)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 5 {
                sqlx::raw_sql(bootstrap::UPGRADE_V6)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 6 {
                sqlx::raw_sql(content_cache::UPGRADE_V7)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 7 {
                sqlx::raw_sql(outbox::UPGRADE_V8)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 8 {
                sqlx::raw_sql(credential::UPGRADE_V9)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 9 {
                sqlx::raw_sql(outbox::UPGRADE_V10)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 10 {
                sqlx::raw_sql(outbox::UPGRADE_V11)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 11 {
                sqlx::raw_sql(outbox::UPGRADE_V12)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 12 {
                sqlx::raw_sql(outbox::UPGRADE_V13)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 13 {
                sqlx::raw_sql(outbox::UPGRADE_V14)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 14 {
                sqlx::raw_sql(outbox::UPGRADE_V15)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 15 {
                sqlx::raw_sql(content_conflict::UPGRADE_V16)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 16 {
                sqlx::raw_sql(content_conflict::UPGRADE_V17)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 17 {
                sqlx::raw_sql(content_conflict::UPGRADE_V18)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 18 {
                sqlx::raw_sql(content_conflict::UPGRADE_V19)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 19 {
                let content_column: Option<(String, i64)> = sqlx::query_as("SELECT type,\"notnull\" FROM pragma_table_info('content_conflict_copies') WHERE name='content_operation_id'")
                .fetch_optional(&mut *transaction).await.map_err(storage_error)?;
                match content_column {
                    None => {
                        sqlx::query("ALTER TABLE content_conflict_copies ADD COLUMN content_operation_id TEXT REFERENCES client_outbox(operation_id)").execute(&mut *transaction).await.map_err(storage_error)?;
                    }
                    Some((kind, required))
                        if kind.eq_ignore_ascii_case("TEXT") && required == 0 => {}
                    Some(_) => return Err(StoreError::UnsupportedFormat),
                }
                let content_fk: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_list('content_conflict_copies') WHERE \"table\"='client_outbox' AND \"from\"='content_operation_id' AND \"to\"='operation_id'")
                .fetch_one(&mut *transaction).await.map_err(storage_error)?;
                if content_fk != 1 {
                    return Err(StoreError::UnsupportedFormat);
                }
                let unique_content: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pragma_index_list('content_conflict_copies') x JOIN pragma_index_info(x.name) y WHERE x.\"unique\"=1 AND x.partial=0 GROUP BY x.name HAVING count(*)=1 AND max(y.name)='content_operation_id')")
                .fetch_one(&mut *transaction).await.map_err(storage_error)?;
                if unique_content == 0 {
                    sqlx::query("CREATE UNIQUE INDEX copy_content_operation_unique ON content_conflict_copies(content_operation_id)")
                    .execute(&mut *transaction).await.map_err(storage_error)?;
                }
                sqlx::raw_sql(content_conflict::UPGRADE_V20_ATTEMPTS)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                let columns: Vec<(String, String, i64, i64)> = sqlx::query_as("SELECT name,type,\"notnull\",pk FROM pragma_table_info('content_conflict_copy_attempts') ORDER BY cid")
                .fetch_all(&mut *transaction).await.map_err(storage_error)?;
                let expected = [
                    "old_create_operation_id",
                    "operation_id",
                    "registration_id",
                    "intent_version",
                    "old_parent_id",
                    "old_name",
                    "old_node_id",
                    "new_parent_id",
                    "new_name",
                    "new_node_id",
                    "new_create_operation_id",
                ];
                if columns.len() != expected.len()
                    || columns.iter().zip(expected).enumerate().any(
                        |(index, ((name, kind, required, primary), expected))| {
                            name != expected
                                || !kind.eq_ignore_ascii_case(if index == 2 || index == 3 {
                                    "INTEGER"
                                } else {
                                    "TEXT"
                                })
                                || *required != 1
                                || *primary != i64::from(index == 0)
                        },
                    )
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                let links: Vec<(String,String,String)> = sqlx::query_as("SELECT \"table\",\"from\",\"to\" FROM pragma_foreign_key_list('content_conflict_copy_attempts') ORDER BY \"from\"")
                .fetch_all(&mut *transaction).await.map_err(storage_error)?;
                if links
                    != vec![
                        (
                            "client_outbox".into(),
                            "old_create_operation_id".into(),
                            "operation_id".into(),
                        ),
                        (
                            "content_conflict_intents".into(),
                            "operation_id".into(),
                            "operation_id".into(),
                        ),
                        (
                            "client_registrations".into(),
                            "registration_id".into(),
                            "id".into(),
                        ),
                    ]
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                let definition: String = sqlx::query_scalar("SELECT sql FROM sqlite_schema WHERE type='table' AND name='content_conflict_copy_attempts'")
                .fetch_one(&mut *transaction).await.map_err(storage_error)?;
                let normalized: String = definition
                    .chars()
                    .filter(|character| !character.is_ascii_whitespace())
                    .flat_map(char::to_lowercase)
                    .collect();
                if !normalized.contains("check(intent_version>=0)") {
                    return Err(StoreError::UnsupportedFormat);
                }
                let unique_new: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pragma_index_list('content_conflict_copy_attempts') x JOIN pragma_index_info(x.name) y WHERE x.\"unique\"=1 AND x.partial=0 GROUP BY x.name HAVING count(*)=1 AND max(y.name)='new_create_operation_id')")
                .fetch_one(&mut *transaction).await.map_err(storage_error)?;
                if unique_new != 1 {
                    return Err(StoreError::UnsupportedFormat);
                }
                for table in ["content_conflict_copies", "content_conflict_copy_attempts"] {
                    let violation = match table {
                        "content_conflict_copies" => {
                            sqlx::query("PRAGMA foreign_key_check(content_conflict_copies)")
                                .fetch_optional(&mut *transaction)
                                .await
                                .map_err(storage_error)?
                        }
                        _ => {
                            sqlx::query("PRAGMA foreign_key_check(content_conflict_copy_attempts)")
                                .fetch_optional(&mut *transaction)
                                .await
                                .map_err(storage_error)?
                        }
                    };
                    if violation.is_some() {
                        return Err(StoreError::UnsupportedFormat);
                    }
                }
                sqlx::query("PRAGMA user_version=20")
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 20 {
                sqlx::raw_sql(deleted_draft_copy::UPGRADE_V21)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 21 {
                sqlx::raw_sql(outbox::UPGRADE_V22)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            if initialize || existing_version <= 22 {
                sqlx::raw_sql(outbox::UPGRADE_V23)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
            sqlx::raw_sql(credential::UPGRADE_V24)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
            transaction.commit().await.map_err(storage_error)?;
        }
        Ok(Self {
            connection,
            sessions: HashMap::new(),
            _lock: lock,
            database_path,
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
                refreshed_base: false,
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
        if session.refreshed_base && base_exists != 0 {
            sqlx::query("UPDATE draft_bases SET bytes=?,revision=? WHERE document_id=?")
                .bind(&session.base)
                .bind(&session.base_revision)
                .bind(document_id)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
        }
        if base_exists == 0 {
            sqlx::query("INSERT INTO draft_bases (document_id, bytes, revision) VALUES (?, ?, ?) ON CONFLICT(document_id) DO NOTHING")
                .bind(document_id).bind(&session.base).bind(&session.base_revision)
                .execute(&mut *transaction).await.map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)?;
        if let Some(session) = self.sessions.get_mut(document_id) {
            session.refreshed_base = false;
        }
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

    pub fn invalidate_edit_session(&mut self, edit_session_id: &str) {
        self.sessions
            .retain(|_, session| session.id != edit_session_id);
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
