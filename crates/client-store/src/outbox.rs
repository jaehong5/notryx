use super::{
    ClientStore, RegistrationBinding, StoreError, decode_draft, storage_error, validate_document,
};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteRow};

pub(crate) const CREATE_SCHEMA: &str = "
    CREATE TABLE outbox_streams (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        next_seq INTEGER NOT NULL CHECK (next_seq > 0),
        ack_through INTEGER NOT NULL CHECK (ack_through >= 0)
    );
    CREATE TABLE client_outbox (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        document_id TEXT NOT NULL,
        storage_id TEXT NOT NULL REFERENCES registered_documents(storage_id),
        generation INTEGER NOT NULL CHECK (generation > 0),
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        UNIQUE(registration_id, operation_seq)
    );
    PRAGMA user_version = 3;
";

pub(crate) const UPGRADE_V4: &str = "
    ALTER TABLE client_outbox ADD COLUMN abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent'));
    ALTER TABLE client_outbox ADD COLUMN terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied'));
    PRAGMA user_version = 4;
";

pub(crate) const UPGRADE_V5: &str = "
    ALTER TABLE outbox_streams ADD COLUMN confirmed_through INTEGER NOT NULL DEFAULT 0 CHECK (confirmed_through >= 0);
    ALTER TABLE client_outbox ADD COLUMN payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1));
    ALTER TABLE client_outbox ADD COLUMN payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880);
    UPDATE client_outbox SET payload_len = length(bytes);
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 5;
";

pub(crate) const UPGRADE_V8: &str = "
    PRAGMA defer_foreign_keys = ON;
    CREATE TABLE client_outbox_v8 (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox_v8(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind = 'restore' AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
    INSERT INTO client_outbox_v8
        (operation_id, registration_id, operation_seq, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len)
    SELECT operation_id, registration_id, operation_seq, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len
    FROM client_outbox;
    DROP TABLE client_outbox;
    ALTER TABLE client_outbox_v8 RENAME TO client_outbox;
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 8;
";

pub(crate) const UPGRADE_V10: &str = "
    PRAGMA defer_foreign_keys = ON;
    CREATE TABLE client_outbox_v10 (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore', 'create')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox_v10(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder', 'node_id_conflict')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind IN ('restore', 'create') AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
    INSERT INTO client_outbox_v10
        (operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json)
    SELECT operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json
    FROM client_outbox;
    DROP TABLE client_outbox;
    ALTER TABLE client_outbox_v10 RENAME TO client_outbox;
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 10;
";

pub(crate) const UPGRADE_V12: &str = "
    PRAGMA defer_foreign_keys = ON;
    CREATE TABLE create_reconciliations_v12 (
        old_operation_id TEXT PRIMARY KEY NOT NULL,
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE,
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations_v12 SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations;
    DROP TABLE create_reconciliations;
    CREATE TABLE client_outbox_v12 (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore', 'create', 'delete')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox_v12(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder', 'node_id_conflict', 'node_precondition_failed')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind IN ('restore', 'create', 'delete') AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
    INSERT INTO client_outbox_v12
        (operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json)
    SELECT operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json
    FROM client_outbox;
    DROP TABLE client_outbox;
    ALTER TABLE client_outbox_v12 RENAME TO client_outbox;
    CREATE TABLE create_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations_v12;
    DROP TABLE create_reconciliations_v12;
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 12;
";

pub(crate) const UPGRADE_V14: &str = "
    PRAGMA defer_foreign_keys = ON;
    CREATE TABLE create_reconciliations_v14 (
        old_operation_id TEXT PRIMARY KEY NOT NULL,
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE,
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations_v14 SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations;
    DROP TABLE create_reconciliations;
    CREATE TABLE delete_reconciliations_v14 (
        old_operation_id TEXT PRIMARY KEY NOT NULL,
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE,
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO delete_reconciliations_v14 SELECT old_operation_id, decision, new_operation_id FROM delete_reconciliations;
    DROP TABLE delete_reconciliations;
    CREATE TABLE client_outbox_v14 (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore', 'create', 'delete', 'move')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox_v14(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder', 'node_id_conflict', 'node_precondition_failed', 'expected_parent_mismatch')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind IN ('restore', 'create', 'delete', 'move') AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
    INSERT INTO client_outbox_v14
        (operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json)
    SELECT operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json
    FROM client_outbox;
    DROP TABLE client_outbox;
    ALTER TABLE client_outbox_v14 RENAME TO client_outbox;
    CREATE TABLE create_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations_v14;
    DROP TABLE create_reconciliations_v14;
    CREATE TABLE delete_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO delete_reconciliations SELECT old_operation_id, decision, new_operation_id FROM delete_reconciliations_v14;
    DROP TABLE delete_reconciliations_v14;
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 14;
";

pub(crate) const UPGRADE_V13: &str = "
    CREATE TABLE delete_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    PRAGMA user_version = 13;
";

pub(crate) const UPGRADE_V15: &str = "
    CREATE TABLE move_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    PRAGMA user_version = 15;
";

pub(crate) const UPGRADE_V11: &str = "
    CREATE TABLE create_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    PRAGMA user_version = 11;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxStatus {
    Queued,
    Sent,
    Committed,
    Conflict,
    Abandoning,
    Abandoned,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxAbandonState {
    None,
    Requested,
    Sent,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OutboxOperation {
    pub binding: RegistrationBinding,
    pub operation_id: String,
    pub operation_seq: u64,
    pub document_id: String,
    pub generation: u64,
    pub expected_revision: Option<String>,
    pub conflict_revision: Option<String>,
    pub bytes: Vec<u8>,
    pub payload_hash: String,
    pub status: OutboxStatus,
    pub frozen: bool,
    pub failure_kind: Option<OutboxFailureKind>,
    pub abandon_state: OutboxAbandonState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutboxOperation {
    pub binding: RegistrationBinding,
    pub operation_id: String,
    pub operation_seq: u64,
    pub node_id: notryx_domain::NodeId,
    pub request: notryx_protocol::RestoreNodeRequest,
    pub status: OutboxStatus,
    pub abandon_state: OutboxAbandonState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOutboxOperation {
    pub binding: RegistrationBinding,
    pub operation_id: String,
    pub operation_seq: u64,
    pub request: notryx_protocol::CreateNodeRequest,
    pub status: OutboxStatus,
    pub abandon_state: OutboxAbandonState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutboxOperation {
    pub binding: RegistrationBinding,
    pub operation_id: String,
    pub operation_seq: u64,
    pub node_id: notryx_domain::NodeId,
    pub expected_node_etag: String,
    pub status: OutboxStatus,
    pub abandon_state: OutboxAbandonState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutboxOperation {
    pub binding: RegistrationBinding,
    pub operation_id: String,
    pub operation_seq: u64,
    pub node_id: notryx_domain::NodeId,
    pub expected_meta_etag: String,
    pub request: notryx_protocol::MoveNodeRequest,
    pub status: OutboxStatus,
    pub abandon_state: OutboxAbandonState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutboxState {
    pub operation: MoveOutboxOperation,
    pub receipt: Option<notryx_protocol::OperationReceipt>,
    pub frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutboxPage {
    pub entries: Vec<MoveOutboxState>,
    pub next_after_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenMovePage {
    pub entries: Vec<MoveOutboxState>,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrozenMoveChoice {
    Apply {
        expected_meta_etag: String,
        request: notryx_protocol::MoveNodeRequest,
    },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutboxState {
    pub operation: DeleteOutboxOperation,
    pub receipt: Option<notryx_protocol::OperationReceipt>,
    pub frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutboxPage {
    pub entries: Vec<DeleteOutboxState>,
    pub next_after_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrozenDeleteChoice {
    Apply { expected_node_etag: String },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenDeletePage {
    pub entries: Vec<DeleteOutboxState>,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrozenCreateChoice {
    Apply {
        parent_id: notryx_domain::NodeId,
        name: String,
    },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOutboxState {
    pub operation: CreateOutboxOperation,
    pub receipt: Option<notryx_protocol::OperationReceipt>,
    pub frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOutboxPage {
    pub entries: Vec<CreateOutboxState>,
    pub next_after_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenCreatePage {
    pub entries: Vec<CreateOutboxState>,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingOutboxOperation {
    Content(OutboxOperation),
    Restore(RestoreOutboxOperation),
    Create(CreateOutboxOperation),
    Delete(DeleteOutboxOperation),
    Move(MoveOutboxOperation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutboxState {
    pub operation: RestoreOutboxOperation,
    pub receipt: Option<notryx_protocol::OperationReceipt>,
    pub frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutboxPage {
    pub entries: Vec<RestoreOutboxState>,
    pub next_after_seq: Option<u64>,
}

impl std::fmt::Debug for OutboxOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutboxOperation")
            .field("binding", &self.binding)
            .field("operation_id", &self.operation_id)
            .field("operation_seq", &self.operation_seq)
            .field("document_id", &self.document_id)
            .field("generation", &self.generation)
            .field("expected_revision", &self.expected_revision)
            .field("conflict_revision", &self.conflict_revision)
            .field("payload_bytes", &self.bytes.len())
            .field("payload_hash", &self.payload_hash)
            .field("status", &self.status)
            .field("frozen", &self.frozen)
            .field("failure_kind", &self.failure_kind)
            .field("abandon_state", &self.abandon_state)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxFailureKind {
    NodeDeleted,
    RecoveryConflict,
    NotFound,
    NotApplied,
}

impl OutboxFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::NodeDeleted => "node_deleted",
            Self::RecoveryConflict => "recovery_conflict",
            Self::NotFound => "not_found",
            Self::NotApplied => "not_applied",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "node_deleted" => Some(Self::NodeDeleted),
            "recovery_conflict" => Some(Self::RecoveryConflict),
            "not_found" => Some(Self::NotFound),
            "not_applied" => Some(Self::NotApplied),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxFailure {
    pub operation_id: String,
    pub operation_seq: u64,
    pub workspace_epoch: String,
    pub kind: OutboxFailureKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxAbandoned {
    pub operation_id: String,
    pub operation_seq: u64,
    pub workspace_epoch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxSummary {
    pub operation_id: String,
    pub operation_seq: u64,
    pub document_id: String,
    pub generation: u64,
    pub status: OutboxStatus,
    pub frozen: bool,
    pub payload_len: usize,
    pub abandon_state: OutboxAbandonState,
    pub failure_kind: Option<OutboxFailureKind>,
    pub payload_retired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxDocumentState {
    pub latest: Option<OutboxSummary>,
    pub has_conflict: bool,
    pub has_failure: bool,
    pub confirmed_through: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxPage {
    pub entries: Vec<OutboxSummary>,
    pub next_after_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxCommit {
    pub operation_id: String,
    pub operation_seq: u64,
    pub workspace_epoch: String,
    pub document_id: String,
    pub content_revision: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxConflict {
    pub operation_id: String,
    pub operation_seq: u64,
    pub workspace_epoch: String,
    pub current_revision: String,
    pub current_etag: String,
}

pub(crate) fn payload_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

fn valid_content_revision(revision: &str) -> bool {
    notryx_protocol::ContentPrecondition::parse(&format!("\"{revision}\""))
        .and_then(|value| value.revision())
        .is_ok_and(|value| value.as_str() == revision)
}

pub(crate) async fn requires_reconciliation(
    connection: &mut SqliteConnection,
    workspace_id: &str,
    document_id: &str,
) -> Result<bool, StoreError> {
    validate_relevant_dismissals(connection, workspace_id, document_id).await?;
    let histories = sqlx::query("SELECT z.* FROM frozen_content_reconciliations z JOIN client_outbox o ON o.operation_id=z.old_operation_id JOIN client_registrations r ON r.id=o.registration_id WHERE r.status='frozen' AND r.workspace_id=? AND o.document_id=?")
        .bind(workspace_id).bind(document_id).fetch_all(&mut *connection).await.map_err(storage_error)?;
    for history in histories {
        let stored: String = history.try_get("history_hash").map_err(storage_error)?;
        let fingerprint: String = history.try_get("view_fingerprint").map_err(storage_error)?;
        let approval: String = history.try_get("approval_hash").map_err(storage_error)?;
        if !valid_frozen_fingerprint(&stored)
            || !valid_frozen_fingerprint(&fingerprint)
            || !valid_frozen_fingerprint(&approval)
            || frozen_decision_history_hash(&history)? != stored
        {
            return Ok(true);
        }
    }
    let exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM client_registrations r
            JOIN registered_documents d ON d.registration_id = r.id
            LEFT JOIN draft_checkpoints c ON c.document_id = d.storage_id
            LEFT JOIN draft_bases b ON b.document_id = d.storage_id
            WHERE r.status = 'frozen' AND r.workspace_id = ? AND d.document_id = ?
              AND (
                (c.document_id IS NOT NULL AND NOT EXISTS (
                    SELECT 1 FROM client_outbox x
                    WHERE x.registration_id = r.id AND x.document_id = d.document_id
                      AND x.operation_kind = 'content' AND x.status = 'committed' AND x.terminal_kind IS NULL
                      AND x.generation = c.generation AND x.bytes = c.bytes
                      AND b.bytes = x.bytes AND b.revision = x.committed_revision
                ) AND NOT EXISTS (
                    SELECT 1 FROM frozen_content_reconciliations z
                    JOIN client_outbox o ON o.operation_id=z.old_operation_id
                    JOIN client_registrations current ON current.id=z.current_registration_id AND current.status='active'
                    JOIN client_outbox applied ON applied.operation_id=z.new_operation_id AND applied.registration_id=current.id AND applied.document_id=d.document_id AND applied.operation_kind='content'
                    JOIN registered_documents active_doc ON active_doc.registration_id=current.id AND active_doc.document_id=d.document_id AND active_doc.storage_id=applied.storage_id
                    WHERE applied.generation=1 AND applied.operation_seq=z.new_operation_seq AND applied.expected_revision=z.new_expected_revision AND applied.payload_hash=z.new_payload_hash AND applied.payload_len=length(z.draft_bytes)
                      AND ((applied.payload_retired=0 AND applied.bytes=z.draft_bytes) OR (applied.payload_retired=1 AND applied.status='committed' AND applied.terminal_kind IS NULL AND length(applied.bytes)=0 AND EXISTS(SELECT 1 FROM outbox_streams st WHERE st.registration_id=current.id AND st.confirmed_through>=applied.operation_seq)))
                      AND o.registration_id=r.id AND o.document_id=d.document_id AND o.operation_kind='content'
                      AND c.generation=z.generation AND c.bytes=z.draft_bytes AND b.bytes=z.base_bytes AND b.revision=z.base_revision
                      AND o.bytes=z.old_payload AND o.payload_hash=z.old_payload_hash AND o.status=z.old_status AND o.abandon_state='none'
                      AND o.generation=z.old_generation AND o.operation_seq=z.old_seq AND o.expected_revision IS z.old_expected_revision
                      AND o.dependency IS z.old_dependency AND o.payload_len=z.old_payload_len AND o.payload_retired=z.old_payload_retired
                      AND o.storage_id=d.storage_id AND o.terminal_kind IS NULL
                ))
                OR EXISTS (
                    SELECT 1 FROM client_outbox x
                    WHERE x.registration_id = r.id AND x.document_id = d.document_id
                      AND x.status IN ('queued', 'sent', 'conflict')
                      AND (x.operation_kind <> 'content' OR NOT EXISTS (
                          SELECT 1 FROM frozen_content_reconciliations z
                          JOIN client_registrations current ON current.id=z.current_registration_id AND current.status='active'
                          JOIN client_outbox applied ON applied.operation_id=z.new_operation_id AND applied.registration_id=current.id AND applied.document_id=d.document_id AND applied.operation_kind='content'
                          JOIN registered_documents active_doc ON active_doc.registration_id=current.id AND active_doc.document_id=d.document_id AND active_doc.storage_id=applied.storage_id
                          WHERE applied.generation=1 AND applied.operation_seq=z.new_operation_seq AND applied.expected_revision=z.new_expected_revision AND applied.payload_hash=z.new_payload_hash AND applied.payload_len=length(z.draft_bytes)
                            AND ((applied.payload_retired=0 AND applied.bytes=z.draft_bytes) OR (applied.payload_retired=1 AND applied.status='committed' AND applied.terminal_kind IS NULL AND length(applied.bytes)=0 AND EXISTS(SELECT 1 FROM outbox_streams st WHERE st.registration_id=current.id AND st.confirmed_through>=applied.operation_seq)))
                            AND z.old_operation_id=x.operation_id AND c.generation=z.generation AND c.bytes=z.draft_bytes
                            AND b.bytes=z.base_bytes AND b.revision=z.base_revision AND x.bytes=z.old_payload
                            AND x.payload_hash=z.old_payload_hash AND x.status=z.old_status AND x.abandon_state='none'
                            AND x.generation=z.old_generation AND x.operation_seq=z.old_seq AND x.expected_revision IS z.old_expected_revision
                            AND x.dependency IS z.old_dependency AND x.payload_len=z.old_payload_len AND x.payload_retired=z.old_payload_retired
                            AND x.storage_id=d.storage_id AND x.terminal_kind IS NULL
                      ))
                      AND (x.operation_kind <> 'content' OR NOT EXISTS (
                          SELECT 1 FROM frozen_content_dismissals dismissed
                          JOIN client_registrations current ON current.id=dismissed.current_registration_id AND current.status='active'
                          WHERE dismissed.old_operation_id=x.operation_id
                      ))
                      AND (x.operation_kind <> 'create' OR NOT EXISTS (
                          SELECT 1 FROM create_reconciliations z WHERE z.old_operation_id=x.operation_id
                      )) AND (x.operation_kind <> 'delete' OR NOT EXISTS (
                          SELECT 1 FROM delete_reconciliations z WHERE z.old_operation_id=x.operation_id
                      )) AND (x.operation_kind <> 'move' OR NOT EXISTS (
                          SELECT 1 FROM move_reconciliations z WHERE z.old_operation_id=x.operation_id
                      ))
                )
              )
        ) OR EXISTS (
            SELECT 1 FROM client_registrations r
            JOIN client_outbox x ON x.registration_id = r.id
            WHERE r.status = 'frozen' AND r.workspace_id = ? AND x.document_id = ?
              AND x.status IN ('queued', 'sent', 'conflict')
              AND ((x.operation_kind = 'content' AND NOT EXISTS (SELECT 1 FROM registered_documents mapping WHERE mapping.registration_id=r.id AND mapping.document_id=x.document_id AND mapping.storage_id=x.storage_id)) OR x.operation_kind = 'restore' OR (x.operation_kind = 'move' AND NOT EXISTS (SELECT 1 FROM move_reconciliations z WHERE z.old_operation_id=x.operation_id)) OR (x.operation_kind = 'delete' AND NOT EXISTS (SELECT 1 FROM delete_reconciliations z WHERE z.old_operation_id=x.operation_id)) OR (x.operation_kind = 'create' AND NOT EXISTS (
                  SELECT 1 FROM create_reconciliations z WHERE z.old_operation_id=x.operation_id
              )))
        )",
    )
    .bind(workspace_id)
    .bind(document_id)
    .bind(workspace_id)
    .bind(document_id)
    .fetch_one(connection)
    .await
    .map_err(storage_error)?;
    Ok(exists != 0)
}

fn abandon_state(row: &SqliteRow) -> Result<OutboxAbandonState, StoreError> {
    let value: String = row.try_get("abandon_state").map_err(storage_error)?;
    match value.as_str() {
        "none" => Ok(OutboxAbandonState::None),
        "requested" => Ok(OutboxAbandonState::Requested),
        "sent" => Ok(OutboxAbandonState::Sent),
        _ => Err(StoreError::UnsupportedFormat),
    }
}

fn status(row: &SqliteRow) -> Result<(OutboxStatus, Option<OutboxFailureKind>), StoreError> {
    let state: String = row.try_get("status").map_err(storage_error)?;
    let abandon: String = row.try_get("abandon_state").map_err(storage_error)?;
    let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
    match (state.as_str(), abandon.as_str(), terminal.as_deref()) {
        ("queued", "none", None) => Ok((OutboxStatus::Queued, None)),
        ("queued", "requested" | "sent", None) => Ok((OutboxStatus::Abandoning, None)),
        ("sent", "none", None) => Ok((OutboxStatus::Sent, None)),
        ("committed", "none", None) => Ok((OutboxStatus::Committed, None)),
        ("committed", "sent", Some("abandoned")) => Ok((OutboxStatus::Abandoned, None)),
        ("conflict", "none", None) => Ok((OutboxStatus::Conflict, None)),
        ("conflict", "none", Some(kind)) => OutboxFailureKind::parse(kind)
            .map(|value| (OutboxStatus::Failed, Some(value)))
            .ok_or(StoreError::UnsupportedFormat),
        _ => Err(StoreError::UnsupportedFormat),
    }
}

pub(crate) fn operation(
    row: &SqliteRow,
    binding: &RegistrationBinding,
    frozen: bool,
) -> Result<OutboxOperation, StoreError> {
    let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
    if kind != "content" {
        return Err(StoreError::UnsupportedFormat);
    }
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    if retired == 1 {
        return Err(StoreError::PayloadRetired);
    }
    if retired != 0 {
        return Err(StoreError::UnsupportedFormat);
    }
    let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let generation: i64 = row.try_get("generation").map_err(storage_error)?;
    if sequence <= 0 || generation <= 0 {
        return Err(StoreError::UnsupportedFormat);
    }
    let (status, failure_kind) = status(row)?;
    let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    let payload_len: i64 = row.try_get("payload_len").map_err(storage_error)?;
    if bytes.len() > super::MAX_DOCUMENT_BYTES
        || payload_len != bytes.len() as i64
        || payload_hash(&bytes) != hash
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(OutboxOperation {
        binding: binding.clone(),
        operation_id: row.try_get("operation_id").map_err(storage_error)?,
        operation_seq: sequence as u64,
        document_id: row.try_get("document_id").map_err(storage_error)?,
        generation: generation as u64,
        expected_revision: row.try_get("expected_revision").map_err(storage_error)?,
        conflict_revision: row.try_get("conflict_revision").map_err(storage_error)?,
        bytes,
        payload_hash: hash,
        status,
        frozen,
        failure_kind,
        abandon_state: abandon_state(row)?,
    })
}

fn summary(row: &SqliteRow, frozen: bool) -> Result<OutboxSummary, StoreError> {
    let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let generation: i64 = row.try_get("generation").map_err(storage_error)?;
    let length: i64 = row.try_get("payload_len").map_err(storage_error)?;
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    if sequence <= 0
        || generation <= 0
        || length < 0
        || length as u64 > super::MAX_DOCUMENT_BYTES as u64
        || !matches!(retired, 0 | 1)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let (state, failure_kind) = status(row)?;
    Ok(OutboxSummary {
        operation_id: row.try_get("operation_id").map_err(storage_error)?,
        operation_seq: sequence as u64,
        document_id: row.try_get("document_id").map_err(storage_error)?,
        generation: generation as u64,
        status: state,
        frozen,
        payload_len: length as usize,
        abandon_state: abandon_state(row)?,
        failure_kind,
        payload_retired: retired == 1,
    })
}

fn pending_operation(
    row: &SqliteRow,
    binding: &RegistrationBinding,
) -> Result<PendingOutboxOperation, StoreError> {
    let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
    if kind == "content" {
        return Ok(PendingOutboxOperation::Content(operation(
            row, binding, false,
        )?));
    }
    if kind == "create" {
        return pending_create_operation(row, binding).map(PendingOutboxOperation::Create);
    }
    if kind == "delete" {
        return pending_delete_operation(row, binding).map(PendingOutboxOperation::Delete);
    }
    if kind == "move" {
        return pending_move_operation(row, binding).map(PendingOutboxOperation::Move);
    }
    if kind != "restore" {
        return Err(StoreError::UnsupportedFormat);
    }
    let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let operation_id: String = row.try_get("operation_id").map_err(storage_error)?;
    let node: String = row.try_get("document_id").map_err(storage_error)?;
    let parsed_operation: notryx_domain::OperationId = operation_id
        .parse()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let node_id: notryx_domain::NodeId = node.parse().map_err(|_| StoreError::UnsupportedFormat)?;
    let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    let payload_len: i64 = row.try_get("payload_len").map_err(storage_error)?;
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    let request: notryx_protocol::RestoreNodeRequest =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::UnsupportedFormat)?;
    let state_value: String = row.try_get("status").map_err(storage_error)?;
    let abandon = abandon_state(row)?;
    let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
    let state = match (state_value.as_str(), abandon, terminal.as_deref()) {
        ("queued", OutboxAbandonState::None, None) => OutboxStatus::Queued,
        ("sent", OutboxAbandonState::None, None) => OutboxStatus::Sent,
        ("queued" | "sent", OutboxAbandonState::Requested | OutboxAbandonState::Sent, None) => {
            OutboxStatus::Abandoning
        }
        _ => return Err(StoreError::UnsupportedFormat),
    };
    if sequence <= 0
        || parsed_operation.to_string() != operation_id
        || node_id.to_string() != node
        || request.parent_id == node_id
        || bytes.len() > super::MAX_DOCUMENT_BYTES
        || payload_len != bytes.len() as i64
        || payload_hash(&bytes) != hash
        || retired != 0
        || !matches!(
            state,
            OutboxStatus::Queued | OutboxStatus::Sent | OutboxStatus::Abandoning
        )
        || row
            .try_get::<Option<String>, _>("storage_id")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<i64>, _>("generation")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("dependency")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("expected_revision")
            .map_err(storage_error)?
            .is_some()
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(PendingOutboxOperation::Restore(RestoreOutboxOperation {
        binding: binding.clone(),
        operation_id,
        operation_seq: sequence as u64,
        node_id,
        request,
        status: state,
        abandon_state: abandon,
    }))
}

async fn verify_prior_delete_conflicts(
    connection: &mut SqliteConnection,
    registration_id: i64,
    binding: &RegistrationBinding,
    document_id: &str,
) -> Result<(), StoreError> {
    let rows = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,'sent' AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='delete' AND status='conflict' AND terminal_kind='node_precondition_failed'")
        .bind(registration_id).bind(document_id).fetch_all(&mut *connection).await.map_err(storage_error)?;
    for row in rows {
        let item = validated_delete_record(&row, binding)?;
        if item.status != OutboxStatus::Failed || item.node_id.to_string() != document_id {
            return Err(StoreError::UnsupportedFormat);
        }
        let ack: i64 =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id=?")
                .bind(registration_id)
                .fetch_one(&mut *connection)
                .await
                .map_err(storage_error)?;
        if u64::try_from(ack)
            .ok()
            .is_none_or(|ack| ack < item.operation_seq)
        {
            return Err(StoreError::UnsupportedFormat);
        }
    }
    Ok(())
}

async fn move_receipt_conflicts_with_cached_source(
    connection: &mut SqliteConnection,
    registration_id: i64,
    item: &MoveOutboxOperation,
    node: &notryx_protocol::NodeState,
) -> Result<bool, StoreError> {
    if item.request.parent_id != item.request.expected_parent_id
        || node.meta_etag != item.expected_meta_etag
    {
        return Ok(false);
    }
    let cached: Option<String> =
        sqlx::query_scalar("SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?")
            .bind(registration_id)
            .bind(item.node_id.to_string())
            .fetch_optional(connection)
            .await
            .map_err(storage_error)?;
    let Some(cached) = cached else {
        return Ok(false);
    };
    let current: notryx_protocol::NodeState =
        serde_json::from_str(&cached).map_err(|_| StoreError::UnsupportedFormat)?;
    if current.node_id != item.node_id
        || notryx_protocol::StrongEtag::from_meta_revision(&current.meta_rev)
            .map_err(|_| StoreError::UnsupportedFormat)?
            .as_str()
            != current.meta_etag
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(current.meta_etag == item.expected_meta_etag
        && current.parent_id == Some(item.request.expected_parent_id)
        && current.name != item.request.name)
}

fn pending_move_operation(
    row: &SqliteRow,
    binding: &RegistrationBinding,
) -> Result<MoveOutboxOperation, StoreError> {
    let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let operation_id: String = row.try_get("operation_id").map_err(storage_error)?;
    let parsed: notryx_domain::OperationId = operation_id
        .parse()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let node: String = row.try_get("document_id").map_err(storage_error)?;
    let node_id: notryx_domain::NodeId = node.parse().map_err(|_| StoreError::UnsupportedFormat)?;
    let expected_meta_etag: String = row.try_get("expected_revision").map_err(storage_error)?;
    let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
    let request: notryx_protocol::MoveNodeRequest =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::UnsupportedFormat)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    let status: String = row.try_get("status").map_err(storage_error)?;
    let abandon = abandon_state(row)?;
    let status = match (status.as_str(), abandon) {
        ("queued", OutboxAbandonState::None) => OutboxStatus::Queued,
        ("sent", OutboxAbandonState::None) => OutboxStatus::Sent,
        ("queued" | "sent", OutboxAbandonState::Requested | OutboxAbandonState::Sent) => {
            OutboxStatus::Abandoning
        }
        _ => return Err(StoreError::UnsupportedFormat),
    };
    if sequence <= 0
        || parsed.to_string() != operation_id
        || node_id.to_string() != node
        || expected_meta_etag.len() > 256
        || notryx_protocol::MetadataPrecondition::parse(&expected_meta_etag)
            .and_then(|p| p.revision())
            .is_err()
        || request.parent_id == node_id
        || request.expected_parent_id == node_id
        || !request.name.to_ascii_lowercase().ends_with(".md")
        || row
            .try_get::<i64, _>("payload_len")
            .map_err(storage_error)?
            != bytes.len() as i64
        || row
            .try_get::<i64, _>("payload_retired")
            .map_err(storage_error)?
            != 0
        || hash != payload_hash(&bytes)
        || row
            .try_get::<Option<String>, _>("terminal_kind")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("storage_id")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<i64>, _>("generation")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("dependency")
            .map_err(storage_error)?
            .is_some()
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(MoveOutboxOperation {
        binding: binding.clone(),
        operation_id,
        operation_seq: sequence as u64,
        node_id,
        expected_meta_etag,
        request,
        status,
        abandon_state: abandon,
    })
}

fn pending_delete_operation(
    row: &SqliteRow,
    binding: &RegistrationBinding,
) -> Result<DeleteOutboxOperation, StoreError> {
    let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let operation_id: String = row.try_get("operation_id").map_err(storage_error)?;
    let parsed: notryx_domain::OperationId = operation_id
        .parse()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let node: String = row.try_get("document_id").map_err(storage_error)?;
    let node_id: notryx_domain::NodeId = node.parse().map_err(|_| StoreError::UnsupportedFormat)?;
    let etag: String = row.try_get("expected_revision").map_err(storage_error)?;
    let status: String = row.try_get("status").map_err(storage_error)?;
    let abandon = abandon_state(row)?;
    let status = match (status.as_str(), abandon) {
        ("queued", OutboxAbandonState::None) => OutboxStatus::Queued,
        ("sent", OutboxAbandonState::None) => OutboxStatus::Sent,
        ("queued" | "sent", OutboxAbandonState::Requested | OutboxAbandonState::Sent) => {
            OutboxStatus::Abandoning
        }
        _ => return Err(StoreError::UnsupportedFormat),
    };
    let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    if sequence <= 0
        || parsed.to_string() != operation_id
        || node_id.to_string() != node
        || etag.len() > 256
        || notryx_protocol::StrongEtag::parse(&etag).is_err()
        || !bytes.is_empty()
        || hash != payload_hash(&bytes)
        || row
            .try_get::<i64, _>("payload_len")
            .map_err(storage_error)?
            != 0
        || row
            .try_get::<i64, _>("payload_retired")
            .map_err(storage_error)?
            != 0
        || row
            .try_get::<Option<String>, _>("terminal_kind")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("storage_id")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<i64>, _>("generation")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("dependency")
            .map_err(storage_error)?
            .is_some()
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(DeleteOutboxOperation {
        binding: binding.clone(),
        operation_id,
        operation_seq: sequence as u64,
        node_id,
        expected_node_etag: etag,
        status,
        abandon_state: abandon,
    })
}

pub(crate) fn pending_create_operation(
    row: &SqliteRow,
    binding: &RegistrationBinding,
) -> Result<CreateOutboxOperation, StoreError> {
    let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let operation_id: String = row.try_get("operation_id").map_err(storage_error)?;
    let parsed_operation: notryx_domain::OperationId = operation_id
        .parse()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let node: String = row.try_get("document_id").map_err(storage_error)?;
    let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
    let request: notryx_protocol::CreateNodeRequest =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::UnsupportedFormat)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    let payload_len: i64 = row.try_get("payload_len").map_err(storage_error)?;
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    let state_value: String = row.try_get("status").map_err(storage_error)?;
    let abandon = abandon_state(row)?;
    let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
    let status = match (state_value.as_str(), abandon, terminal.as_deref()) {
        ("queued", OutboxAbandonState::None, None) => OutboxStatus::Queued,
        ("sent", OutboxAbandonState::None, None) => OutboxStatus::Sent,
        ("queued" | "sent", OutboxAbandonState::Requested | OutboxAbandonState::Sent, None) => {
            OutboxStatus::Abandoning
        }
        _ => return Err(StoreError::UnsupportedFormat),
    };
    if sequence <= 0
        || parsed_operation.to_string() != operation_id
        || request.node_id.to_string() != node
        || request.node_id == request.parent_id
        || request.upload_id.is_some()
        || payload_len != bytes.len() as i64
        || payload_hash(&bytes) != hash
        || retired != 0
        || row
            .try_get::<Option<String>, _>("storage_id")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<i64>, _>("generation")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("dependency")
            .map_err(storage_error)?
            .is_some()
        || row
            .try_get::<Option<String>, _>("expected_revision")
            .map_err(storage_error)?
            .is_some()
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(CreateOutboxOperation {
        binding: binding.clone(),
        operation_id,
        operation_seq: sequence as u64,
        request,
        status,
        abandon_state: abandon,
    })
}

fn delete_receipt_for_display(
    item: &DeleteOutboxOperation,
    raw: &str,
    terminal: Option<&str>,
) -> Result<notryx_protocol::OperationReceipt, StoreError> {
    use notryx_protocol::{NodeKind, OperationReceipt, StrongEtag};
    if raw.len() > 65536 {
        return Err(StoreError::UnsupportedFormat);
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| StoreError::UnsupportedFormat)?;
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::UnsupportedFormat)?;
    if value
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
        != Some(item.operation_id.as_str())
        || value
            .get("operation_seq")
            .and_then(serde_json::Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            != Some(item.operation_seq)
        || value
            .get("workspace_epoch")
            .and_then(serde_json::Value::as_str)
            != Some(item.binding.workspace_epoch.as_str())
        || status != terminal.unwrap_or("deleted")
        || !matches!(
            status,
            "deleted"
                | "node_precondition_failed"
                | "node_deleted"
                | "not_found"
                | "not_applied"
                | "recovery_conflict"
                | "abandoned"
        )
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let receipt: OperationReceipt =
        serde_json::from_value(value).map_err(|_| StoreError::UnsupportedFormat)?;
    match &receipt {
        OperationReceipt::Deleted {
            node, commit_seq, ..
        } => {
            let content_revision = node
                .content_rev
                .as_ref()
                .ok_or(StoreError::UnsupportedFormat)?;
            let content_etag = StrongEtag::from_content_revision(content_revision)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            let meta_etag = StrongEtag::from_meta_revision(&node.meta_rev)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            let hash = node
                .content_hash
                .as_deref()
                .and_then(|v| v.strip_prefix("sha256:"))
                .ok_or(StoreError::UnsupportedFormat)?;
            if node.node_id != item.node_id
                || node.kind != NodeKind::File
                || !node.deleted
                || node.parent_id.is_none()
                || commit_seq.get() == 0
                || node.last_change_seq != *commit_seq
                || node.meta_etag != meta_etag.as_str()
                || node.content_etag.as_deref() != Some(content_etag.as_str())
                || StrongEtag::parse(&node.node_etag).is_err()
                || hash.len() != 64
                || !hash
                    .bytes()
                    .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
                || node
                    .size
                    .as_deref()
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_none()
            {
                return Err(StoreError::UnsupportedFormat);
            }
        }
        OperationReceipt::NodePreconditionFailed {
            current_node_etag, ..
        } if (current_node_etag.len() > 256
            || StrongEtag::parse(current_node_etag).is_err()
            || current_node_etag == &item.expected_node_etag) =>
        {
            return Err(StoreError::UnsupportedFormat);
        }
        _ => {}
    }
    Ok(receipt)
}

fn move_receipt_for_display(
    item: &MoveOutboxOperation,
    raw: &str,
    terminal: Option<&str>,
) -> Result<notryx_protocol::OperationReceipt, StoreError> {
    use notryx_protocol::{NodeKind, OperationReceipt, StrongEtag};
    if raw.len() > 65536 {
        return Err(StoreError::UnsupportedFormat);
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| StoreError::UnsupportedFormat)?;
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::UnsupportedFormat)?;
    if value
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
        != Some(item.operation_id.as_str())
        || value
            .get("operation_seq")
            .and_then(serde_json::Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            != Some(item.operation_seq)
        || value
            .get("workspace_epoch")
            .and_then(serde_json::Value::as_str)
            != Some(item.binding.workspace_epoch.as_str())
        || status != terminal.unwrap_or("moved")
        || !matches!(
            status,
            "moved"
                | "metadata_precondition_failed"
                | "expected_parent_mismatch"
                | "destination_exists"
                | "parent_not_folder"
                | "node_deleted"
                | "not_found"
                | "not_applied"
                | "recovery_conflict"
                | "abandoned"
        )
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let receipt: OperationReceipt =
        serde_json::from_value(value).map_err(|_| StoreError::UnsupportedFormat)?;
    match &receipt {
        OperationReceipt::Moved {
            node, commit_seq, ..
        } => {
            let content_revision = node
                .content_rev
                .as_ref()
                .ok_or(StoreError::UnsupportedFormat)?;
            let content_etag = StrongEtag::from_content_revision(content_revision)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            let meta_etag = StrongEtag::from_meta_revision(&node.meta_rev)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            let hash = node
                .content_hash
                .as_deref()
                .and_then(|v| v.strip_prefix("sha256:"))
                .ok_or(StoreError::UnsupportedFormat)?;
            if node.node_id != item.node_id
                || node.kind != NodeKind::File
                || node.deleted
                || node.parent_id != Some(item.request.parent_id)
                || node.name != item.request.name
                || (item.request.parent_id != item.request.expected_parent_id
                    && node.meta_etag == item.expected_meta_etag)
                || commit_seq.get() == 0
                || node.last_change_seq != *commit_seq
                || node.meta_etag != meta_etag.as_str()
                || node.content_etag.as_deref() != Some(content_etag.as_str())
                || StrongEtag::parse(&node.node_etag).is_err()
                || hash.len() != 64
                || !hash
                    .bytes()
                    .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
                || node
                    .size
                    .as_deref()
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_none()
            {
                return Err(StoreError::UnsupportedFormat);
            }
        }
        OperationReceipt::MetadataPreconditionFailed {
            current_meta_rev,
            current_meta_etag,
            ..
        } if (StrongEtag::from_meta_revision(current_meta_rev)
            .map_err(|_| StoreError::UnsupportedFormat)?
            .as_str()
            != current_meta_etag
            || current_meta_etag == &item.expected_meta_etag) =>
        {
            return Err(StoreError::UnsupportedFormat);
        }
        _ => {}
    }
    Ok(receipt)
}

fn matches_empty_created_node(
    request: &notryx_protocol::CreateNodeRequest,
    node: &notryx_protocol::NodeState,
    commit_seq: notryx_domain::ChangeSeq,
) -> bool {
    use notryx_protocol::{NodeKind, StrongEtag};
    if node.node_id != request.node_id
        || node.parent_id != Some(request.parent_id)
        || node.name != request.name
        || node.kind != request.kind
        || node.deleted
        || node.children_generation.get() != 0
        || commit_seq.get() == 0
        || node.last_change_seq != commit_seq
        || StrongEtag::from_meta_revision(&node.meta_rev)
            .ok()
            .as_ref()
            .map(StrongEtag::as_str)
            != Some(node.meta_etag.as_str())
        || StrongEtag::parse(&node.node_etag).is_err()
    {
        return false;
    }
    match request.kind {
        NodeKind::Folder => {
            node.content_rev.is_none()
                && node.content_etag.is_none()
                && node.content_hash.is_none()
                && node.size.is_none()
        }
        NodeKind::File => {
            let etag = node
                .content_rev
                .as_ref()
                .and_then(|rev| StrongEtag::from_content_revision(rev).ok());
            etag.as_ref().map(StrongEtag::as_str) == node.content_etag.as_deref()
                && node.content_rev.is_some()
                && node.content_hash.as_deref()
                    == Some(
                        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                    )
                && node
                    .size
                    .as_deref()
                    .and_then(|size| size.parse::<u64>().ok())
                    == Some(0)
        }
    }
}

fn create_receipt_for_display(
    item: &CreateOutboxOperation,
    raw: &str,
    terminal: Option<&str>,
) -> Result<notryx_protocol::OperationReceipt, StoreError> {
    use notryx_protocol::OperationReceipt;
    if raw.len() > 65536 {
        return Err(StoreError::UnsupportedFormat);
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| StoreError::UnsupportedFormat)?;
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::UnsupportedFormat)?;
    if value
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
        != Some(item.operation_id.as_str())
        || value
            .get("operation_seq")
            .and_then(serde_json::Value::as_str)
            .and_then(|seq| seq.parse::<u64>().ok())
            != Some(item.operation_seq)
        || value
            .get("workspace_epoch")
            .and_then(serde_json::Value::as_str)
            != Some(item.binding.workspace_epoch.as_str())
        || status != terminal.unwrap_or("created")
        || !matches!(
            status,
            "created"
                | "recovery_conflict"
                | "node_deleted"
                | "destination_exists"
                | "node_id_conflict"
                | "parent_not_folder"
                | "not_found"
                | "not_applied"
                | "abandoned"
        )
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let receipt: OperationReceipt =
        serde_json::from_value(value).map_err(|_| StoreError::UnsupportedFormat)?;
    if let OperationReceipt::Created {
        node, commit_seq, ..
    } = &receipt
        && !matches_empty_created_node(&item.request, node, *commit_seq)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(receipt)
}

pub(crate) fn validated_create_record(
    row: &SqliteRow,
    binding: &RegistrationBinding,
) -> Result<CreateOutboxOperation, StoreError> {
    let mut item = pending_create_operation(row, binding)?;
    let actual: String = row.try_get("actual_status").map_err(storage_error)?;
    let terminal: Option<String> = row.try_get("actual_terminal").map_err(storage_error)?;
    let receipt: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
    match (actual.as_str(), terminal.as_deref(), receipt.as_deref()) {
        ("queued" | "sent", None, None) => {}
        ("committed", None, Some(raw)) => {
            create_receipt_for_display(&item, raw, None)?;
            item.status = OutboxStatus::Committed;
        }
        ("committed", Some("abandoned"), Some(raw))
            if item.abandon_state == OutboxAbandonState::Sent =>
        {
            create_receipt_for_display(&item, raw, Some("abandoned"))?;
            item.status = OutboxStatus::Abandoned;
        }
        ("conflict", Some(kind), Some(raw)) if item.abandon_state == OutboxAbandonState::None => {
            create_receipt_for_display(&item, raw, Some(kind))?;
            item.status = OutboxStatus::Failed;
        }
        _ => return Err(StoreError::UnsupportedFormat),
    }
    Ok(item)
}

fn validated_delete_record(
    row: &SqliteRow,
    binding: &RegistrationBinding,
) -> Result<DeleteOutboxOperation, StoreError> {
    let mut item = pending_delete_operation(row, binding)?;
    let actual: String = row.try_get("actual_status").map_err(storage_error)?;
    let terminal: Option<String> = row.try_get("actual_terminal").map_err(storage_error)?;
    let receipt: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
    match (actual.as_str(), terminal.as_deref(), receipt.as_deref()) {
        ("queued" | "sent", None, None) => {}
        ("committed", None, Some(raw)) => {
            delete_receipt_for_display(&item, raw, None)?;
            item.status = OutboxStatus::Committed;
        }
        ("committed", Some("abandoned"), Some(raw))
            if item.abandon_state == OutboxAbandonState::Sent =>
        {
            delete_receipt_for_display(&item, raw, Some("abandoned"))?;
            item.status = OutboxStatus::Abandoned;
        }
        ("conflict", Some(kind), Some(raw)) => {
            delete_receipt_for_display(&item, raw, Some(kind))?;
            item.status = OutboxStatus::Failed;
        }
        _ => return Err(StoreError::UnsupportedFormat),
    }
    Ok(item)
}

fn validated_move_record(
    row: &SqliteRow,
    binding: &RegistrationBinding,
) -> Result<MoveOutboxOperation, StoreError> {
    let mut item = pending_move_operation(row, binding)?;
    let actual: String = row.try_get("actual_status").map_err(storage_error)?;
    let terminal: Option<String> = row.try_get("actual_terminal").map_err(storage_error)?;
    let receipt: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
    match (actual.as_str(), terminal.as_deref(), receipt.as_deref()) {
        ("queued" | "sent", None, None) => {}
        ("committed", None, Some(raw)) => {
            move_receipt_for_display(&item, raw, None)?;
            item.status = OutboxStatus::Committed;
        }
        ("committed", Some("abandoned"), Some(raw))
            if item.abandon_state == OutboxAbandonState::Sent =>
        {
            move_receipt_for_display(&item, raw, Some("abandoned"))?;
            item.status = OutboxStatus::Abandoned;
        }
        ("conflict", Some(kind), Some(raw)) => {
            move_receipt_for_display(&item, raw, Some(kind))?;
            item.status = OutboxStatus::Failed;
        }
        _ => return Err(StoreError::UnsupportedFormat),
    }
    Ok(item)
}

fn verify_failed_move_ack(row: &SqliteRow, item: &MoveOutboxOperation) -> Result<(), StoreError> {
    if item.status == OutboxStatus::Failed {
        let ack: Option<i64> = row.try_get("saved_ack").map_err(storage_error)?;
        if ack
            .and_then(|value| u64::try_from(value).ok())
            .is_none_or(|value| value < item.operation_seq)
        {
            return Err(StoreError::UnsupportedFormat);
        }
    }
    Ok(())
}

fn restore_receipt_for_display(
    item: &RestoreOutboxOperation,
    raw: &str,
    terminal: Option<&str>,
) -> Result<notryx_protocol::OperationReceipt, StoreError> {
    use notryx_protocol::{NodeKind, OperationReceipt, StrongEtag};
    if raw.len() > 65536 {
        return Err(StoreError::UnsupportedFormat);
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| StoreError::UnsupportedFormat)?;
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::UnsupportedFormat)?;
    if value
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
        != Some(item.operation_id.as_str())
        || value
            .get("operation_seq")
            .and_then(serde_json::Value::as_str)
            .and_then(|seq| seq.parse::<u64>().ok())
            != Some(item.operation_seq)
        || value
            .get("workspace_epoch")
            .and_then(serde_json::Value::as_str)
            != Some(item.binding.workspace_epoch.as_str())
        || status != terminal.unwrap_or("restored")
        || !matches!(
            status,
            "restored"
                | "recovery_conflict"
                | "node_deleted"
                | "not_deleted"
                | "history_expired"
                | "metadata_precondition_failed"
                | "destination_exists"
                | "parent_not_folder"
                | "not_found"
                | "not_applied"
                | "abandoned"
        )
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let receipt: OperationReceipt =
        serde_json::from_value(value).map_err(|_| StoreError::UnsupportedFormat)?;
    match &receipt {
        OperationReceipt::Restored {
            node, commit_seq, ..
        } => {
            let meta = StrongEtag::from_meta_revision(&node.meta_rev)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            if node.node_id != item.node_id
                || node.parent_id != Some(item.request.parent_id)
                || node.name != item.request.name
                || node.deleted
                || commit_seq.get() == 0
                || node.last_change_seq != *commit_seq
                || node.meta_rev == item.request.expected_meta_rev
                || meta.as_str() != node.meta_etag
                || StrongEtag::parse(&node.node_etag).is_err()
            {
                return Err(StoreError::UnsupportedFormat);
            }
            if node.kind == NodeKind::File {
                let revision = node
                    .content_rev
                    .as_ref()
                    .ok_or(StoreError::UnsupportedFormat)?;
                let etag = StrongEtag::from_content_revision(revision)
                    .map_err(|_| StoreError::UnsupportedFormat)?;
                let hash = node
                    .content_hash
                    .as_deref()
                    .and_then(|value| value.strip_prefix("sha256:"))
                    .ok_or(StoreError::UnsupportedFormat)?;
                if node.content_etag.as_deref() != Some(etag.as_str())
                    || hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    || node
                        .size
                        .as_deref()
                        .and_then(|size| size.parse::<u64>().ok())
                        .is_none()
                {
                    return Err(StoreError::UnsupportedFormat);
                }
            }
        }
        OperationReceipt::MetadataPreconditionFailed {
            current_meta_rev,
            current_meta_etag,
            ..
        } if StrongEtag::from_meta_revision(current_meta_rev)
            .map_err(|_| StoreError::UnsupportedFormat)?
            .as_str()
            != current_meta_etag =>
        {
            return Err(StoreError::UnsupportedFormat);
        }
        _ => {}
    }
    Ok(receipt)
}

pub(crate) async fn queue_create_on_connection(
    connection: &mut SqliteConnection,
    registration_id: i64,
    binding: &RegistrationBinding,
    request: &notryx_protocol::CreateNodeRequest,
    id: &str,
) -> Result<CreateOutboxOperation, StoreError> {
    let bytes = serde_json::to_vec(request).map_err(|_| StoreError::InvalidRequest)?;
    if request.upload_id.is_some()
        || request.parent_id == request.node_id
        || serde_json::from_slice::<notryx_protocol::CreateNodeRequest>(&bytes)
            .ok()
            .as_ref()
            != Some(request)
    {
        return Err(StoreError::InvalidRequest);
    }
    let old_create: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox x JOIN client_registrations r ON r.id=x.registration_id WHERE r.status='frozen' AND r.workspace_id=? AND x.operation_kind='create' AND x.document_id=?)")
            .bind(&binding.workspace_id).bind(request.node_id.to_string()).fetch_one(&mut *connection).await.map_err(storage_error)?;
    if old_create != 0 {
        return Err(StoreError::ReconciliationRequired);
    }
    if requires_reconciliation(
        &mut *connection,
        &binding.workspace_id,
        &request.node_id.to_string(),
    )
    .await?
    {
        return Err(StoreError::ReconciliationRequired);
    }
    let parent: Option<String> =
        sqlx::query_scalar("SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?")
            .bind(registration_id)
            .bind(request.parent_id.to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?;
    let parent: notryx_protocol::NodeState =
        serde_json::from_str(&parent.ok_or(StoreError::InvalidRequest)?)
            .map_err(|_| StoreError::UnsupportedFormat)?;
    if parent.node_id != request.parent_id
        || parent.deleted
        || parent.kind != notryx_protocol::NodeKind::Folder
    {
        return Err(StoreError::InvalidRequest);
    }
    let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM remote_nodes WHERE registration_id=? AND node_id=? UNION ALL SELECT 1 FROM client_outbox WHERE registration_id=? AND operation_kind='create' AND document_id=?)")
            .bind(registration_id).bind(request.node_id.to_string()).bind(registration_id).bind(request.node_id.to_string())
            .fetch_one(&mut *connection).await.map_err(storage_error)?;
    if exists != 0 {
        return Err(StoreError::InvalidRequest);
    }
    sqlx::query("INSERT INTO outbox_streams (registration_id, next_seq, ack_through) VALUES (?,1,0) ON CONFLICT(registration_id) DO NOTHING")
            .bind(registration_id).execute(&mut *connection).await.map_err(storage_error)?;
    let sequence: i64 =
        sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration_id)
            .fetch_one(&mut *connection)
            .await
            .map_err(storage_error)?;
    let next = sequence
        .checked_add(1)
        .ok_or(StoreError::InvalidGeneration)?;
    sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
        .bind(next)
        .bind(registration_id)
        .execute(&mut *connection)
        .await
        .map_err(storage_error)?;
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,bytes,payload_hash,status,payload_len) VALUES (?,?,?,'create',?,?,?,'queued',?)")
            .bind(id).bind(registration_id).bind(sequence).bind(request.node_id.to_string())
            .bind(&bytes).bind(payload_hash(&bytes)).bind(bytes.len() as i64)
            .execute(&mut *connection).await.map_err(storage_error)?;
    let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id=?")
        .bind(id)
        .fetch_one(&mut *connection)
        .await
        .map_err(storage_error)?;
    let operation = pending_create_operation(&row, binding)?;
    Ok(operation)
}

impl ClientStore {
    pub async fn list_frozen_moves(
        &mut self,
        current: &RegistrationBinding,
        after: Option<&str>,
        limit: u16,
    ) -> Result<FrozenMovePage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        if let Some(after) = after {
            let parsed: notryx_domain::OperationId =
                after.parse().map_err(|_| StoreError::InvalidRequest)?;
            if parsed.to_string() != after {
                return Err(StoreError::InvalidRequest);
            }
        }
        self.registration_id(current, true).await?;
        let mut rows = sqlx::query("SELECT x.operation_id,x.operation_seq,x.operation_kind,x.document_id,x.storage_id,x.generation,x.dependency,x.expected_revision,x.bytes,x.payload_hash,x.payload_len,x.payload_retired,CASE WHEN x.status='conflict' THEN 'sent' ELSE x.status END AS status,x.abandon_state,NULL AS terminal_kind,x.status AS actual_status,x.terminal_kind AS actual_terminal,x.receipt_json,(SELECT s.ack_through FROM outbox_streams s WHERE s.registration_id=x.registration_id) AS saved_ack,r.server_origin,r.workspace_id,r.workspace_epoch,r.device_id,r.device_stream_id FROM client_outbox x JOIN client_registrations r ON r.id=x.registration_id WHERE r.status='frozen' AND r.workspace_id=? AND x.operation_kind='move' AND x.status IN ('queued','sent','conflict') AND x.operation_id>? AND NOT EXISTS(SELECT 1 FROM move_reconciliations d WHERE d.old_operation_id=x.operation_id) ORDER BY x.operation_id LIMIT ?")
            .bind(&current.workspace_id).bind(after.unwrap_or("")).bind(i64::from(limit)+1)
            .fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let binding = super::registration::binding_from_row(&row)?;
            let item = validated_move_record(&row, &binding)?;
            verify_failed_move_ack(&row, &item)?;
            let receipt = if item.status == OutboxStatus::Failed {
                let raw: String = row.try_get("receipt_json").map_err(storage_error)?;
                Some(serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?)
            } else {
                None
            };
            entries.push(MoveOutboxState {
                operation: item,
                receipt,
                frozen: true,
            });
        }
        let next_after_id = if has_more {
            entries
                .last()
                .map(|entry| entry.operation.operation_id.clone())
        } else {
            None
        };
        Ok(FrozenMovePage {
            entries,
            next_after_id,
        })
    }
    pub async fn resolve_frozen_move(
        &mut self,
        old_binding: &RegistrationBinding,
        current_binding: &RegistrationBinding,
        operation_id: &str,
        choice: &FrozenMoveChoice,
    ) -> Result<(), StoreError> {
        if old_binding.workspace_id != current_binding.workspace_id
            || old_binding == current_binding
        {
            return Err(StoreError::InvalidRegistration);
        }
        let old_registration = self.registration_id(old_binding, false).await?;
        let current_registration = self.registration_id(current_binding, true).await?;
        let parsed: notryx_domain::OperationId = operation_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        if parsed.to_string() != operation_id {
            return Err(StoreError::InvalidRequest);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let current_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(current_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let old_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(old_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if current_status != "active" || old_status != "frozen" {
            return Err(StoreError::StaleRegistration);
        }
        let row = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json,(SELECT s.ack_through FROM outbox_streams s WHERE s.registration_id=client_outbox.registration_id) AS saved_ack FROM client_outbox WHERE registration_id=? AND operation_id=? AND operation_kind='move'")
            .bind(old_registration).bind(operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let old = validated_move_record(&row, old_binding)?;
        verify_failed_move_ack(&row, &old)?;
        if !matches!(
            old.status,
            OutboxStatus::Queued
                | OutboxStatus::Sent
                | OutboxStatus::Abandoning
                | OutboxStatus::Failed
        ) {
            return Err(StoreError::InvalidRequest);
        }
        let dismiss = matches!(choice, FrozenMoveChoice::Dismiss);
        let sequence =
            i64::try_from(old.operation_seq).map_err(|_| StoreError::InvalidGeneration)?;
        let dependent: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE dependency=?) OR EXISTS(SELECT 1 FROM client_outbox x WHERE x.registration_id=? AND x.document_id=? AND x.operation_id<>? AND x.status IN ('queued','sent','conflict') AND NOT (x.operation_kind='move' AND EXISTS(SELECT 1 FROM move_reconciliations d WHERE d.old_operation_id=x.operation_id)) AND NOT (x.operation_kind='delete' AND EXISTS(SELECT 1 FROM delete_reconciliations d WHERE d.old_operation_id=x.operation_id)) AND NOT (x.operation_kind='create' AND EXISTS(SELECT 1 FROM create_reconciliations d WHERE d.old_operation_id=x.operation_id)) AND (x.operation_kind<>'move' OR ?=0 OR x.operation_seq<?))")
            .bind(operation_id).bind(old_registration).bind(old.node_id.to_string()).bind(operation_id).bind(dismiss).bind(sequence).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if dependent != 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        if let Some(row) = sqlx::query(
            "SELECT decision,new_operation_id FROM move_reconciliations WHERE old_operation_id=?",
        )
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?
        {
            let decision: String = row.try_get("decision").map_err(storage_error)?;
            let new_id: Option<String> = row.try_get("new_operation_id").map_err(storage_error)?;
            match (choice, decision.as_str(), new_id) {
                (FrozenMoveChoice::Dismiss, "dismissed", None) => {}
                (
                    FrozenMoveChoice::Apply {
                        expected_meta_etag,
                        request,
                    },
                    "applied",
                    Some(new_id),
                ) => {
                    let row = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=? AND operation_kind='move'")
                        .bind(new_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
                    let registration: i64 =
                        row.try_get("registration_id").map_err(storage_error)?;
                    let new = validated_move_record(&row, current_binding)?;
                    if registration != current_registration
                        || new.node_id != old.node_id
                        || &new.expected_meta_etag != expected_meta_etag
                        || &new.request != request
                    {
                        return Err(StoreError::InvalidRequest);
                    }
                }
                _ => return Err(StoreError::InvalidRequest),
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        match choice {
            FrozenMoveChoice::Dismiss => {
                sqlx::query("INSERT INTO move_reconciliations VALUES (?,'dismissed',NULL)")
                    .bind(operation_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            FrozenMoveChoice::Apply {
                expected_meta_etag,
                request,
            } => {
                if expected_meta_etag.len() > 256
                    || notryx_protocol::MetadataPrecondition::parse(expected_meta_etag)
                        .and_then(|p| p.revision())
                        .is_err()
                    || !request.name.to_ascii_lowercase().ends_with(".md")
                    || request.parent_id == old.node_id
                    || request.expected_parent_id == old.node_id
                {
                    return Err(StoreError::InvalidRequest);
                }
                let bytes = serde_json::to_vec(request).map_err(|_| StoreError::InvalidRequest)?;
                if serde_json::from_slice::<notryx_protocol::MoveNodeRequest>(&bytes)
                    .ok()
                    .as_ref()
                    != Some(request)
                {
                    return Err(StoreError::InvalidRequest);
                }
                let ready: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM remote_index WHERE registration_id=?) AND NOT EXISTS(SELECT 1 FROM sync_bootstrap WHERE registration_id=?)").bind(current_registration).bind(current_registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
                if ready == 0 {
                    return Err(StoreError::InvalidRequest);
                }
                let node_id = old.node_id.to_string();
                let raw: Option<String> = sqlx::query_scalar(
                    "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
                )
                .bind(current_registration)
                .bind(&node_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
                let current: notryx_protocol::NodeState =
                    serde_json::from_str(&raw.ok_or(StoreError::InvalidRequest)?)
                        .map_err(|_| StoreError::UnsupportedFormat)?;
                if notryx_protocol::StrongEtag::from_meta_revision(&current.meta_rev)
                    .map_err(|_| StoreError::UnsupportedFormat)?
                    .as_str()
                    != current.meta_etag
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                if current.node_id != old.node_id
                    || current.deleted
                    || current.kind != notryx_protocol::NodeKind::File
                    || current.meta_etag != *expected_meta_etag
                    || current.parent_id != Some(request.expected_parent_id)
                {
                    return Err(StoreError::InvalidRequest);
                }
                let raw: Option<String> = sqlx::query_scalar(
                    "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
                )
                .bind(current_registration)
                .bind(request.parent_id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
                let parent: notryx_protocol::NodeState =
                    serde_json::from_str(&raw.ok_or(StoreError::InvalidRequest)?)
                        .map_err(|_| StoreError::UnsupportedFormat)?;
                if parent.node_id != request.parent_id
                    || parent.deleted
                    || parent.kind != notryx_protocol::NodeKind::Folder
                {
                    return Err(StoreError::InvalidRequest);
                }
                sqlx::query("INSERT INTO move_reconciliations VALUES (?,'dismissed',NULL)")
                    .bind(operation_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                if requires_reconciliation(&mut tx, &current_binding.workspace_id, &node_id).await?
                {
                    return Err(StoreError::ReconciliationRequired);
                }
                let structural: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind IN ('move','delete','restore') AND status IN ('queued','sent'))")
                    .bind(current_registration).bind(&node_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
                if structural != 0 {
                    return Err(StoreError::OutboxBlocked);
                }
                sqlx::query("INSERT INTO outbox_streams (registration_id,next_seq,ack_through) VALUES (?,1,0) ON CONFLICT(registration_id) DO NOTHING").bind(current_registration).execute(&mut *tx).await.map_err(storage_error)?;
                let seq: i64 = sqlx::query_scalar(
                    "SELECT next_seq FROM outbox_streams WHERE registration_id=?",
                )
                .bind(current_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
                let next = seq.checked_add(1).ok_or(StoreError::InvalidGeneration)?;
                sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
                    .bind(next)
                    .bind(current_registration)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                let new_id = uuid::Uuid::now_v7().to_string();
                sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,?,?,'move',?,?,?,?,'queued',?)")
                    .bind(&new_id).bind(current_registration).bind(seq).bind(&node_id).bind(expected_meta_etag).bind(&bytes).bind(payload_hash(&bytes)).bind(bytes.len() as i64).execute(&mut *tx).await.map_err(storage_error)?;
                sqlx::query("UPDATE move_reconciliations SET decision='applied',new_operation_id=? WHERE old_operation_id=?").bind(new_id).bind(operation_id).execute(&mut *tx).await.map_err(storage_error)?;
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn request_move_abandon_matching(
        &mut self,
        binding: &RegistrationBinding,
        expected: &MoveOutboxOperation,
    ) -> Result<bool, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND status IN ('queued','sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        let PendingOutboxOperation::Move(item) = pending_operation(&row, binding)? else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        if item != *expected {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        }
        if item.abandon_state == OutboxAbandonState::None {
            let changed = sqlx::query("UPDATE client_outbox SET abandon_state='requested' WHERE registration_id=? AND operation_id=? AND abandon_state='none'")
                .bind(registration_id).bind(&item.operation_id).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
            if changed != 1 {
                return Err(StoreError::Storage);
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    pub async fn queue_move(
        &mut self,
        binding: &RegistrationBinding,
        node_id: notryx_domain::NodeId,
        expected_meta_etag: &str,
        request: &notryx_protocol::MoveNodeRequest,
    ) -> Result<MoveOutboxOperation, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        if expected_meta_etag.len() > 256
            || notryx_protocol::MetadataPrecondition::parse(expected_meta_etag)
                .and_then(|p| p.revision())
                .is_err()
        {
            return Err(StoreError::InvalidRequest);
        }
        let bytes = serde_json::to_vec(request).map_err(|_| StoreError::InvalidRequest)?;
        if !request.name.to_ascii_lowercase().ends_with(".md")
            || request.parent_id == node_id
            || request.expected_parent_id == node_id
            || serde_json::from_slice::<notryx_protocol::MoveNodeRequest>(&bytes)
                .ok()
                .as_ref()
                != Some(request)
        {
            return Err(StoreError::InvalidRequest);
        }
        let node = node_id.to_string();
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        if requires_reconciliation(&mut tx, &binding.workspace_id, &node).await? {
            return Err(StoreError::ReconciliationRequired);
        }
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration_id)
        .bind(&node)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let current: notryx_protocol::NodeState =
            serde_json::from_str(&raw.ok_or(StoreError::InvalidRequest)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if notryx_protocol::StrongEtag::from_meta_revision(&current.meta_rev)
            .map_err(|_| StoreError::UnsupportedFormat)?
            .as_str()
            != current.meta_etag
        {
            return Err(StoreError::UnsupportedFormat);
        }
        if current.node_id != node_id
            || current.deleted
            || current.kind != notryx_protocol::NodeKind::File
            || current.meta_etag != expected_meta_etag
            || current.parent_id != Some(request.expected_parent_id)
        {
            return Err(StoreError::InvalidRequest);
        }
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration_id)
        .bind(request.parent_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let parent: notryx_protocol::NodeState =
            serde_json::from_str(&raw.ok_or(StoreError::InvalidRequest)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if parent.node_id != request.parent_id
            || parent.deleted
            || parent.kind != notryx_protocol::NodeKind::Folder
        {
            return Err(StoreError::InvalidRequest);
        }
        let structural: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind IN ('move','delete','restore') AND status IN ('queued','sent'))")
            .bind(registration_id).bind(&node).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if structural != 0 {
            return Err(StoreError::OutboxBlocked);
        }
        sqlx::query("INSERT INTO outbox_streams (registration_id,next_seq,ack_through) VALUES (?,1,0) ON CONFLICT(registration_id) DO NOTHING").bind(registration_id).execute(&mut *tx).await.map_err(storage_error)?;
        let sequence: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(registration_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let next = sequence
            .checked_add(1)
            .ok_or(StoreError::InvalidGeneration)?;
        sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
            .bind(next)
            .bind(registration_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,?,?,'move',?,?,?,?,'queued',?)")
            .bind(&id).bind(registration_id).bind(sequence).bind(&node).bind(expected_meta_etag).bind(&bytes).bind(payload_hash(&bytes)).bind(bytes.len() as i64).execute(&mut *tx).await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id=?")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
        let operation = pending_move_operation(&row, binding)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(operation)
    }

    pub async fn apply_move_receipt(
        &mut self,
        binding: &RegistrationBinding,
        receipt: &notryx_protocol::OperationReceipt,
    ) -> Result<(), StoreError> {
        use notryx_protocol::{NodeKind, OperationReceipt, StrongEtag};
        let registration_id = self.registration_id(binding, true).await?;
        let json = serde_json::to_string(receipt).map_err(|_| StoreError::InvalidReceipt)?;
        if json.len() > 65536
            || serde_json::from_str::<OperationReceipt>(&json)
                .ok()
                .as_ref()
                != Some(receipt)
        {
            return Err(StoreError::InvalidReceipt);
        }
        let (id, seq, epoch, terminal) = match receipt {
            OperationReceipt::Moved {
                operation_id,
                operation_seq,
                workspace_epoch,
                ..
            } => (operation_id, operation_seq, workspace_epoch, None),
            OperationReceipt::MetadataPreconditionFailed {
                operation_id,
                operation_seq,
                workspace_epoch,
                ..
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("metadata_precondition_failed"),
            ),
            OperationReceipt::ExpectedParentMismatch {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("expected_parent_mismatch"),
            ),
            OperationReceipt::DestinationExists {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("destination_exists"),
            ),
            OperationReceipt::NodeDeleted {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("node_deleted"),
            ),
            OperationReceipt::ParentNotFolder {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("parent_not_folder"),
            ),
            OperationReceipt::NotFound {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_found"),
            ),
            OperationReceipt::NotApplied {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_applied"),
            ),
            OperationReceipt::RecoveryConflict {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("recovery_conflict"),
            ),
            OperationReceipt::Abandoned {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("abandoned"),
            ),
            _ => return Err(StoreError::InvalidReceipt),
        };
        if epoch.to_string() != binding.workspace_epoch {
            return Err(StoreError::InvalidReceipt);
        }
        let sequence = i64::try_from(seq.get()).map_err(|_| StoreError::InvalidReceipt)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row =
            sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration_id)
                .bind(id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?
                .ok_or(StoreError::InvalidReceipt)?;
        if row
            .try_get::<String, _>("operation_kind")
            .map_err(storage_error)?
            != "move"
            || row
                .try_get::<i64, _>("operation_seq")
                .map_err(storage_error)?
                != sequence
        {
            return Err(StoreError::InvalidReceipt);
        }
        let stored: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
        if let Some(stored) = stored {
            if serde_json::from_str::<OperationReceipt>(&stored)
                .ok()
                .as_ref()
                != Some(receipt)
            {
                return Err(StoreError::InvalidReceipt);
            }
            let ack: i64 = sqlx::query_scalar(
                "SELECT ack_through FROM outbox_streams WHERE registration_id=?",
            )
            .bind(registration_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
            if ack < sequence {
                return Err(StoreError::InvalidReceipt);
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        let item = pending_move_operation(&row, binding)?;
        if !matches!(item.status, OutboxStatus::Sent | OutboxStatus::Abandoning)
            || (terminal == Some("abandoned") && item.abandon_state != OutboxAbandonState::Sent)
        {
            return Err(StoreError::InvalidReceipt);
        }
        match receipt {
            OperationReceipt::Moved {
                node, commit_seq, ..
            } => {
                let content_revision = node
                    .content_rev
                    .as_ref()
                    .ok_or(StoreError::InvalidReceipt)?;
                let content_etag = StrongEtag::from_content_revision(content_revision)
                    .map_err(|_| StoreError::InvalidReceipt)?;
                let meta_etag = StrongEtag::from_meta_revision(&node.meta_rev)
                    .map_err(|_| StoreError::InvalidReceipt)?;
                let hash = node
                    .content_hash
                    .as_deref()
                    .and_then(|v| v.strip_prefix("sha256:"))
                    .ok_or(StoreError::InvalidReceipt)?;
                if node.node_id != item.node_id
                    || node.kind != NodeKind::File
                    || node.deleted
                    || node.parent_id != Some(item.request.parent_id)
                    || node.name != item.request.name
                    || (item.request.parent_id != item.request.expected_parent_id
                        && node.meta_etag == item.expected_meta_etag)
                    || commit_seq.get() == 0
                    || node.last_change_seq != *commit_seq
                    || node.meta_etag != meta_etag.as_str()
                    || node.content_etag.as_deref() != Some(content_etag.as_str())
                    || StrongEtag::parse(&node.node_etag).is_err()
                    || hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
                    || node
                        .size
                        .as_deref()
                        .and_then(|v| v.parse::<u64>().ok())
                        .is_none()
                {
                    return Err(StoreError::InvalidReceipt);
                }
                if move_receipt_conflicts_with_cached_source(&mut tx, registration_id, &item, node)
                    .await?
                {
                    return Err(StoreError::InvalidReceipt);
                }
            }
            OperationReceipt::MetadataPreconditionFailed {
                current_meta_rev,
                current_meta_etag,
                ..
            } if (StrongEtag::from_meta_revision(current_meta_rev)
                .map_err(|_| StoreError::InvalidReceipt)?
                .as_str()
                != current_meta_etag
                || current_meta_etag == &item.expected_meta_etag) =>
            {
                return Err(StoreError::InvalidReceipt);
            }
            _ => {}
        }
        let ack: i64 =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id=?")
                .bind(registration_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if ack.checked_add(1) != Some(sequence) {
            return Err(StoreError::InvalidReceipt);
        }
        let changed = sqlx::query("UPDATE client_outbox SET status=?,terminal_kind=?,receipt_json=? WHERE registration_id=? AND operation_id=? AND status IN ('queued','sent')")
            .bind(if terminal.is_none() || terminal == Some("abandoned") { "committed" } else { "conflict" }).bind(terminal).bind(&json).bind(registration_id).bind(id.to_string())
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::Storage);
        }
        sqlx::query("UPDATE outbox_streams SET ack_through=? WHERE registration_id=?")
            .bind(sequence)
            .bind(registration_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_frozen_deletes(
        &mut self,
        current: &RegistrationBinding,
        after: Option<&str>,
        limit: u16,
    ) -> Result<FrozenDeletePage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        if let Some(after) = after {
            let parsed: notryx_domain::OperationId =
                after.parse().map_err(|_| StoreError::InvalidRequest)?;
            if parsed.to_string() != after {
                return Err(StoreError::InvalidRequest);
            }
        }
        self.registration_id(current, true).await?;
        let mut rows = sqlx::query("SELECT x.operation_id,x.operation_seq,x.operation_kind,x.document_id,x.storage_id,x.generation,x.dependency,x.expected_revision,x.bytes,x.payload_hash,x.payload_len,x.payload_retired,CASE WHEN x.status='conflict' THEN 'sent' ELSE x.status END AS status,x.abandon_state,NULL AS terminal_kind,x.status AS actual_status,x.terminal_kind AS actual_terminal,x.receipt_json,r.server_origin,r.workspace_id,r.workspace_epoch,r.device_id,r.device_stream_id FROM client_outbox x JOIN client_registrations r ON r.id=x.registration_id WHERE r.status='frozen' AND r.workspace_id=? AND x.operation_kind='delete' AND x.status IN ('queued','sent','conflict') AND x.operation_id>? AND NOT EXISTS(SELECT 1 FROM delete_reconciliations d WHERE d.old_operation_id=x.operation_id) ORDER BY x.operation_id LIMIT ?")
            .bind(&current.workspace_id).bind(after.unwrap_or("")).bind(i64::from(limit)+1)
            .fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let binding = super::registration::binding_from_row(&row)?;
            let item = validated_delete_record(&row, &binding)?;
            let receipt = if item.status == OutboxStatus::Failed {
                let raw: String = row.try_get("receipt_json").map_err(storage_error)?;
                Some(serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?)
            } else {
                None
            };
            entries.push(DeleteOutboxState {
                operation: item,
                receipt,
                frozen: true,
            });
        }
        let next_after_id = if has_more {
            entries
                .last()
                .map(|entry| entry.operation.operation_id.clone())
        } else {
            None
        };
        Ok(FrozenDeletePage {
            entries,
            next_after_id,
        })
    }
    pub async fn resolve_frozen_delete(
        &mut self,
        old_binding: &RegistrationBinding,
        current_binding: &RegistrationBinding,
        operation_id: &str,
        choice: &FrozenDeleteChoice,
    ) -> Result<(), StoreError> {
        if old_binding.workspace_id != current_binding.workspace_id
            || old_binding == current_binding
        {
            return Err(StoreError::InvalidRegistration);
        }
        let old_registration = self.registration_id(old_binding, false).await?;
        let current_registration = self.registration_id(current_binding, true).await?;
        let parsed: notryx_domain::OperationId = operation_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        if parsed.to_string() != operation_id {
            return Err(StoreError::InvalidRequest);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let current_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(current_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let old_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(old_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if current_status != "active" || old_status != "frozen" {
            return Err(StoreError::StaleRegistration);
        }
        let row = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE registration_id=? AND operation_id=? AND operation_kind='delete'")
            .bind(old_registration).bind(operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let old = validated_delete_record(&row, old_binding)?;
        if !matches!(
            old.status,
            OutboxStatus::Queued
                | OutboxStatus::Sent
                | OutboxStatus::Abandoning
                | OutboxStatus::Failed
        ) {
            return Err(StoreError::InvalidRequest);
        }
        let dependent: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE dependency=?)")
                .bind(operation_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if dependent != 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        if let Some(row) = sqlx::query(
            "SELECT decision,new_operation_id FROM delete_reconciliations WHERE old_operation_id=?",
        )
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?
        {
            let decision: String = row.try_get("decision").map_err(storage_error)?;
            let new_id: Option<String> = row.try_get("new_operation_id").map_err(storage_error)?;
            match (choice, decision.as_str(), new_id) {
                (FrozenDeleteChoice::Dismiss, "dismissed", None) => {}
                (FrozenDeleteChoice::Apply { expected_node_etag }, "applied", Some(new_id)) => {
                    let new: Option<(i64, String, Option<String>)> = sqlx::query_as("SELECT registration_id,document_id,expected_revision FROM client_outbox WHERE operation_id=? AND operation_kind='delete'").bind(new_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                    if new
                        != Some((
                            current_registration,
                            old.node_id.to_string(),
                            Some(expected_node_etag.clone()),
                        ))
                    {
                        return Err(StoreError::InvalidRequest);
                    }
                }
                _ => return Err(StoreError::InvalidRequest),
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        match choice {
            FrozenDeleteChoice::Dismiss => {
                sqlx::query("INSERT INTO delete_reconciliations VALUES (?,'dismissed',NULL)")
                    .bind(operation_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            FrozenDeleteChoice::Apply { expected_node_etag } => {
                if expected_node_etag.len() > 256
                    || notryx_protocol::StrongEtag::parse(expected_node_etag).is_err()
                {
                    return Err(StoreError::InvalidRequest);
                }
                let ready: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM remote_index WHERE registration_id=?) AND NOT EXISTS(SELECT 1 FROM sync_bootstrap WHERE registration_id=?)")
                    .bind(current_registration).bind(current_registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
                if ready == 0 {
                    return Err(StoreError::InvalidRequest);
                }
                let node_id = old.node_id.to_string();
                let rows: Vec<String> = sqlx::query_scalar("SELECT d.storage_id FROM registered_documents d JOIN client_registrations r ON r.id=d.registration_id WHERE r.workspace_id=? AND d.document_id=?")
                    .bind(&current_binding.workspace_id).bind(&node_id).fetch_all(&mut *tx).await.map_err(storage_error)?;
                if rows.iter().any(|id| self.sessions.contains_key(id)) {
                    return Err(StoreError::OpenEditSessions);
                }
                let raw: Option<String> = sqlx::query_scalar(
                    "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
                )
                .bind(current_registration)
                .bind(&node_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
                let current: notryx_protocol::NodeState =
                    serde_json::from_str(&raw.ok_or(StoreError::InvalidRequest)?)
                        .map_err(|_| StoreError::UnsupportedFormat)?;
                if current.node_id != old.node_id
                    || current.deleted
                    || current.kind != notryx_protocol::NodeKind::File
                    || current.node_etag != *expected_node_etag
                {
                    return Err(StoreError::InvalidRequest);
                }
                sqlx::query("INSERT INTO delete_reconciliations VALUES (?,'dismissed',NULL)")
                    .bind(operation_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                if requires_reconciliation(&mut tx, &current_binding.workspace_id, &node_id).await?
                {
                    return Err(StoreError::ReconciliationRequired);
                }
                verify_prior_delete_conflicts(
                    &mut tx,
                    current_registration,
                    current_binding,
                    &node_id,
                )
                .await?;
                let dirty: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id LEFT JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.registration_id=? AND d.document_id=? AND (b.document_id IS NULL OR c.bytes<>b.bytes OR b.revision<>?)) OR EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND status IN ('queued','sent','conflict') AND NOT (operation_kind='delete' AND status='conflict' AND terminal_kind='node_precondition_failed'))")
                    .bind(current_registration).bind(&node_id).bind(current.content_rev.as_ref().map(|v| v.as_str())).bind(current_registration).bind(&node_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
                if dirty != 0 {
                    return Err(StoreError::OutboxBlocked);
                }
                sqlx::query("INSERT INTO outbox_streams (registration_id,next_seq,ack_through) VALUES (?,1,0) ON CONFLICT(registration_id) DO NOTHING").bind(current_registration).execute(&mut *tx).await.map_err(storage_error)?;
                let seq: i64 = sqlx::query_scalar(
                    "SELECT next_seq FROM outbox_streams WHERE registration_id=?",
                )
                .bind(current_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
                let next = seq.checked_add(1).ok_or(StoreError::InvalidGeneration)?;
                sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
                    .bind(next)
                    .bind(current_registration)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                let new_id = uuid::Uuid::now_v7().to_string();
                sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,?,?,'delete',?,?,?,?,'queued',0)")
                    .bind(&new_id).bind(current_registration).bind(seq).bind(&node_id).bind(expected_node_etag).bind(&b""[..]).bind(payload_hash(b""))
                    .execute(&mut *tx).await.map_err(storage_error)?;
                sqlx::query("UPDATE delete_reconciliations SET decision='applied',new_operation_id=? WHERE old_operation_id=?").bind(new_id).bind(operation_id).execute(&mut *tx).await.map_err(storage_error)?;
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_delete_operations(
        &mut self,
        binding: &RegistrationBinding,
        after: u64,
        limit: u16,
    ) -> Result<DeleteOutboxPage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        let after = i64::try_from(after).map_err(|_| StoreError::InvalidRequest)?;
        let registration_id = self.registration_id(binding, false).await?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(registration_id)
                .fetch_one(&mut self.connection)
                .await
                .map_err(storage_error)?;
        if !matches!(status.as_str(), "active" | "frozen") {
            return Err(StoreError::UnsupportedFormat);
        }
        let mut rows = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE registration_id=? AND operation_kind='delete' AND operation_seq>? ORDER BY operation_seq LIMIT ?")
            .bind(registration_id).bind(after).bind(i64::from(limit)+1).fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let mut operation = pending_delete_operation(&row, binding)?;
            let actual: String = row.try_get("actual_status").map_err(storage_error)?;
            let terminal: Option<String> = row.try_get("actual_terminal").map_err(storage_error)?;
            let raw: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
            let receipt = match (actual.as_str(), terminal.as_deref(), raw.as_deref()) {
                ("queued" | "sent", None, None) => None,
                ("committed", None, Some(raw)) => {
                    operation.status = OutboxStatus::Committed;
                    Some(delete_receipt_for_display(&operation, raw, None)?)
                }
                ("committed", Some("abandoned"), Some(raw))
                    if operation.abandon_state == OutboxAbandonState::Sent =>
                {
                    operation.status = OutboxStatus::Abandoned;
                    Some(delete_receipt_for_display(
                        &operation,
                        raw,
                        Some("abandoned"),
                    )?)
                }
                ("conflict", Some(kind), Some(raw)) => {
                    operation.status = OutboxStatus::Failed;
                    Some(delete_receipt_for_display(&operation, raw, Some(kind))?)
                }
                _ => return Err(StoreError::UnsupportedFormat),
            };
            entries.push(DeleteOutboxState {
                operation,
                receipt,
                frozen: status == "frozen",
            });
        }
        let next_after_seq = if has_more {
            entries.last().map(|entry| entry.operation.operation_seq)
        } else {
            None
        };
        Ok(DeleteOutboxPage {
            entries,
            next_after_seq,
        })
    }

    pub async fn list_move_operations(
        &mut self,
        binding: &RegistrationBinding,
        after: u64,
        limit: u16,
    ) -> Result<MoveOutboxPage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        let after = i64::try_from(after).map_err(|_| StoreError::InvalidRequest)?;
        let registration_id = self.registration_id(binding, false).await?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(registration_id)
                .fetch_one(&mut self.connection)
                .await
                .map_err(storage_error)?;
        if !matches!(status.as_str(), "active" | "frozen") {
            return Err(StoreError::UnsupportedFormat);
        }
        let mut rows = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE registration_id=? AND operation_kind='move' AND operation_seq>? ORDER BY operation_seq LIMIT ?")
            .bind(registration_id).bind(after).bind(i64::from(limit)+1).fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let mut operation = pending_move_operation(&row, binding)?;
            let actual: String = row.try_get("actual_status").map_err(storage_error)?;
            let terminal: Option<String> = row.try_get("actual_terminal").map_err(storage_error)?;
            let raw: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
            let receipt = match (actual.as_str(), terminal.as_deref(), raw.as_deref()) {
                ("queued" | "sent", None, None) => None,
                ("committed", None, Some(raw)) => {
                    operation.status = OutboxStatus::Committed;
                    Some(move_receipt_for_display(&operation, raw, None)?)
                }
                ("committed", Some("abandoned"), Some(raw))
                    if operation.abandon_state == OutboxAbandonState::Sent =>
                {
                    operation.status = OutboxStatus::Abandoned;
                    Some(move_receipt_for_display(
                        &operation,
                        raw,
                        Some("abandoned"),
                    )?)
                }
                ("conflict", Some(kind), Some(raw)) => {
                    operation.status = OutboxStatus::Failed;
                    Some(move_receipt_for_display(&operation, raw, Some(kind))?)
                }
                _ => return Err(StoreError::UnsupportedFormat),
            };
            if let Some(notryx_protocol::OperationReceipt::Moved { node, .. }) = &receipt
                && move_receipt_conflicts_with_cached_source(
                    &mut self.connection,
                    registration_id,
                    &operation,
                    node,
                )
                .await?
            {
                return Err(StoreError::UnsupportedFormat);
            }
            entries.push(MoveOutboxState {
                operation,
                receipt,
                frozen: status == "frozen",
            });
        }
        let next_after_seq = if has_more {
            entries.last().map(|entry| entry.operation.operation_seq)
        } else {
            None
        };
        Ok(MoveOutboxPage {
            entries,
            next_after_seq,
        })
    }

    pub async fn request_delete_abandon_matching(
        &mut self,
        binding: &RegistrationBinding,
        expected: &DeleteOutboxOperation,
    ) -> Result<bool, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND status IN ('queued','sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        let PendingOutboxOperation::Delete(item) = pending_operation(&row, binding)? else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        if item != *expected {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        }
        if item.abandon_state == OutboxAbandonState::None {
            let changed = sqlx::query("UPDATE client_outbox SET abandon_state='requested' WHERE registration_id=? AND operation_id=? AND abandon_state='none'")
                .bind(registration_id).bind(&item.operation_id).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
            if changed != 1 {
                return Err(StoreError::Storage);
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    pub async fn apply_delete_receipt(
        &mut self,
        binding: &RegistrationBinding,
        receipt: &notryx_protocol::OperationReceipt,
    ) -> Result<(), StoreError> {
        use notryx_protocol::OperationReceipt;
        let registration_id = self.registration_id(binding, true).await?;
        let json = serde_json::to_string(receipt).map_err(|_| StoreError::InvalidReceipt)?;
        if json.len() > 65536
            || serde_json::from_str::<OperationReceipt>(&json)
                .ok()
                .as_ref()
                != Some(receipt)
        {
            return Err(StoreError::InvalidReceipt);
        }
        let (id, seq, epoch, terminal) = match receipt {
            OperationReceipt::Deleted {
                operation_id,
                operation_seq,
                workspace_epoch,
                ..
            } => (operation_id, operation_seq, workspace_epoch, None),
            OperationReceipt::Abandoned {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("abandoned"),
            ),
            OperationReceipt::NodePreconditionFailed {
                operation_id,
                operation_seq,
                workspace_epoch,
                ..
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("node_precondition_failed"),
            ),
            OperationReceipt::NodeDeleted {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("node_deleted"),
            ),
            OperationReceipt::NotFound {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_found"),
            ),
            OperationReceipt::NotApplied {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_applied"),
            ),
            OperationReceipt::RecoveryConflict {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("recovery_conflict"),
            ),
            _ => return Err(StoreError::InvalidReceipt),
        };
        if epoch.to_string() != binding.workspace_epoch {
            return Err(StoreError::InvalidReceipt);
        }
        let sequence = i64::try_from(seq.get()).map_err(|_| StoreError::InvalidReceipt)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row =
            sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration_id)
                .bind(id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?
                .ok_or(StoreError::InvalidReceipt)?;
        if row
            .try_get::<String, _>("operation_kind")
            .map_err(storage_error)?
            != "delete"
            || row
                .try_get::<i64, _>("operation_seq")
                .map_err(storage_error)?
                != sequence
        {
            return Err(StoreError::InvalidReceipt);
        }
        let stored: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
        if let Some(stored) = stored {
            if serde_json::from_str::<OperationReceipt>(&stored)
                .ok()
                .as_ref()
                != Some(receipt)
            {
                return Err(StoreError::InvalidReceipt);
            }
            let ack: i64 = sqlx::query_scalar(
                "SELECT ack_through FROM outbox_streams WHERE registration_id=?",
            )
            .bind(registration_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
            if ack < sequence {
                return Err(StoreError::InvalidReceipt);
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        let item = pending_delete_operation(&row, binding)?;
        if !matches!(item.status, OutboxStatus::Sent | OutboxStatus::Abandoning)
            || (terminal == Some("abandoned") && item.abandon_state != OutboxAbandonState::Sent)
        {
            return Err(StoreError::InvalidReceipt);
        }
        match receipt {
            OperationReceipt::Deleted {
                node, commit_seq, ..
            } => {
                let content_revision = node
                    .content_rev
                    .as_ref()
                    .ok_or(StoreError::InvalidReceipt)?;
                let content_etag =
                    notryx_protocol::StrongEtag::from_content_revision(content_revision)
                        .map_err(|_| StoreError::InvalidReceipt)?;
                let meta_etag = notryx_protocol::StrongEtag::from_meta_revision(&node.meta_rev)
                    .map_err(|_| StoreError::InvalidReceipt)?;
                let hash = node
                    .content_hash
                    .as_deref()
                    .and_then(|v| v.strip_prefix("sha256:"))
                    .ok_or(StoreError::InvalidReceipt)?;
                if node.node_id != item.node_id
                    || node.kind != notryx_protocol::NodeKind::File
                    || !node.deleted
                    || node.parent_id.is_none()
                    || commit_seq.get() == 0
                    || node.last_change_seq != *commit_seq
                    || node.meta_etag != meta_etag.as_str()
                    || node.content_etag.as_deref() != Some(content_etag.as_str())
                    || notryx_protocol::StrongEtag::parse(&node.node_etag).is_err()
                    || hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
                    || node
                        .size
                        .as_deref()
                        .and_then(|v| v.parse::<u64>().ok())
                        .is_none()
                {
                    return Err(StoreError::InvalidReceipt);
                }
            }
            OperationReceipt::NodePreconditionFailed {
                current_node_etag, ..
            } if (current_node_etag.len() > 256
                || notryx_protocol::StrongEtag::parse(current_node_etag).is_err()
                || current_node_etag == &item.expected_node_etag) =>
            {
                return Err(StoreError::InvalidReceipt);
            }
            _ => {}
        }
        let ack: i64 =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id=?")
                .bind(registration_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if ack.checked_add(1) != Some(sequence) {
            return Err(StoreError::InvalidReceipt);
        }
        let changed = sqlx::query("UPDATE client_outbox SET status=?,terminal_kind=?,receipt_json=? WHERE registration_id=? AND operation_id=? AND status IN ('queued','sent')")
            .bind(if terminal.is_none() || terminal == Some("abandoned") { "committed" } else { "conflict" }).bind(terminal).bind(&json).bind(registration_id).bind(id.to_string())
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::Storage);
        }
        sqlx::query("UPDATE outbox_streams SET ack_through=? WHERE registration_id=?")
            .bind(sequence)
            .bind(registration_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn queue_delete(
        &mut self,
        binding: &RegistrationBinding,
        node_id: notryx_domain::NodeId,
        expected_node_etag: &str,
    ) -> Result<DeleteOutboxOperation, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        if expected_node_etag.len() > 256
            || notryx_protocol::StrongEtag::parse(expected_node_etag).is_err()
        {
            return Err(StoreError::InvalidRequest);
        }
        let node = node_id.to_string();
        let storage_ids: Vec<String> = sqlx::query_scalar("SELECT d.storage_id FROM registered_documents d JOIN client_registrations r ON r.id=d.registration_id WHERE r.workspace_id=? AND d.document_id=?")
            .bind(&binding.workspace_id).bind(&node).fetch_all(&mut self.connection).await.map_err(storage_error)?;
        if storage_ids.iter().any(|id| self.sessions.contains_key(id)) {
            return Err(StoreError::OpenEditSessions);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        if requires_reconciliation(&mut tx, &binding.workspace_id, &node).await? {
            return Err(StoreError::ReconciliationRequired);
        }
        let state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration_id)
        .bind(&node)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let current: notryx_protocol::NodeState =
            serde_json::from_str(&state.ok_or(StoreError::InvalidRequest)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if current.node_id != node_id
            || current.deleted
            || current.kind != notryx_protocol::NodeKind::File
            || current.node_etag != expected_node_etag
        {
            return Err(StoreError::InvalidRequest);
        }
        verify_prior_delete_conflicts(&mut tx, registration_id, binding, &node).await?;
        let unsafe_local: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id LEFT JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.registration_id=? AND d.document_id=? AND (b.document_id IS NULL OR c.bytes<>b.bytes OR b.revision<>?)) OR EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND status IN ('queued','sent','conflict') AND NOT (operation_kind='delete' AND status='conflict' AND terminal_kind='node_precondition_failed'))")
            .bind(registration_id).bind(&node).bind(current.content_rev.as_ref().map(|revision| revision.as_str())).bind(registration_id).bind(&node)
            .fetch_one(&mut *tx).await.map_err(storage_error)?;
        if unsafe_local != 0 {
            return Err(StoreError::OutboxBlocked);
        }
        sqlx::query("INSERT INTO outbox_streams (registration_id,next_seq,ack_through) VALUES (?,1,0) ON CONFLICT(registration_id) DO NOTHING")
            .bind(registration_id).execute(&mut *tx).await.map_err(storage_error)?;
        let sequence: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(registration_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let next = sequence
            .checked_add(1)
            .ok_or(StoreError::InvalidGeneration)?;
        sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
            .bind(next)
            .bind(registration_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,?,?,'delete',?,?,? ,?,'queued',0)")
            .bind(&id).bind(registration_id).bind(sequence).bind(&node).bind(expected_node_etag).bind(&b""[..]).bind(payload_hash(b""))
            .execute(&mut *tx).await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id=?")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
        let operation = pending_delete_operation(&row, binding)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(operation)
    }
    pub async fn list_frozen_creates(
        &mut self,
        current: &RegistrationBinding,
        after: Option<&str>,
        limit: u16,
    ) -> Result<FrozenCreatePage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        if let Some(after) = after {
            let parsed: notryx_domain::OperationId =
                after.parse().map_err(|_| StoreError::InvalidRequest)?;
            if parsed.to_string() != after {
                return Err(StoreError::InvalidRequest);
            }
        }
        self.registration_id(current, true).await?;
        let mut rows = sqlx::query("SELECT x.operation_id,x.operation_seq,x.operation_kind,x.document_id,x.storage_id,x.generation,x.dependency,x.expected_revision,x.bytes,x.payload_hash,x.payload_len,x.payload_retired,CASE WHEN x.status='conflict' THEN 'sent' ELSE x.status END AS status,x.abandon_state,NULL AS terminal_kind,x.status AS actual_status,x.terminal_kind AS actual_terminal,x.receipt_json,r.server_origin,r.workspace_id,r.workspace_epoch,r.device_id,r.device_stream_id FROM client_outbox x JOIN client_registrations r ON r.id=x.registration_id WHERE r.status='frozen' AND r.workspace_id=? AND x.operation_kind='create' AND x.status IN ('queued','sent','conflict') AND x.operation_id>? AND NOT EXISTS(SELECT 1 FROM create_reconciliations d WHERE d.old_operation_id=x.operation_id) ORDER BY x.operation_id LIMIT ?")
            .bind(&current.workspace_id).bind(after.unwrap_or("")).bind(i64::from(limit)+1)
            .fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let binding = super::registration::binding_from_row(&row)?;
            let item = validated_create_record(&row, &binding)?;
            let receipt = if item.status == OutboxStatus::Failed {
                let raw: String = row.try_get("receipt_json").map_err(storage_error)?;
                Some(serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?)
            } else {
                None
            };
            entries.push(CreateOutboxState {
                operation: item,
                receipt,
                frozen: true,
            });
        }
        let next_after_id = if has_more {
            entries
                .last()
                .map(|entry| entry.operation.operation_id.clone())
        } else {
            None
        };
        Ok(FrozenCreatePage {
            entries,
            next_after_id,
        })
    }
    pub async fn resolve_frozen_create(
        &mut self,
        old_binding: &RegistrationBinding,
        current_binding: &RegistrationBinding,
        operation_id: &str,
        choice: &FrozenCreateChoice,
    ) -> Result<(), StoreError> {
        self.resolve_frozen_create_inner(
            old_binding,
            current_binding,
            operation_id,
            choice,
            #[cfg(test)]
            None,
        )
        .await
    }

    async fn resolve_frozen_create_inner(
        &mut self,
        old_binding: &RegistrationBinding,
        current_binding: &RegistrationBinding,
        operation_id: &str,
        choice: &FrozenCreateChoice,
        #[cfg(test)] after_registration_lookup: Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    ) -> Result<(), StoreError> {
        if old_binding.workspace_id != current_binding.workspace_id
            || old_binding == current_binding
        {
            return Err(StoreError::InvalidRegistration);
        }
        let old_registration = self.registration_id(old_binding, false).await?;
        let current_registration = self.registration_id(current_binding, true).await?;
        let parsed: notryx_domain::OperationId = operation_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        if parsed.to_string() != operation_id {
            return Err(StoreError::InvalidRequest);
        }
        #[cfg(test)]
        if let Some((ready, resume)) = after_registration_lookup {
            ready.send(()).map_err(|_| StoreError::Storage)?;
            resume.await.map_err(|_| StoreError::Storage)?;
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let current_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(current_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if current_status != "active" {
            return Err(StoreError::StaleRegistration);
        }
        let old_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(old_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if old_status != "frozen" {
            return Err(StoreError::StaleRegistration);
        }
        let row = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE registration_id=? AND operation_id=? AND operation_kind='create'")
            .bind(old_registration).bind(operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let old_item = validated_create_record(&row, old_binding)?;
        if !matches!(
            old_item.status,
            OutboxStatus::Queued
                | OutboxStatus::Sent
                | OutboxStatus::Abandoning
                | OutboxStatus::Failed
        ) {
            return Err(StoreError::InvalidRequest);
        }
        let old_request = old_item.request;
        let dependent: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE dependency=?)")
                .bind(operation_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if dependent != 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        let children = sqlx::query("SELECT x.bytes,x.payload_hash,x.payload_len FROM client_outbox x JOIN client_registrations r ON r.id=x.registration_id WHERE r.workspace_id=? AND x.operation_kind='create' AND x.operation_id<>? AND x.status IN ('queued','sent','conflict') AND NOT EXISTS(SELECT 1 FROM create_reconciliations d WHERE d.old_operation_id=x.operation_id)")
            .bind(&old_binding.workspace_id).bind(operation_id).fetch_all(&mut *tx).await.map_err(storage_error)?;
        for child in children {
            let bytes: Vec<u8> = child.try_get("bytes").map_err(storage_error)?;
            let hash: String = child.try_get("payload_hash").map_err(storage_error)?;
            let len: i64 = child.try_get("payload_len").map_err(storage_error)?;
            let request: notryx_protocol::CreateNodeRequest =
                serde_json::from_slice(&bytes).map_err(|_| StoreError::UnsupportedFormat)?;
            if len != bytes.len() as i64 || payload_hash(&bytes) != hash {
                return Err(StoreError::UnsupportedFormat);
            }
            if request.parent_id == old_request.node_id {
                return Err(StoreError::ReconciliationRequired);
            }
        }
        let existing = sqlx::query(
            "SELECT decision,new_operation_id FROM create_reconciliations WHERE old_operation_id=?",
        )
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        if let Some(existing) = existing {
            let decision: String = existing.try_get("decision").map_err(storage_error)?;
            let new_id: Option<String> = existing
                .try_get("new_operation_id")
                .map_err(storage_error)?;
            match (choice, decision.as_str(), new_id) {
                (FrozenCreateChoice::Dismiss, "dismissed", None) => {}
                (FrozenCreateChoice::Apply { parent_id, name }, "applied", Some(new_id)) => {
                    let row = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=? AND operation_kind='create'")
                        .bind(new_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
                    let registration: i64 =
                        row.try_get("registration_id").map_err(storage_error)?;
                    let request = validated_create_record(&row, current_binding)?.request;
                    if registration != current_registration
                        || request.parent_id != *parent_id
                        || request.name != *name
                        || request.node_id == old_request.node_id
                        || request.kind != old_request.kind
                    {
                        return Err(StoreError::InvalidRequest);
                    }
                }
                _ => return Err(StoreError::InvalidRequest),
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        match choice {
            FrozenCreateChoice::Dismiss => {
                sqlx::query("INSERT INTO create_reconciliations(old_operation_id,decision,new_operation_id) VALUES (?,'dismissed',NULL)")
                    .bind(operation_id).execute(&mut *tx).await.map_err(storage_error)?;
            }
            FrozenCreateChoice::Apply { parent_id, name } => {
                if old_request.kind == notryx_protocol::NodeKind::File
                    && !name.to_ascii_lowercase().ends_with(".md")
                {
                    return Err(StoreError::InvalidRequest);
                }
                if *parent_id == old_request.node_id {
                    return Err(StoreError::ReconciliationRequired);
                }
                let ready: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM remote_index WHERE registration_id=?) AND NOT EXISTS(SELECT 1 FROM sync_bootstrap WHERE registration_id=?)")
                    .bind(current_registration).bind(current_registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
                if ready == 0 {
                    return Err(StoreError::InvalidRequest);
                }
                let parent: Option<String> = sqlx::query_scalar(
                    "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
                )
                .bind(current_registration)
                .bind(parent_id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
                let parent: notryx_protocol::NodeState =
                    serde_json::from_str(&parent.ok_or(StoreError::InvalidRequest)?)
                        .map_err(|_| StoreError::UnsupportedFormat)?;
                if parent.node_id != *parent_id
                    || parent.deleted
                    || parent.kind != notryx_protocol::NodeKind::Folder
                {
                    return Err(StoreError::InvalidRequest);
                }
                let node_id: notryx_domain::NodeId = loop {
                    let candidate: notryx_domain::NodeId = uuid::Uuid::now_v7()
                        .to_string()
                        .parse()
                        .map_err(|_| StoreError::InvalidRequest)?;
                    if candidate == old_request.node_id || candidate == *parent_id {
                        continue;
                    }
                    let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM remote_nodes WHERE registration_id=? AND node_id=? UNION ALL SELECT 1 FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE r.workspace_id=? AND o.document_id=?)")
                        .bind(current_registration).bind(candidate.to_string()).bind(&current_binding.workspace_id).bind(candidate.to_string()).fetch_one(&mut *tx).await.map_err(storage_error)?;
                    if exists == 0 {
                        break candidate;
                    }
                };
                let request = notryx_protocol::CreateNodeRequest {
                    node_id,
                    parent_id: *parent_id,
                    name: name.clone(),
                    kind: old_request.kind,
                    upload_id: None,
                };
                let payload =
                    serde_json::to_vec(&request).map_err(|_| StoreError::InvalidRequest)?;
                if serde_json::from_slice::<notryx_protocol::CreateNodeRequest>(&payload)
                    .ok()
                    .as_ref()
                    != Some(&request)
                {
                    return Err(StoreError::InvalidRequest);
                }
                sqlx::query("INSERT INTO outbox_streams(registration_id,next_seq,ack_through) VALUES(?,1,0) ON CONFLICT(registration_id) DO NOTHING")
                    .bind(current_registration).execute(&mut *tx).await.map_err(storage_error)?;
                let sequence: i64 = sqlx::query_scalar(
                    "SELECT next_seq FROM outbox_streams WHERE registration_id=?",
                )
                .bind(current_registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
                let next = sequence
                    .checked_add(1)
                    .ok_or(StoreError::InvalidGeneration)?;
                sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
                    .bind(next)
                    .bind(current_registration)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                let new_id = uuid::Uuid::now_v7().to_string();
                sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,operation_kind,document_id,bytes,payload_hash,status,payload_len) VALUES(?,?,?,'create',?,?,?,'queued',?)")
                    .bind(&new_id).bind(current_registration).bind(sequence).bind(node_id.to_string()).bind(&payload).bind(payload_hash(&payload)).bind(payload.len() as i64)
                    .execute(&mut *tx).await.map_err(storage_error)?;
                sqlx::query("INSERT INTO create_reconciliations(old_operation_id,decision,new_operation_id) VALUES(?,'applied',?)")
                    .bind(operation_id).bind(new_id).execute(&mut *tx).await.map_err(storage_error)?;
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }
    pub async fn confirm_receipt_ack(
        &mut self,
        binding: &RegistrationBinding,
        ack: &notryx_protocol::AckResponse,
    ) -> Result<(), StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        if ack.workspace_epoch.to_string() != binding.workspace_epoch
            || ack.device_stream_id != binding.device_stream_id
        {
            return Err(StoreError::InvalidReceipt);
        }
        let retired =
            i64::try_from(ack.retired_through.get()).map_err(|_| StoreError::InvalidReceipt)?;
        let last = i64::try_from(ack.last_seq.get()).map_err(|_| StoreError::InvalidReceipt)?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let stream = sqlx::query("SELECT next_seq, ack_through, confirmed_through FROM outbox_streams WHERE registration_id = ?")
            .bind(registration_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?
            .ok_or(StoreError::InvalidReceipt)?;
        let next: i64 = stream.try_get("next_seq").map_err(storage_error)?;
        let local: i64 = stream.try_get("ack_through").map_err(storage_error)?;
        let previous: i64 = stream.try_get("confirmed_through").map_err(storage_error)?;
        if retired < 0 || last < retired || retired > local || last >= next || previous > local {
            return Err(StoreError::InvalidReceipt);
        }
        let confirmed = previous.max(retired);
        sqlx::query("UPDATE outbox_streams SET confirmed_through = ? WHERE registration_id = ? AND confirmed_through <> ?")
            .bind(confirmed)
            .bind(registration_id)
            .bind(confirmed)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let candidates = sqlx::query("SELECT x.operation_id, x.document_id, x.storage_id FROM client_outbox x WHERE x.registration_id = ? AND x.operation_kind = 'content' AND x.operation_seq <= ? AND x.status = 'committed' AND x.terminal_kind IS NULL AND x.payload_retired = 0 AND EXISTS (SELECT 1 FROM client_outbox n WHERE n.registration_id = x.registration_id AND n.document_id = x.document_id AND n.operation_kind = 'content' AND n.status = 'committed' AND n.terminal_kind IS NULL AND n.operation_seq > x.operation_seq) AND NOT EXISTS (SELECT 1 FROM client_outbox d WHERE d.registration_id = x.registration_id AND d.dependency = x.operation_id AND d.status IN ('queued', 'sent', 'conflict')) AND NOT EXISTS (SELECT 1 FROM draft_checkpoints c WHERE c.document_id = x.storage_id AND c.generation > (SELECT n.generation FROM client_outbox n WHERE n.registration_id = x.registration_id AND n.document_id = x.document_id AND n.operation_kind = 'content' AND n.status = 'committed' AND n.terminal_kind IS NULL ORDER BY n.operation_seq DESC LIMIT 1)) ORDER BY x.operation_seq LIMIT 32")
            .bind(registration_id).bind(confirmed).fetch_all(&mut *transaction).await.map_err(storage_error)?;
        let mut clean_documents = std::collections::HashMap::new();
        for candidate in candidates {
            let id: String = candidate.try_get("operation_id").map_err(storage_error)?;
            let document: String = candidate.try_get("document_id").map_err(storage_error)?;
            let storage_id: String = candidate.try_get("storage_id").map_err(storage_error)?;
            let clean = if let Some(clean) = clean_documents.get(&document) {
                *clean
            } else {
                let newest = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND document_id = ? AND operation_kind = 'content' AND status = 'committed' AND terminal_kind IS NULL ORDER BY operation_seq DESC LIMIT 1")
                    .bind(registration_id).bind(&document).fetch_one(&mut *transaction).await.map_err(storage_error)?;
                let newest_storage: String = newest.try_get("storage_id").map_err(storage_error)?;
                let newest_generation: i64 = newest.try_get("generation").map_err(storage_error)?;
                if newest_storage != storage_id || newest_generation <= 0 {
                    return Err(StoreError::UnsupportedFormat);
                }
                let checkpoint = sqlx::query("SELECT c.generation, c.format_version, c.bytes, b.bytes AS base_bytes, b.revision FROM draft_checkpoints c LEFT JOIN draft_bases b USING(document_id) WHERE c.document_id = ?")
                    .bind(&storage_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
                let draft = decode_draft(&checkpoint, &storage_id)?;
                if draft.generation < newest_generation as u64 {
                    return Err(StoreError::UnsupportedFormat);
                }
                let clean = if draft.generation > newest_generation as u64 {
                    false
                } else {
                    let latest = operation(&newest, binding, false)?;
                    let revision: Option<String> = newest
                        .try_get("committed_revision")
                        .map_err(storage_error)?;
                    if !revision.as_deref().is_some_and(valid_content_revision)
                        || revision.as_deref() != Some(&draft.base_revision)
                        || latest.bytes != draft.bytes
                        || latest.bytes != draft.base_bytes
                    {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    true
                };
                clean_documents.insert(document, clean);
                clean
            };
            if !clean {
                continue;
            }
            let old = sqlx::query("SELECT * FROM client_outbox WHERE operation_id = ?")
                .bind(&id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
            operation(&old, binding, false)?;
            let old_revision: Option<String> =
                old.try_get("committed_revision").map_err(storage_error)?;
            if !old_revision.as_deref().is_some_and(valid_content_revision) {
                return Err(StoreError::UnsupportedFormat);
            }
            sqlx::query(
                "UPDATE client_outbox SET bytes = X'', payload_retired = 1 WHERE operation_id = ?",
            )
            .bind(&id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)
    }

    pub async fn confirmed_receipt_through(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<u64, StoreError> {
        let registration_id = self.registration_id(binding, false).await?;
        let confirmed: Option<i64> = sqlx::query_scalar(
            "SELECT confirmed_through FROM outbox_streams WHERE registration_id = ?",
        )
        .bind(registration_id)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        u64::try_from(confirmed.unwrap_or(0)).map_err(|_| StoreError::UnsupportedFormat)
    }

    pub async fn document_outbox_state(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<OutboxDocumentState, StoreError> {
        validate_document(document_id, &[], "status")?;
        let registration_id = self.registration_id(binding, false).await?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let registration_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id = ?")
                .bind(registration_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        let row = sqlx::query("SELECT operation_id, operation_seq, document_id, generation, status, abandon_state, terminal_kind, payload_len, payload_retired FROM client_outbox WHERE registration_id = ? AND document_id = ? AND operation_kind = 'content' ORDER BY operation_seq DESC LIMIT 1")
            .bind(registration_id).bind(document_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?;
        let latest = row
            .as_ref()
            .map(|row| summary(row, registration_status == "frozen"))
            .transpose()?;
        let conflicts: Vec<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT x.operation_id,i.completed_generation,i.completed_through
             FROM client_outbox x LEFT JOIN content_conflict_intents i
               ON i.registration_id=x.registration_id AND i.operation_id=x.operation_id
             WHERE x.registration_id=? AND x.document_id=? AND x.operation_kind='content'
               AND x.status='conflict' AND x.terminal_kind IS NULL
             ORDER BY x.operation_seq",
        )
        .bind(registration_id)
        .bind(document_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let mut has_conflict = false;
        for (operation_id, generation, through) in conflicts {
            if generation.is_none() && through.is_none() {
                if let Some((resolved_document, _)) =
                    crate::content_conflict::verified_completed_merge(
                        &mut transaction,
                        registration_id,
                        binding,
                        &operation_id,
                    )
                    .await?
                {
                    if resolved_document != document_id {
                        return Err(StoreError::UnsupportedFormat);
                    }
                } else {
                    has_conflict = true;
                }
                continue;
            }
            let Some((resolved, _)) = crate::content_conflict::verified_completed_choose_remote(
                &mut transaction,
                registration_id,
                binding,
                &operation_id,
            )
            .await?
            else {
                return Err(StoreError::UnsupportedFormat);
            };
            if resolved.document_id != document_id || resolved.operation_id != operation_id {
                return Err(StoreError::UnsupportedFormat);
            }
        }
        let failure: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id = ? AND document_id = ? AND operation_kind = 'content' AND status = 'conflict' AND terminal_kind IS NOT NULL)")
            .bind(registration_id).bind(document_id).fetch_one(&mut *transaction).await.map_err(storage_error)?;
        let confirmed: Option<i64> = sqlx::query_scalar(
            "SELECT confirmed_through FROM outbox_streams WHERE registration_id=?",
        )
        .bind(registration_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let confirmed =
            u64::try_from(confirmed.unwrap_or(0)).map_err(|_| StoreError::UnsupportedFormat)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok(OutboxDocumentState {
            latest,
            has_conflict,
            has_failure: failure != 0,
            confirmed_through: confirmed,
        })
    }
    pub async fn request_outbox_abandon(
        &mut self,
        binding: &RegistrationBinding,
        id: &str,
    ) -> Result<(), StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT * FROM client_outbox WHERE registration_id = ? AND operation_id = ?",
        )
        .bind(registration_id)
        .bind(id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::InvalidReceipt)?;
        let item = operation(&row, binding, false)?;
        if crate::content_conflict::merged_followup_requires_approval(
            &mut transaction,
            registration_id,
            binding,
            &item,
        )
        .await?
        {
            return Err(StoreError::OutboxBlocked);
        }
        match item.status {
            OutboxStatus::Queued => {
                sqlx::query(
                    "UPDATE client_outbox SET abandon_state = 'requested' WHERE operation_id = ?",
                )
                .bind(id)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
            }
            OutboxStatus::Abandoning => {}
            _ => return Err(StoreError::InvalidReceipt),
        }
        transaction.commit().await.map_err(storage_error)
    }

    pub async fn apply_outbox_abandoned(
        &mut self,
        binding: &RegistrationBinding,
        receipt: &OutboxAbandoned,
    ) -> Result<(), StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        if receipt.workspace_epoch != binding.workspace_epoch {
            return Err(StoreError::InvalidReceipt);
        }
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT * FROM client_outbox WHERE registration_id = ? AND operation_id = ?",
        )
        .bind(registration_id)
        .bind(&receipt.operation_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::InvalidReceipt)?;
        let item = operation(&row, binding, false)?;
        if item.operation_seq != receipt.operation_seq {
            return Err(StoreError::InvalidReceipt);
        }
        if item.status == OutboxStatus::Abandoned {
            transaction.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        let abandon: String = row.try_get("abandon_state").map_err(storage_error)?;
        if item.status != OutboxStatus::Abandoning || abandon != "sent" {
            return Err(StoreError::InvalidReceipt);
        }
        let sequence = i64::try_from(item.operation_seq).map_err(|_| StoreError::InvalidReceipt)?;
        let changed = sqlx::query("UPDATE outbox_streams SET ack_through = ? WHERE registration_id = ? AND ack_through = ?")
            .bind(sequence).bind(registration_id).bind(sequence - 1).execute(&mut *transaction).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidReceipt);
        }
        sqlx::query("UPDATE client_outbox SET status = 'committed', terminal_kind = 'abandoned' WHERE operation_id = ?")
            .bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)
    }

    pub async fn apply_outbox_failure(
        &mut self,
        binding: &RegistrationBinding,
        receipt: &OutboxFailure,
    ) -> Result<(), StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        if receipt.workspace_epoch != binding.workspace_epoch {
            return Err(StoreError::InvalidReceipt);
        }
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT * FROM client_outbox WHERE registration_id = ? AND operation_id = ?",
        )
        .bind(registration_id)
        .bind(&receipt.operation_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::InvalidReceipt)?;
        let item = operation(&row, binding, false)?;
        if item.operation_seq != receipt.operation_seq {
            return Err(StoreError::InvalidReceipt);
        }
        if item.status == OutboxStatus::Failed {
            if item.failure_kind != Some(receipt.kind) {
                return Err(StoreError::InvalidReceipt);
            }
            transaction.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        if item.status != OutboxStatus::Sent {
            return Err(StoreError::InvalidReceipt);
        }
        let sequence = i64::try_from(item.operation_seq).map_err(|_| StoreError::InvalidReceipt)?;
        let changed = sqlx::query("UPDATE outbox_streams SET ack_through = ? WHERE registration_id = ? AND ack_through = ?")
            .bind(sequence).bind(registration_id).bind(sequence - 1).execute(&mut *transaction).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidReceipt);
        }
        sqlx::query("UPDATE client_outbox SET status = 'conflict', terminal_kind = ? WHERE operation_id = ?")
            .bind(receipt.kind.as_str()).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)
    }

    pub async fn list_outbox(
        &mut self,
        binding: &RegistrationBinding,
        after: u64,
        limit: u16,
    ) -> Result<OutboxPage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        let after = i64::try_from(after).map_err(|_| StoreError::InvalidRequest)?;
        let registration_id = self.registration_id(binding, false).await?;
        let registration_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id = ?")
                .bind(registration_id)
                .fetch_one(&mut self.connection)
                .await
                .map_err(storage_error)?;
        let mut rows = sqlx::query("SELECT operation_id, operation_seq, document_id, generation, status, abandon_state, terminal_kind, payload_len, payload_retired FROM client_outbox WHERE registration_id = ? AND operation_kind = 'content' AND operation_seq > ? ORDER BY operation_seq LIMIT ?")
            .bind(registration_id).bind(after).bind(i64::from(limit) + 1).fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(summary(&row, registration_status == "frozen")?);
        }
        let next_after_seq = if has_more {
            entries.last().map(|item| item.operation_seq)
        } else {
            None
        };
        Ok(OutboxPage {
            entries,
            next_after_seq,
        })
    }

    pub async fn read_outbox_operation(
        &mut self,
        binding: &RegistrationBinding,
        id: &str,
    ) -> Result<Option<OutboxOperation>, StoreError> {
        let registration_id = self.registration_id(binding, false).await?;
        let registration_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id = ?")
                .bind(registration_id)
                .fetch_one(&mut self.connection)
                .await
                .map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT * FROM client_outbox WHERE registration_id = ? AND operation_id = ?",
        )
        .bind(registration_id)
        .bind(id)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        row.as_ref()
            .map(|row| operation(row, binding, registration_status == "frozen"))
            .transpose()
    }
    pub async fn apply_outbox_conflict(
        &mut self,
        binding: &RegistrationBinding,
        conflict: &OutboxConflict,
    ) -> Result<(), StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let revision = notryx_protocol::ContentPrecondition::parse(&conflict.current_etag)
            .and_then(|etag| etag.revision())
            .map_err(|_| StoreError::InvalidReceipt)?;
        if conflict.workspace_epoch != binding.workspace_epoch
            || revision.as_str() != conflict.current_revision
        {
            return Err(StoreError::InvalidReceipt);
        }
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT * FROM client_outbox WHERE registration_id = ? AND operation_id = ?",
        )
        .bind(registration_id)
        .bind(&conflict.operation_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::InvalidReceipt)?;
        let item = operation(&row, binding, false)?;
        if item.operation_seq != conflict.operation_seq
            || item.expected_revision.as_deref().is_none_or(|expected| {
                !valid_content_revision(expected) || expected == conflict.current_revision.as_str()
            })
        {
            return Err(StoreError::InvalidReceipt);
        }
        if item.status == OutboxStatus::Conflict {
            let etag: Option<String> = row.try_get("conflict_etag").map_err(storage_error)?;
            let ack: Option<i64> = sqlx::query_scalar(
                "SELECT ack_through FROM outbox_streams WHERE registration_id = ?",
            )
            .bind(registration_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(storage_error)?;
            let sequence =
                i64::try_from(item.operation_seq).map_err(|_| StoreError::InvalidReceipt)?;
            if item.conflict_revision.as_deref() != Some(&conflict.current_revision)
                || etag.as_deref() != Some(&conflict.current_etag)
                || ack.is_none_or(|ack| ack < sequence)
            {
                return Err(StoreError::InvalidReceipt);
            }
            transaction.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        if item.status != OutboxStatus::Sent {
            return Err(StoreError::InvalidReceipt);
        }
        let ack: i64 =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id = ?")
                .bind(registration_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        let sequence = i64::try_from(item.operation_seq).map_err(|_| StoreError::InvalidReceipt)?;
        if ack.checked_add(1) != Some(sequence) {
            return Err(StoreError::InvalidReceipt);
        }
        sqlx::query("UPDATE client_outbox SET status = 'conflict', conflict_revision = ?, conflict_etag = ? WHERE operation_id = ?")
            .bind(&conflict.current_revision).bind(&conflict.current_etag).bind(&item.operation_id)
            .execute(&mut *transaction).await.map_err(storage_error)?;
        sqlx::query("UPDATE outbox_streams SET ack_through = ? WHERE registration_id = ?")
            .bind(sequence)
            .bind(registration_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn queue_checkpoint(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        expected_generation: u64,
    ) -> Result<OutboxOperation, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let storage_id = self
            .registered_storage_id(registration_id, document_id)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT c.generation, c.format_version, c.bytes, b.bytes AS base_bytes, b.revision FROM draft_checkpoints c LEFT JOIN draft_bases b USING(document_id) WHERE c.document_id = ?")
            .bind(&storage_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?
            .ok_or(StoreError::MissingDraft)?;
        let draft = decode_draft(&row, &storage_id)?;
        if draft.generation != expected_generation {
            return Err(StoreError::GenerationMismatch);
        }
        let generation =
            i64::try_from(draft.generation).map_err(|_| StoreError::InvalidGeneration)?;
        let hash = payload_hash(&draft.bytes);
        if requires_reconciliation(&mut transaction, &binding.workspace_id, document_id).await? {
            return Err(StoreError::ReconciliationRequired);
        }
        let last = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND document_id = ? AND operation_kind = 'content' ORDER BY operation_seq DESC LIMIT 1")
            .bind(registration_id).bind(document_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?;
        let mut dependency = None;
        if let Some(row) = last {
            let old = operation(&row, binding, false)?;
            if draft.generation < old.generation {
                return Err(StoreError::StaleGeneration);
            }
            if matches!(old.status, OutboxStatus::Conflict | OutboxStatus::Abandoned) {
                let completed: Vec<(String, i64)> = sqlx::query_as(
                    "SELECT operation_id,completed_generation FROM content_conflict_intents
                     WHERE registration_id=? AND document_id=? AND completed_generation IS NOT NULL
                     ORDER BY completed_generation DESC,operation_id LIMIT 2",
                )
                .bind(registration_id)
                .bind(document_id)
                .fetch_all(&mut *transaction)
                .await
                .map_err(storage_error)?;
                let Some((operation_id, completed_generation)) = completed.first() else {
                    return Err(StoreError::OutboxBlocked);
                };
                if completed
                    .get(1)
                    .is_some_and(|(_, next_generation)| next_generation == completed_generation)
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                let Some((resolved, through)) =
                    crate::content_conflict::verified_completed_choose_remote(
                        &mut transaction,
                        registration_id,
                        binding,
                        operation_id,
                    )
                    .await?
                else {
                    return Err(StoreError::UnsupportedFormat);
                };
                if resolved.binding != *binding
                    || resolved.document_id != document_id
                    || resolved.generation
                        != u64::try_from(*completed_generation)
                            .map_err(|_| StoreError::UnsupportedFormat)?
                    || through != old.operation_seq
                    || draft.generation <= resolved.generation
                    || draft.base_revision != resolved.remote_revision
                    || draft.base_bytes != resolved.remote_bytes
                {
                    return Err(StoreError::OutboxBlocked);
                }
            } else if matches!(old.status, OutboxStatus::Failed | OutboxStatus::Abandoning) {
                return Err(StoreError::OutboxBlocked);
            }
            if draft.generation == old.generation {
                if draft.bytes != old.bytes {
                    return Err(StoreError::GenerationMismatch);
                }
                if old.status == OutboxStatus::Committed {
                    let revision: Option<String> =
                        row.try_get("committed_revision").map_err(storage_error)?;
                    if revision.as_deref() != Some(draft.base_revision.as_str())
                        || draft.base_bytes != old.bytes
                    {
                        return Err(StoreError::UnsupportedFormat);
                    }
                } else if old
                    .expected_revision
                    .as_ref()
                    .is_some_and(|revision| revision != &draft.base_revision)
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                transaction.commit().await.map_err(storage_error)?;
                return Ok(old);
            }
            let intervening_structure: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id = ? AND document_id = ? AND operation_kind IN ('restore', 'move') AND operation_seq > ?)")
                .bind(registration_id).bind(document_id).bind(i64::try_from(old.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?)
                .fetch_one(&mut *transaction).await.map_err(storage_error)?;
            let fixed_merge: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM content_conflict_merged_operations WHERE registration_id=? AND content_operation_id=?) OR EXISTS(SELECT 1 FROM frozen_content_reconciliations WHERE current_registration_id=? AND new_operation_id=?)")
                .bind(registration_id).bind(&old.operation_id).bind(registration_id).bind(&old.operation_id).fetch_one(&mut *transaction).await.map_err(storage_error)?;
            if old.status == OutboxStatus::Queued && intervening_structure == 0 && fixed_merge == 0
            {
                sqlx::query("UPDATE client_outbox SET generation = ?, bytes = ?, payload_len = ?, payload_hash = ? WHERE operation_id = ? AND status = 'queued'")
                    .bind(generation).bind(&draft.bytes).bind(draft.bytes.len() as i64).bind(&hash).bind(&old.operation_id)
                    .execute(&mut *transaction).await.map_err(storage_error)?;
                let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id = ?")
                    .bind(&old.operation_id)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                let updated = operation(&row, binding, false)?;
                transaction.commit().await.map_err(storage_error)?;
                return Ok(updated);
            }
            if old.status == OutboxStatus::Sent
                || (old.status == OutboxStatus::Queued
                    && (intervening_structure != 0 || fixed_merge != 0))
            {
                dependency = Some(old.operation_id);
            }
        }
        sqlx::query("INSERT INTO outbox_streams (registration_id, next_seq, ack_through) VALUES (?, 1, 0) ON CONFLICT(registration_id) DO NOTHING")
            .bind(registration_id).execute(&mut *transaction).await.map_err(storage_error)?;
        let sequence: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id = ?")
                .bind(registration_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        let next = sequence
            .checked_add(1)
            .ok_or(StoreError::InvalidGeneration)?;
        sqlx::query("UPDATE outbox_streams SET next_seq = ? WHERE registration_id = ?")
            .bind(next)
            .bind(registration_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let id = uuid::Uuid::now_v7().to_string();
        let expected = if dependency.is_none() {
            Some(draft.base_revision.as_str())
        } else {
            None
        };
        sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, dependency, bytes, payload_len, payload_hash, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued')")
            .bind(&id).bind(registration_id).bind(sequence).bind(document_id).bind(&storage_id)
            .bind(generation).bind(expected).bind(dependency).bind(&draft.bytes).bind(draft.bytes.len() as i64).bind(&hash)
            .execute(&mut *transaction).await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id = ?")
            .bind(&id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let queued = operation(&row, binding, false)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok(queued)
    }

    pub async fn next_outbox(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<Option<OutboxOperation>, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND status IN ('queued', 'sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        row.map(|row| operation(&row, binding, false)).transpose()
    }

    pub async fn next_pending_outbox(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<Option<PendingOutboxOperation>, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND status IN ('queued', 'sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        let Some(row) = row else { return Ok(None) };
        pending_operation(&row, binding).map(Some)
    }

    pub async fn list_restore_operations(
        &mut self,
        binding: &RegistrationBinding,
        after: u64,
        limit: u16,
    ) -> Result<RestoreOutboxPage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        let after = i64::try_from(after).map_err(|_| StoreError::InvalidRequest)?;
        let registration_id = self.registration_id(binding, false).await?;
        let registration_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id = ?")
                .bind(registration_id)
                .fetch_one(&mut self.connection)
                .await
                .map_err(storage_error)?;
        if !matches!(registration_status.as_str(), "active" | "frozen") {
            return Err(StoreError::UnsupportedFormat);
        }
        let mut rows = sqlx::query("SELECT operation_id, operation_seq, operation_kind, document_id, storage_id, generation, dependency, expected_revision, bytes, payload_hash, payload_len, payload_retired, CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status, abandon_state, NULL AS terminal_kind, status AS actual_status, terminal_kind AS actual_terminal, receipt_json FROM client_outbox WHERE registration_id = ? AND operation_kind = 'restore' AND operation_seq > ? ORDER BY operation_seq LIMIT ?")
            .bind(registration_id).bind(after).bind(i64::from(limit) + 1).fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let PendingOutboxOperation::Restore(mut operation) = pending_operation(&row, binding)?
            else {
                return Err(StoreError::UnsupportedFormat);
            };
            let actual: String = row.try_get("actual_status").map_err(storage_error)?;
            let terminal: Option<String> = row.try_get("actual_terminal").map_err(storage_error)?;
            let raw: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
            let receipt = match (actual.as_str(), terminal.as_deref(), raw.as_deref()) {
                ("queued" | "sent", None, None) => None,
                ("committed", None, Some(raw)) => {
                    operation.status = OutboxStatus::Committed;
                    Some(restore_receipt_for_display(&operation, raw, None)?)
                }
                ("committed", Some("abandoned"), Some(raw))
                    if operation.abandon_state == OutboxAbandonState::Sent =>
                {
                    operation.status = OutboxStatus::Abandoned;
                    Some(restore_receipt_for_display(
                        &operation,
                        raw,
                        Some("abandoned"),
                    )?)
                }
                ("conflict", Some(kind), Some(raw)) => {
                    operation.status = OutboxStatus::Failed;
                    Some(restore_receipt_for_display(&operation, raw, Some(kind))?)
                }
                _ => return Err(StoreError::UnsupportedFormat),
            };
            entries.push(RestoreOutboxState {
                operation,
                receipt,
                frozen: registration_status == "frozen",
            });
        }
        let next_after_seq = if has_more {
            entries.last().map(|entry| entry.operation.operation_seq)
        } else {
            None
        };
        Ok(RestoreOutboxPage {
            entries,
            next_after_seq,
        })
    }

    pub async fn list_create_operations(
        &mut self,
        binding: &RegistrationBinding,
        after: u64,
        limit: u16,
    ) -> Result<CreateOutboxPage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        let after = i64::try_from(after).map_err(|_| StoreError::InvalidRequest)?;
        let registration_id = self.registration_id(binding, false).await?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(registration_id)
                .fetch_one(&mut self.connection)
                .await
                .map_err(storage_error)?;
        if !matches!(status.as_str(), "active" | "frozen") {
            return Err(StoreError::UnsupportedFormat);
        }
        let mut rows = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE registration_id=? AND operation_kind='create' AND operation_seq>? ORDER BY operation_seq LIMIT ?")
            .bind(registration_id).bind(after).bind(i64::from(limit)+1).fetch_all(&mut self.connection).await.map_err(storage_error)?;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let mut operation = pending_create_operation(&row, binding)?;
            let actual: String = row.try_get("actual_status").map_err(storage_error)?;
            let terminal: Option<String> = row.try_get("actual_terminal").map_err(storage_error)?;
            let raw: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
            let receipt = match (actual.as_str(), terminal.as_deref(), raw.as_deref()) {
                ("queued" | "sent", None, None) => None,
                ("committed", None, Some(raw)) => {
                    operation.status = OutboxStatus::Committed;
                    Some(create_receipt_for_display(&operation, raw, None)?)
                }
                ("committed", Some("abandoned"), Some(raw))
                    if operation.abandon_state == OutboxAbandonState::Sent =>
                {
                    operation.status = OutboxStatus::Abandoned;
                    Some(create_receipt_for_display(
                        &operation,
                        raw,
                        Some("abandoned"),
                    )?)
                }
                ("conflict", Some(kind), Some(raw)) => {
                    operation.status = OutboxStatus::Failed;
                    Some(create_receipt_for_display(&operation, raw, Some(kind))?)
                }
                _ => return Err(StoreError::UnsupportedFormat),
            };
            entries.push(CreateOutboxState {
                operation,
                receipt,
                frozen: status == "frozen",
            });
        }
        let next_after_seq = if has_more {
            entries.last().map(|entry| entry.operation.operation_seq)
        } else {
            None
        };
        Ok(CreateOutboxPage {
            entries,
            next_after_seq,
        })
    }

    pub async fn queue_create(
        &mut self,
        binding: &RegistrationBinding,
        request: &notryx_protocol::CreateNodeRequest,
    ) -> Result<CreateOutboxOperation, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let id = uuid::Uuid::now_v7().to_string();
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let operation =
            queue_create_on_connection(&mut tx, registration_id, binding, request, &id).await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(operation)
    }

    pub async fn queue_restore(
        &mut self,
        binding: &RegistrationBinding,
        node_id: notryx_domain::NodeId,
        request: &notryx_protocol::RestoreNodeRequest,
    ) -> Result<RestoreOutboxOperation, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let bytes = serde_json::to_vec(request).map_err(|_| StoreError::InvalidRequest)?;
        if request.parent_id == node_id
            || serde_json::from_slice::<notryx_protocol::RestoreNodeRequest>(&bytes)
                .ok()
                .as_ref()
                != Some(request)
        {
            return Err(StoreError::InvalidRequest);
        }
        let node = node_id.to_string();
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        if requires_reconciliation(&mut transaction, &binding.workspace_id, &node).await? {
            return Err(StoreError::ReconciliationRequired);
        }
        let pending: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id = ? AND document_id = ? AND operation_kind = 'restore' AND status IN ('queued', 'sent'))")
            .bind(registration_id).bind(&node).fetch_one(&mut *transaction).await.map_err(storage_error)?;
        if pending != 0 {
            return Err(StoreError::OutboxBlocked);
        }
        sqlx::query("INSERT INTO outbox_streams (registration_id, next_seq, ack_through) VALUES (?, 1, 0) ON CONFLICT(registration_id) DO NOTHING")
            .bind(registration_id).execute(&mut *transaction).await.map_err(storage_error)?;
        let sequence: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id = ?")
                .bind(registration_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        let next = sequence
            .checked_add(1)
            .ok_or(StoreError::InvalidGeneration)?;
        sqlx::query("UPDATE outbox_streams SET next_seq = ? WHERE registration_id = ?")
            .bind(next)
            .bind(registration_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, operation_kind, document_id, bytes, payload_hash, status, payload_len) VALUES (?, ?, ?, 'restore', ?, ?, ?, 'queued', ?)")
            .bind(&id).bind(registration_id).bind(sequence).bind(&node).bind(&bytes)
            .bind(payload_hash(&bytes)).bind(bytes.len() as i64).execute(&mut *transaction).await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id = ?")
            .bind(&id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let PendingOutboxOperation::Restore(operation) = pending_operation(&row, binding)? else {
            return Err(StoreError::UnsupportedFormat);
        };
        transaction.commit().await.map_err(storage_error)?;
        Ok(operation)
    }

    pub async fn request_create_abandon_matching(
        &mut self,
        binding: &RegistrationBinding,
        expected: &CreateOutboxOperation,
    ) -> Result<bool, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND status IN ('queued','sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        let PendingOutboxOperation::Create(item) = pending_operation(&row, binding)? else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        if item != *expected {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        }
        if item.abandon_state == OutboxAbandonState::None {
            let changed = sqlx::query("UPDATE client_outbox SET abandon_state='requested' WHERE registration_id=? AND operation_id=? AND abandon_state='none'")
                .bind(registration_id).bind(&item.operation_id).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
            if changed != 1 {
                return Err(StoreError::Storage);
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    pub async fn request_restore_abandon_matching(
        &mut self,
        binding: &RegistrationBinding,
        expected: &RestoreOutboxOperation,
    ) -> Result<bool, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND status IN ('queued','sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        let PendingOutboxOperation::Restore(item) = pending_operation(&row, binding)? else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        if item != *expected {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        }
        if item.abandon_state == OutboxAbandonState::None {
            sqlx::query("UPDATE client_outbox SET abandon_state='requested' WHERE registration_id=? AND operation_id=? AND abandon_state='none'")
                .bind(registration_id).bind(&item.operation_id).execute(&mut *tx).await.map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    pub async fn prepare_pending_outbox(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<Option<PendingOutboxOperation>, StoreError> {
        self.prepare_pending_inner(binding, None).await
    }

    pub async fn prepare_pending_outbox_matching(
        &mut self,
        binding: &RegistrationBinding,
        expected: &PendingOutboxOperation,
    ) -> Result<Option<PendingOutboxOperation>, StoreError> {
        self.prepare_pending_inner(binding, Some(expected)).await
    }

    async fn prepare_pending_inner(
        &mut self,
        binding: &RegistrationBinding,
        expected: Option<&PendingOutboxOperation>,
    ) -> Result<Option<PendingOutboxOperation>, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND status IN ('queued', 'sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?;
        let Some(row) = row else {
            transaction.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let item = pending_operation(&row, binding)?;
        if expected.is_some_and(|expected| expected != &item) {
            transaction.rollback().await.map_err(storage_error)?;
            return Ok(None);
        }
        match item {
            PendingOutboxOperation::Move(mut item) => {
                if item.abandon_state == OutboxAbandonState::Requested {
                    let updated = sqlx::query("UPDATE client_outbox SET abandon_state='sent' WHERE registration_id=? AND operation_id=? AND abandon_state='requested'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?.rows_affected();
                    if updated != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.abandon_state = OutboxAbandonState::Sent;
                } else if item.status == OutboxStatus::Queued {
                    let updated = sqlx::query("UPDATE client_outbox SET status='sent' WHERE registration_id=? AND operation_id=? AND status='queued'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?.rows_affected();
                    if updated != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.status = OutboxStatus::Sent;
                }
                transaction.commit().await.map_err(storage_error)?;
                Ok(Some(PendingOutboxOperation::Move(item)))
            }
            PendingOutboxOperation::Delete(mut item) => {
                let storage_ids: Vec<String> = sqlx::query_scalar("SELECT d.storage_id FROM registered_documents d JOIN client_registrations r ON r.id=d.registration_id WHERE r.workspace_id=? AND d.document_id=?")
                    .bind(&binding.workspace_id).bind(item.node_id.to_string()).fetch_all(&mut *transaction).await.map_err(storage_error)?;
                if item.abandon_state == OutboxAbandonState::None
                    && (storage_ids.iter().any(|id| self.sessions.contains_key(id))
                        || requires_reconciliation(
                            &mut transaction,
                            &binding.workspace_id,
                            &item.node_id.to_string(),
                        )
                        .await?)
                {
                    return Err(StoreError::OutboxBlocked);
                }
                let dirty: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id LEFT JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.registration_id=? AND d.document_id=? AND (b.document_id IS NULL OR c.bytes<>b.bytes)) OR EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind<>'delete' AND status IN ('queued','sent','conflict'))")
                    .bind(registration_id).bind(item.node_id.to_string()).bind(registration_id).bind(item.node_id.to_string()).fetch_one(&mut *transaction).await.map_err(storage_error)?;
                if item.abandon_state == OutboxAbandonState::None && dirty != 0 {
                    return Err(StoreError::OutboxBlocked);
                }
                if item.abandon_state == OutboxAbandonState::Requested {
                    let result = sqlx::query("UPDATE client_outbox SET abandon_state='sent' WHERE registration_id=? AND operation_id=? AND abandon_state='requested'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
                    if result.rows_affected() != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.abandon_state = OutboxAbandonState::Sent;
                } else if item.status == OutboxStatus::Queued {
                    let result = sqlx::query("UPDATE client_outbox SET status='sent' WHERE registration_id=? AND operation_id=? AND status='queued'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
                    if result.rows_affected() != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.status = OutboxStatus::Sent;
                }
                transaction.commit().await.map_err(storage_error)?;
                Ok(Some(PendingOutboxOperation::Delete(item)))
            }
            PendingOutboxOperation::Content(item) => {
                transaction.rollback().await.map_err(storage_error)?;
                self.prepare_outbox_matching(binding, &item)
                    .await
                    .map(|result| result.map(PendingOutboxOperation::Content))
            }
            PendingOutboxOperation::Restore(mut item) => {
                if item.abandon_state == OutboxAbandonState::Requested {
                    let result = sqlx::query("UPDATE client_outbox SET abandon_state='sent' WHERE registration_id=? AND operation_id=? AND abandon_state='requested'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
                    if result.rows_affected() != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.abandon_state = OutboxAbandonState::Sent;
                } else if item.status == OutboxStatus::Queued {
                    let result = sqlx::query("UPDATE client_outbox SET status = 'sent' WHERE registration_id = ? AND operation_id = ? AND status = 'queued'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
                    if result.rows_affected() != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.status = OutboxStatus::Sent;
                }
                transaction.commit().await.map_err(storage_error)?;
                Ok(Some(PendingOutboxOperation::Restore(item)))
            }
            PendingOutboxOperation::Create(mut item) => {
                if item.abandon_state == OutboxAbandonState::Requested {
                    let result = sqlx::query("UPDATE client_outbox SET abandon_state='sent' WHERE registration_id=? AND operation_id=? AND abandon_state='requested'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
                    if result.rows_affected() != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.abandon_state = OutboxAbandonState::Sent;
                } else if item.status == OutboxStatus::Queued {
                    let result = sqlx::query("UPDATE client_outbox SET status='sent' WHERE registration_id=? AND operation_id=? AND status='queued'")
                        .bind(registration_id).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
                    if result.rows_affected() != 1 {
                        return Err(StoreError::Storage);
                    }
                    item.status = OutboxStatus::Sent;
                }
                transaction.commit().await.map_err(storage_error)?;
                Ok(Some(PendingOutboxOperation::Create(item)))
            }
        }
    }

    pub async fn apply_create_receipt(
        &mut self,
        binding: &RegistrationBinding,
        receipt: &notryx_protocol::OperationReceipt,
    ) -> Result<(), StoreError> {
        use notryx_protocol::OperationReceipt;
        let registration_id = self.registration_id(binding, true).await?;
        let json = serde_json::to_string(receipt).map_err(|_| StoreError::InvalidReceipt)?;
        if json.len() > 65536
            || serde_json::from_str::<OperationReceipt>(&json)
                .ok()
                .as_ref()
                != Some(receipt)
        {
            return Err(StoreError::InvalidReceipt);
        }
        let (operation_id, sequence, epoch, terminal) = match receipt {
            OperationReceipt::Created {
                operation_id,
                operation_seq,
                workspace_epoch,
                ..
            } => (operation_id, operation_seq, workspace_epoch, None),
            OperationReceipt::NodeDeleted {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("node_deleted"),
            ),
            OperationReceipt::DestinationExists {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("destination_exists"),
            ),
            OperationReceipt::NodeIdConflict {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("node_id_conflict"),
            ),
            OperationReceipt::ParentNotFolder {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("parent_not_folder"),
            ),
            OperationReceipt::NotFound {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_found"),
            ),
            OperationReceipt::NotApplied {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_applied"),
            ),
            OperationReceipt::RecoveryConflict {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("recovery_conflict"),
            ),
            OperationReceipt::Abandoned {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("abandoned"),
            ),
            _ => return Err(StoreError::InvalidReceipt),
        };
        if epoch.to_string() != binding.workspace_epoch {
            return Err(StoreError::InvalidReceipt);
        }
        let sequence = i64::try_from(sequence.get()).map_err(|_| StoreError::InvalidReceipt)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row =
            sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration_id)
                .bind(operation_id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?
                .ok_or(StoreError::InvalidReceipt)?;
        let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
        let stored_seq: i64 = row.try_get("operation_seq").map_err(storage_error)?;
        if kind != "create" || stored_seq != sequence {
            return Err(StoreError::InvalidReceipt);
        }
        let stored: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
        if let Some(stored) = stored {
            if serde_json::from_str::<OperationReceipt>(&stored)
                .ok()
                .as_ref()
                != Some(receipt)
            {
                return Err(StoreError::InvalidReceipt);
            }
            let ack: i64 = sqlx::query_scalar(
                "SELECT ack_through FROM outbox_streams WHERE registration_id=?",
            )
            .bind(registration_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
            if ack < sequence {
                return Err(StoreError::InvalidReceipt);
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        let item = pending_create_operation(&row, binding)?;
        if !matches!(item.status, OutboxStatus::Sent | OutboxStatus::Abandoning)
            || (terminal == Some("abandoned") && item.abandon_state != OutboxAbandonState::Sent)
        {
            return Err(StoreError::InvalidReceipt);
        }
        if let OperationReceipt::Created {
            node, commit_seq, ..
        } = receipt
            && !matches_empty_created_node(&item.request, node, *commit_seq)
        {
            return Err(StoreError::InvalidReceipt);
        }
        let ack: i64 =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id=?")
                .bind(registration_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if ack.checked_add(1) != Some(sequence) {
            return Err(StoreError::InvalidReceipt);
        }
        let changed = sqlx::query("UPDATE client_outbox SET status=?,terminal_kind=?,receipt_json=?,abandon_state=? WHERE registration_id=? AND operation_id=? AND status IN ('queued','sent')")
            .bind(if terminal.is_none() || terminal == Some("abandoned") { "committed" } else { "conflict" })
            .bind(terminal).bind(&json).bind(if terminal == Some("abandoned") { "sent" } else { "none" })
            .bind(registration_id).bind(operation_id.to_string()).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::Storage);
        }
        sqlx::query("UPDATE outbox_streams SET ack_through=? WHERE registration_id=?")
            .bind(sequence)
            .bind(registration_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn apply_restore_receipt(
        &mut self,
        binding: &RegistrationBinding,
        receipt: &notryx_protocol::OperationReceipt,
    ) -> Result<(), StoreError> {
        use notryx_protocol::OperationReceipt;
        let registration_id = self.registration_id(binding, true).await?;
        let json = serde_json::to_string(receipt).map_err(|_| StoreError::InvalidReceipt)?;
        if json.len() > 65536
            || serde_json::from_str::<OperationReceipt>(&json)
                .ok()
                .as_ref()
                != Some(receipt)
        {
            return Err(StoreError::InvalidReceipt);
        }
        let (operation_id, sequence, epoch, terminal) = match receipt {
            OperationReceipt::Restored {
                operation_id,
                operation_seq,
                workspace_epoch,
                ..
            } => (operation_id, operation_seq, workspace_epoch, None),
            OperationReceipt::RecoveryConflict {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("recovery_conflict"),
            ),
            OperationReceipt::NodeDeleted {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("node_deleted"),
            ),
            OperationReceipt::NotDeleted {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_deleted"),
            ),
            OperationReceipt::HistoryExpired {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("history_expired"),
            ),
            OperationReceipt::MetadataPreconditionFailed {
                operation_id,
                operation_seq,
                workspace_epoch,
                current_meta_rev,
                current_meta_etag,
            } => {
                let etag = notryx_protocol::StrongEtag::from_meta_revision(current_meta_rev)
                    .map_err(|_| StoreError::InvalidReceipt)?;
                if etag.as_str() != current_meta_etag {
                    return Err(StoreError::InvalidReceipt);
                }
                (
                    operation_id,
                    operation_seq,
                    workspace_epoch,
                    Some("metadata_precondition_failed"),
                )
            }
            OperationReceipt::DestinationExists {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("destination_exists"),
            ),
            OperationReceipt::ParentNotFolder {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("parent_not_folder"),
            ),
            OperationReceipt::NotFound {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_found"),
            ),
            OperationReceipt::NotApplied {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("not_applied"),
            ),
            OperationReceipt::Abandoned {
                operation_id,
                operation_seq,
                workspace_epoch,
            } => (
                operation_id,
                operation_seq,
                workspace_epoch,
                Some("abandoned"),
            ),
            _ => return Err(StoreError::InvalidReceipt),
        };
        if epoch.to_string() != binding.workspace_epoch {
            return Err(StoreError::InvalidReceipt);
        }
        let sequence = i64::try_from(sequence.get()).map_err(|_| StoreError::InvalidReceipt)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT * FROM client_outbox WHERE registration_id = ? AND operation_id = ?",
        )
        .bind(registration_id)
        .bind(operation_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::InvalidReceipt)?;
        let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
        let stored_seq: i64 = row.try_get("operation_seq").map_err(storage_error)?;
        if kind != "restore" || stored_seq != sequence {
            return Err(StoreError::InvalidReceipt);
        }
        let stored: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
        if let Some(stored) = stored {
            if serde_json::from_str::<OperationReceipt>(&stored)
                .ok()
                .as_ref()
                != Some(receipt)
            {
                return Err(StoreError::InvalidReceipt);
            }
            let ack: i64 = sqlx::query_scalar(
                "SELECT ack_through FROM outbox_streams WHERE registration_id = ?",
            )
            .bind(registration_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
            if ack < sequence {
                return Err(StoreError::InvalidReceipt);
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        let PendingOutboxOperation::Restore(item) = pending_operation(&row, binding)? else {
            return Err(StoreError::InvalidReceipt);
        };
        if !matches!(item.status, OutboxStatus::Sent | OutboxStatus::Abandoning)
            || (terminal == Some("abandoned") && item.abandon_state != OutboxAbandonState::Sent)
        {
            return Err(StoreError::InvalidReceipt);
        }
        if let OperationReceipt::Restored {
            node, commit_seq, ..
        } = receipt
        {
            let etag = notryx_protocol::StrongEtag::from_meta_revision(&node.meta_rev)
                .map_err(|_| StoreError::InvalidReceipt)?;
            if node.node_id != item.node_id
                || node.parent_id != Some(item.request.parent_id)
                || node.name != item.request.name
                || node.deleted
                || commit_seq.get() == 0
                || node.last_change_seq != *commit_seq
                || node.meta_rev == item.request.expected_meta_rev
                || etag.as_str() != node.meta_etag
                || notryx_protocol::StrongEtag::parse(&node.node_etag).is_err()
            {
                return Err(StoreError::InvalidReceipt);
            }
            if node.kind == notryx_protocol::NodeKind::File {
                let revision = node
                    .content_rev
                    .as_ref()
                    .ok_or(StoreError::InvalidReceipt)?;
                let etag = notryx_protocol::StrongEtag::from_content_revision(revision)
                    .map_err(|_| StoreError::InvalidReceipt)?;
                let hash = node
                    .content_hash
                    .as_deref()
                    .and_then(|value| value.strip_prefix("sha256:"))
                    .ok_or(StoreError::InvalidReceipt)?;
                if node.content_etag.as_deref() != Some(etag.as_str())
                    || hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    || node
                        .size
                        .as_deref()
                        .and_then(|size| size.parse::<u64>().ok())
                        .is_none()
                {
                    return Err(StoreError::InvalidReceipt);
                }
            }
        }
        let ack: i64 =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id = ?")
                .bind(registration_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if ack.checked_add(1) != Some(sequence) {
            return Err(StoreError::InvalidReceipt);
        }
        sqlx::query("UPDATE client_outbox SET status = ?, terminal_kind = ?, receipt_json = ?, abandon_state = ? WHERE registration_id = ? AND operation_id = ? AND status IN ('queued','sent')")
            .bind(if terminal.is_none() || terminal == Some("abandoned") { "committed" } else { "conflict" }).bind(terminal).bind(&json).bind(if terminal == Some("abandoned") { "sent" } else { "none" }).bind(registration_id).bind(operation_id.to_string())
            .execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("UPDATE outbox_streams SET ack_through = ? WHERE registration_id = ?")
            .bind(sequence)
            .bind(registration_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn prepare_outbox(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<Option<OutboxOperation>, StoreError> {
        self.prepare_outbox_inner(binding, None).await
    }

    pub async fn prepare_outbox_matching(
        &mut self,
        binding: &RegistrationBinding,
        expected: &OutboxOperation,
    ) -> Result<Option<OutboxOperation>, StoreError> {
        self.prepare_outbox_inner(binding, Some(expected)).await
    }

    async fn prepare_outbox_inner(
        &mut self,
        binding: &RegistrationBinding,
        expected: Option<&OutboxOperation>,
    ) -> Result<Option<OutboxOperation>, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND status IN ('queued', 'sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?;
        let Some(row) = row else {
            transaction.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let mut item = operation(&row, binding, false)?;
        if expected.is_some_and(|expected| expected != &item) {
            transaction.rollback().await.map_err(storage_error)?;
            return Ok(None);
        }
        if item.status == OutboxStatus::Abandoning {
            sqlx::query("UPDATE client_outbox SET abandon_state = 'sent' WHERE operation_id = ? AND abandon_state = 'requested'")
                .bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
            item.abandon_state = OutboxAbandonState::Sent;
            transaction.commit().await.map_err(storage_error)?;
            return Ok(Some(item));
        }
        if item.status == OutboxStatus::Queued
            && requires_reconciliation(&mut transaction, &binding.workspace_id, &item.document_id)
                .await?
        {
            return Err(StoreError::ReconciliationRequired);
        }
        if item.expected_revision.is_none() {
            return Err(StoreError::OutboxBlocked);
        }
        let etag = format!(
            "\"{}\"",
            item.expected_revision
                .as_deref()
                .ok_or(StoreError::OutboxBlocked)?
        );
        notryx_protocol::ContentPrecondition::parse(&etag)
            .and_then(|value| value.revision())
            .map_err(|_| StoreError::UnsupportedFormat)?;
        if item.status == OutboxStatus::Queued {
            sqlx::query("UPDATE client_outbox SET status = 'sent' WHERE operation_id = ? AND status = 'queued'")
                .bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
            item.status = OutboxStatus::Sent;
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(Some(item))
    }

    pub async fn apply_outbox_commit(
        &mut self,
        binding: &RegistrationBinding,
        commit: &OutboxCommit,
    ) -> Result<(), StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        if commit.workspace_epoch != binding.workspace_epoch {
            return Err(StoreError::InvalidReceipt);
        }
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT * FROM client_outbox WHERE registration_id = ? AND operation_id = ?",
        )
        .bind(registration_id)
        .bind(&commit.operation_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::InvalidReceipt)?;
        let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
        if retired == 1 {
            let state: String = row.try_get("status").map_err(storage_error)?;
            let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
            let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
            let document: String = row.try_get("document_id").map_err(storage_error)?;
            let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
            let revision: Option<String> =
                row.try_get("committed_revision").map_err(storage_error)?;
            if state != "committed"
                || terminal.is_some()
                || u64::try_from(sequence).ok() != Some(commit.operation_seq)
                || document != commit.document_id
                || hash != commit.content_hash
                || revision.as_deref() != Some(&commit.content_revision)
                || !valid_content_revision(&commit.content_revision)
            {
                return Err(StoreError::InvalidReceipt);
            }
            transaction.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        let item = operation(&row, binding, false)?;
        if item.operation_seq != commit.operation_seq
            || item.document_id != commit.document_id
            || item.payload_hash != commit.content_hash
        {
            return Err(StoreError::InvalidReceipt);
        }
        validate_document(&item.document_id, &item.bytes, &commit.content_revision)
            .map_err(|_| StoreError::InvalidReceipt)?;
        let etag = format!("\"{}\"", commit.content_revision);
        notryx_protocol::ContentPrecondition::parse(&etag)
            .and_then(|value| value.revision())
            .map_err(|_| StoreError::InvalidReceipt)?;
        if item.status == OutboxStatus::Committed {
            let previous: Option<String> =
                row.try_get("committed_revision").map_err(storage_error)?;
            if previous.as_deref() != Some(&commit.content_revision) {
                return Err(StoreError::InvalidReceipt);
            }
            transaction.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        if item.status != OutboxStatus::Sent {
            return Err(StoreError::InvalidReceipt);
        }
        let storage_id: String = row.try_get("storage_id").map_err(storage_error)?;
        let changed =
            sqlx::query("UPDATE draft_bases SET bytes = ?, revision = ? WHERE document_id = ?")
                .bind(&item.bytes)
                .bind(&commit.content_revision)
                .bind(&storage_id)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?
                .rows_affected();
        if changed != 1 {
            return Err(StoreError::UnsupportedFormat);
        }
        sqlx::query("UPDATE client_outbox SET status = 'committed', committed_revision = ? WHERE operation_id = ?")
            .bind(&commit.content_revision).bind(&item.operation_id).execute(&mut *transaction).await.map_err(storage_error)?;
        sqlx::query("UPDATE client_outbox SET expected_revision = ? WHERE registration_id = ? AND dependency = ? AND status = 'queued'")
            .bind(&commit.content_revision).bind(registration_id).bind(&item.operation_id)
            .execute(&mut *transaction).await.map_err(storage_error)?;
        let ack: i64 =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id = ?")
                .bind(registration_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        let sequence = i64::try_from(item.operation_seq).map_err(|_| StoreError::InvalidReceipt)?;
        if ack.checked_add(1) != Some(sequence) {
            return Err(StoreError::InvalidReceipt);
        }
        sqlx::query("UPDATE outbox_streams SET ack_through = ? WHERE registration_id = ?")
            .bind(sequence)
            .bind(registration_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)?;
        if let Some(session) = self.sessions.get_mut(&storage_id) {
            session.base = item.bytes;
            session.base_revision.clone_from(&commit.content_revision);
        }
        Ok(())
    }

    pub async fn receipt_ack_through(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<u64, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let ack: Option<i64> =
            sqlx::query_scalar("SELECT ack_through FROM outbox_streams WHERE registration_id = ?")
                .bind(registration_id)
                .fetch_optional(&mut self.connection)
                .await
                .map_err(storage_error)?;
        u64::try_from(ack.unwrap_or(0)).map_err(|_| StoreError::UnsupportedFormat)
    }
}
// Append in outbox.rs as a private unit-test module. The proposed
// resolve_frozen_create_inner test-only hook is in the companion patch.
#[cfg(test)]
mod frozen_create_registration_gap_tests {
    use super::*;
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::path::Path;
    use uuid::Uuid;

    const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
    const OLD_NODE: &str = "019a1000-0000-7000-8000-000000000003";

    fn binding() -> RegistrationBinding {
        RegistrationBinding {
            server_origin: "https://workspace.example".into(),
            workspace_id: Uuid::new_v4().to_string(),
            workspace_epoch: Uuid::new_v4().to_string(),
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
        }
    }

    fn replacement(old: &RegistrationBinding) -> RegistrationBinding {
        RegistrationBinding {
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
            ..old.clone()
        }
    }

    async fn raw(path: &Path) -> SqliteConnection {
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(path)
                .foreign_keys(true),
        )
        .await
        .unwrap()
    }

    async fn seed_root(path: &Path, binding: &RegistrationBinding) {
        let mut conn = raw(path).await;
        let registration: i64 = sqlx::query_scalar("SELECT id FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .bind(&binding.device_id).bind(&binding.device_stream_id).fetch_one(&mut conn).await.unwrap();
        let root = serde_json::json!({ "node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,
            "content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"",
            "node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"0" });
        sqlx::query("INSERT INTO remote_index VALUES (?,?,'0')")
            .bind(registration)
            .bind(ROOT)
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO remote_nodes VALUES (?,?,?,0)")
            .bind(registration)
            .bind(ROOT)
            .bind(root.to_string())
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }

    #[tokio::test]
    async fn registration_replacement_after_lookup_is_rejected_before_any_decision() {
        for choice in [
            FrozenCreateChoice::Apply {
                parent_id: ROOT.parse().unwrap(),
                name: "replacement".into(),
            },
            FrozenCreateChoice::Dismiss,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("store.db");
            let old = binding();
            let current = replacement(&old);
            let next = replacement(&current);
            let mut store = ClientStore::open(&path).await.unwrap();
            store.activate_registration(&old).await.unwrap();
            seed_root(&path, &old).await;
            let request = notryx_protocol::CreateNodeRequest {
                node_id: OLD_NODE.parse().unwrap(),
                parent_id: ROOT.parse().unwrap(),
                name: "original".into(),
                kind: notryx_protocol::NodeKind::Folder,
                upload_id: None,
            };
            let queued = store.queue_create(&old, &request).await.unwrap();
            store.activate_registration(&current).await.unwrap();
            seed_root(&path, &current).await;
            let (ready, observed) = tokio::sync::oneshot::channel();
            let (resume, resumed) = tokio::sync::oneshot::channel();
            let previous = old.clone();
            let selected = current.clone();
            let id = queued.operation_id.clone();
            let task = tokio::spawn(async move {
                let result = store
                    .resolve_frozen_create_inner(
                        &previous,
                        &selected,
                        &id,
                        &choice,
                        Some((ready, resumed)),
                    )
                    .await;
                (store, result)
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), observed)
                .await
                .unwrap()
                .unwrap();
            let mut writer = raw(&path).await;
            sqlx::query("BEGIN IMMEDIATE")
                .execute(&mut writer)
                .await
                .unwrap();
            let changed = sqlx::query("UPDATE client_registrations SET status='frozen' WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=? AND status='active'")
                .bind(&current.server_origin).bind(&current.workspace_id).bind(&current.workspace_epoch)
                .bind(&current.device_id).bind(&current.device_stream_id).execute(&mut writer).await.unwrap().rows_affected();
            assert_eq!(changed, 1);
            sqlx::query("INSERT INTO client_registrations(server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,status) VALUES(?,?,?,?,?,'active')")
                .bind(&next.server_origin).bind(&next.workspace_id).bind(&next.workspace_epoch)
                .bind(&next.device_id).bind(&next.device_stream_id).execute(&mut writer).await.unwrap();
            sqlx::query("COMMIT").execute(&mut writer).await.unwrap();
            writer.close().await.unwrap();
            resume.send(()).unwrap();
            let (mut store, result) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result, Err(StoreError::StaleRegistration));
            assert_eq!(
                store.current_registration().await.unwrap(),
                Some(next.clone())
            );
            let mut inspection = raw(&path).await;
            let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM create_reconciliations")
                .fetch_one(&mut inspection)
                .await
                .unwrap();
            let newer: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM client_outbox WHERE operation_id<>?")
                    .bind(&queued.operation_id)
                    .fetch_one(&mut inspection)
                    .await
                    .unwrap();
            let newer_streams: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox_streams s JOIN client_registrations r ON r.id=s.registration_id WHERE r.device_id=? OR r.device_id=?")
                .bind(&current.device_id).bind(&next.device_id).fetch_one(&mut inspection).await.unwrap();
            assert_eq!((decisions, newer, newer_streams), (0, 0, 0));
            let (status, data, receipt): (String, Vec<u8>, Option<String>) = sqlx::query_as(
                "SELECT status,bytes,receipt_json FROM client_outbox WHERE operation_id=?",
            )
            .bind(&queued.operation_id)
            .fetch_one(&mut inspection)
            .await
            .unwrap();
            assert_eq!(status, "queued");
            assert_eq!(
                serde_json::from_slice::<notryx_protocol::CreateNodeRequest>(&data).unwrap(),
                request
            );
            assert!(receipt.is_none());
            inspection.close().await.unwrap();
            store.close().await.unwrap();
        }
    }
}

pub(crate) const UPGRADE_V22: &str = "
    CREATE TABLE frozen_content_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        current_registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        new_operation_id TEXT NOT NULL UNIQUE REFERENCES client_outbox(operation_id),
        approval_hash TEXT NOT NULL,
        history_hash TEXT NOT NULL,
        view_fingerprint TEXT NOT NULL,
        new_operation_seq INTEGER NOT NULL CHECK(new_operation_seq > 0),
        generation INTEGER NOT NULL CHECK(generation > 0),
        draft_bytes BLOB NOT NULL,
        base_bytes BLOB NOT NULL,
        base_revision TEXT NOT NULL,
        old_payload BLOB NOT NULL,
        old_payload_hash TEXT NOT NULL,
        old_status TEXT NOT NULL,
        old_generation INTEGER NOT NULL,
        old_seq INTEGER NOT NULL,
        old_expected_revision TEXT,
        old_dependency TEXT,
        old_payload_len INTEGER NOT NULL,
        old_payload_retired INTEGER NOT NULL,
        new_payload_hash TEXT NOT NULL,
        new_expected_revision TEXT NOT NULL
    );
    PRAGMA user_version = 22;
";

pub(crate) const UPGRADE_V23: &str = "
    CREATE TABLE frozen_content_dismissals (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        current_registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        approval_hash TEXT NOT NULL,
        history_hash TEXT NOT NULL,
        view_fingerprint TEXT NOT NULL,
        operation_seq INTEGER NOT NULL CHECK(operation_seq > 0),
        operation_generation INTEGER NOT NULL CHECK(operation_generation > 0),
        operation_status TEXT NOT NULL,
        abandon_state TEXT NOT NULL,
        expected_revision TEXT,
        dependency TEXT,
        payload_hash TEXT NOT NULL,
        payload BLOB NOT NULL,
        draft_generation INTEGER NOT NULL CHECK(draft_generation > 0),
        draft BLOB NOT NULL,
        base BLOB NOT NULL,
        base_revision TEXT NOT NULL
    );
    PRAGMA user_version = 23;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrozenDocumentOperationKind {
    Content,
    Create,
    Move,
    Delete,
    Restore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenDocumentOperationSummary {
    pub operation_id: String,
    pub operation_seq: u64,
    pub kind: FrozenDocumentOperationKind,
    pub generation: Option<u64>,
    pub dependency: Option<String>,
    pub expected_revision: Option<String>,
    pub status: OutboxStatus,
    pub abandon_state: OutboxAbandonState,
}

#[derive(Debug, PartialEq, Eq)]
pub struct FrozenDocumentReviewPage {
    pub old_binding: RegistrationBinding,
    pub current_binding: RegistrationBinding,
    pub document_id: String,
    pub draft: Option<super::PersistedDraft>,
    pub operations: Vec<FrozenDocumentOperationSummary>,
    pub next_after_seq: Option<u64>,
}

async fn frozen_review_registration_ids(
    store: &mut ClientStore,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
) -> Result<(i64, i64), StoreError> {
    let node: notryx_domain::NodeId = document.parse().map_err(|_| StoreError::InvalidRequest)?;
    if node.to_string() != document || old.workspace_id != current.workspace_id || old == current {
        return Err(StoreError::InvalidRegistration);
    }
    let current_id = store.registration_id(current, true).await?;
    let old_id = store.registration_id(old, false).await?;
    Ok((current_id, old_id))
}

async fn validate_frozen_review_registration(
    connection: &mut SqliteConnection,
    current_id: i64,
    old_id: i64,
) -> Result<(), StoreError> {
    let current: Option<String> =
        sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
            .bind(current_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?;
    let old: Option<String> =
        sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
            .bind(old_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?;
    if current.as_deref() != Some("active") || old.as_deref() != Some("frozen") {
        return Err(StoreError::StaleRegistration);
    }
    Ok(())
}

async fn frozen_review_summary(
    connection: &mut SqliteConnection,
    row: &SqliteRow,
    registration: i64,
    document: &str,
) -> Result<FrozenDocumentOperationSummary, StoreError> {
    let id: String = row.try_get("operation_id").map_err(storage_error)?;
    let parsed: notryx_domain::OperationId =
        id.parse().map_err(|_| StoreError::UnsupportedFormat)?;
    let sequence: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
    let kind = match kind.as_str() {
        "content" => FrozenDocumentOperationKind::Content,
        "create" => FrozenDocumentOperationKind::Create,
        "move" => FrozenDocumentOperationKind::Move,
        "delete" => FrozenDocumentOperationKind::Delete,
        "restore" => FrozenDocumentOperationKind::Restore,
        _ => return Err(StoreError::UnsupportedFormat),
    };
    let generation: Option<i64> = row.try_get("generation").map_err(storage_error)?;
    let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
    let expected_revision: Option<String> =
        row.try_get("expected_revision").map_err(storage_error)?;
    let raw_status: String = row.try_get("status").map_err(storage_error)?;
    let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
    let abandon = abandon_state(row)?;
    let status = if kind == FrozenDocumentOperationKind::Content {
        status(row)?.0
    } else {
        match (raw_status.as_str(), abandon, terminal.as_deref()) {
            ("queued", OutboxAbandonState::None, None) => OutboxStatus::Queued,
            ("sent", OutboxAbandonState::None, None) => OutboxStatus::Sent,
            ("queued" | "sent", OutboxAbandonState::Requested | OutboxAbandonState::Sent, None) => {
                OutboxStatus::Abandoning
            }
            ("conflict", OutboxAbandonState::None, Some(terminal))
                if match kind {
                    FrozenDocumentOperationKind::Create => matches!(
                        terminal,
                        "recovery_conflict"
                            | "node_deleted"
                            | "destination_exists"
                            | "node_id_conflict"
                            | "parent_not_folder"
                            | "not_found"
                            | "not_applied"
                    ),
                    FrozenDocumentOperationKind::Move => matches!(
                        terminal,
                        "metadata_precondition_failed"
                            | "expected_parent_mismatch"
                            | "destination_exists"
                            | "parent_not_folder"
                            | "node_deleted"
                            | "not_found"
                            | "not_applied"
                            | "recovery_conflict"
                    ),
                    FrozenDocumentOperationKind::Delete => matches!(
                        terminal,
                        "node_precondition_failed"
                            | "node_deleted"
                            | "not_found"
                            | "not_applied"
                            | "recovery_conflict"
                    ),
                    FrozenDocumentOperationKind::Restore => matches!(
                        terminal,
                        "recovery_conflict"
                            | "node_deleted"
                            | "not_deleted"
                            | "history_expired"
                            | "metadata_precondition_failed"
                            | "destination_exists"
                            | "parent_not_folder"
                            | "not_found"
                            | "not_applied"
                    ),
                    FrozenDocumentOperationKind::Content => false,
                } =>
            {
                OutboxStatus::Failed
            }
            _ => return Err(StoreError::UnsupportedFormat),
        }
    };
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    let length: i64 = row.try_get("payload_len").map_err(storage_error)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    if parsed.to_string() != id
        || sequence <= 0
        || retired != 0
        || length < 0
        || length as u64 > super::MAX_DOCUMENT_BYTES as u64
        || hash.strip_prefix("sha256:").is_none_or(|hex| {
            hex.len() != 64
                || !hex
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
    {
        return Err(StoreError::UnsupportedFormat);
    }
    if kind == FrozenDocumentOperationKind::Content {
        let storage: Option<String> = row.try_get("storage_id").map_err(storage_error)?;
        let registered: Option<String> = sqlx::query_scalar(
            "SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?",
        )
        .bind(registration)
        .bind(document)
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage_error)?;
        if storage.is_none() || storage != registered {
            return Err(StoreError::UnsupportedFormat);
        }
        let generation_value = generation
            .filter(|value| *value > 0)
            .ok_or(StoreError::UnsupportedFormat)?;
        if expected_revision
            .as_ref()
            .is_some_and(|value| !valid_content_revision(value))
            || (status == OutboxStatus::Sent && expected_revision.is_none())
        {
            return Err(StoreError::UnsupportedFormat);
        }
        if let Some(dependency_id) = &dependency {
            let parsed: notryx_domain::OperationId = dependency_id
                .parse()
                .map_err(|_| StoreError::UnsupportedFormat)?;
            if parsed.to_string() != *dependency_id {
                return Err(StoreError::UnsupportedFormat);
            }
            let previous = sqlx::query("SELECT operation_seq,generation,document_id,operation_kind FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration).bind(dependency_id).fetch_optional(&mut *connection).await.map_err(storage_error)?
                .ok_or(StoreError::UnsupportedFormat)?;
            let previous_seq: i64 = previous.try_get("operation_seq").map_err(storage_error)?;
            let previous_generation: Option<i64> =
                previous.try_get("generation").map_err(storage_error)?;
            let previous_document: String =
                previous.try_get("document_id").map_err(storage_error)?;
            let previous_kind: String =
                previous.try_get("operation_kind").map_err(storage_error)?;
            if previous_seq <= 0
                || previous_seq >= sequence
                || previous_generation.is_none_or(|value| value <= 0 || value >= generation_value)
                || previous_document != document
                || previous_kind != "content"
            {
                return Err(StoreError::UnsupportedFormat);
            }
        }
    } else if generation.is_some() || dependency.is_some() {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(FrozenDocumentOperationSummary {
        operation_id: id,
        operation_seq: sequence as u64,
        kind,
        generation: generation.map(|value| value as u64),
        dependency,
        expected_revision,
        status,
        abandon_state: abandon,
    })
}

impl ClientStore {
    pub async fn frozen_document_review(
        &mut self,
        current: &RegistrationBinding,
        old: &RegistrationBinding,
        document: &str,
        after: u64,
        limit: u16,
    ) -> Result<FrozenDocumentReviewPage, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidRequest);
        }
        let after = i64::try_from(after).map_err(|_| StoreError::InvalidRequest)?;
        let (current_id, old_id) =
            frozen_review_registration_ids(self, current, old, document).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        validate_frozen_review_registration(&mut tx, current_id, old_id).await?;
        let draft_row = sqlx::query("SELECT d.storage_id,c.generation,c.format_version,c.bytes,b.bytes AS base_bytes,b.revision FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id LEFT JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.registration_id=? AND d.document_id=?")
            .bind(old_id).bind(document).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let draft = draft_row
            .as_ref()
            .map(|row| {
                let storage: String = row.try_get("storage_id").map_err(storage_error)?;
                decode_draft(row, &storage)
            })
            .transpose()?;
        let mut rows = sqlx::query("SELECT operation_id,operation_seq,operation_kind,storage_id,generation,dependency,expected_revision,status,abandon_state,terminal_kind,payload_retired,payload_len,payload_hash FROM client_outbox WHERE registration_id=? AND document_id=? AND status IN ('queued','sent','conflict') AND operation_seq>? ORDER BY operation_seq LIMIT ?")
            .bind(old_id).bind(document).bind(after).bind(i64::from(limit)+1).fetch_all(&mut *tx).await.map_err(storage_error)?;
        let more = rows.len() > usize::from(limit);
        if more {
            rows.pop();
        }
        let mut operations = Vec::with_capacity(rows.len());
        for row in &rows {
            operations.push(frozen_review_summary(&mut tx, row, old_id, document).await?);
        }
        let next_after_seq = if more {
            operations.last().map(|item| item.operation_seq)
        } else {
            None
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(FrozenDocumentReviewPage {
            old_binding: old.clone(),
            current_binding: current.clone(),
            document_id: document.to_owned(),
            draft,
            operations,
            next_after_seq,
        })
    }

    pub async fn frozen_document_operation_bytes(
        &mut self,
        current: &RegistrationBinding,
        old: &RegistrationBinding,
        document: &str,
        id: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let (current_id, old_id) =
            frozen_review_registration_ids(self, current, old, document).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        validate_frozen_review_registration(&mut tx, current_id, old_id).await?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_id=? AND status IN ('queued','sent','conflict')")
            .bind(old_id).bind(document).bind(id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let summary = frozen_review_summary(&mut tx, &row, old_id, document).await?;
        if summary.kind == FrozenDocumentOperationKind::Content {
            operation(&row, old, true)?;
        } else {
            let aliased = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status='conflict' THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json,(SELECT s.ack_through FROM outbox_streams s WHERE s.registration_id=client_outbox.registration_id) AS saved_ack FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(old_id).bind(id).fetch_one(&mut *tx).await.map_err(storage_error)?;
            match summary.kind {
                FrozenDocumentOperationKind::Create => {
                    validated_create_record(&aliased, old)?;
                }
                FrozenDocumentOperationKind::Move => {
                    let item = validated_move_record(&aliased, old)?;
                    verify_failed_move_ack(&aliased, &item)?;
                }
                FrozenDocumentOperationKind::Delete => {
                    validated_delete_record(&aliased, old)?;
                }
                FrozenDocumentOperationKind::Restore => {
                    let PendingOutboxOperation::Restore(item) = pending_operation(&aliased, old)?
                    else {
                        return Err(StoreError::UnsupportedFormat);
                    };
                    let actual: String = aliased.try_get("actual_status").map_err(storage_error)?;
                    let terminal: Option<String> =
                        aliased.try_get("actual_terminal").map_err(storage_error)?;
                    let receipt: Option<String> =
                        aliased.try_get("receipt_json").map_err(storage_error)?;
                    match (actual.as_str(), terminal.as_deref(), receipt.as_deref()) {
                        ("queued" | "sent", None, None) => {}
                        ("conflict", Some(kind), Some(raw)) => {
                            restore_receipt_for_display(&item, raw, Some(kind))?;
                        }
                        _ => return Err(StoreError::UnsupportedFormat),
                    }
                }
                FrozenDocumentOperationKind::Content => unreachable!(),
            }
        }
        let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(bytes)
    }
}

#[derive(Clone)]
pub struct FrozenContentApplyApproval {
    pub old: RegistrationBinding,
    pub current: RegistrationBinding,
    pub document: String,
    pub old_operation_id: String,
    pub old_operation_seq: u64,
    pub old_operation_hash: String,
    pub old_operation_status: OutboxStatus,
    pub generation: u64,
    pub draft: Vec<u8>,
    pub base: Vec<u8>,
    pub base_revision: String,
    pub remote_revision: String,
    pub remote_hash: String,
    pub remote_bytes: Vec<u8>,
    pub remote_size: u64,
    pub remote_cursor: String,
    pub view_fingerprint: String,
}

fn frozen_approval_hash(approval: &FrozenContentApplyApproval) -> String {
    let mut hash = Sha256::new();
    fn part(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    for binding in [&approval.old, &approval.current] {
        for value in [
            &binding.server_origin,
            &binding.workspace_id,
            &binding.workspace_epoch,
            &binding.device_id,
            &binding.device_stream_id,
        ] {
            part(&mut hash, value.as_bytes());
        }
    }
    for value in [
        &approval.document,
        &approval.old_operation_id,
        &approval.old_operation_hash,
        &approval.base_revision,
        &approval.remote_revision,
        &approval.remote_hash,
        &approval.remote_cursor,
        &approval.view_fingerprint,
    ] {
        part(&mut hash, value.as_bytes());
    }
    part(&mut hash, &approval.old_operation_seq.to_be_bytes());
    part(&mut hash, &approval.generation.to_be_bytes());
    part(&mut hash, &approval.remote_size.to_be_bytes());
    part(
        &mut hash,
        format!("{:?}", approval.old_operation_status).as_bytes(),
    );
    part(&mut hash, &approval.draft);
    part(&mut hash, &approval.base);
    part(&mut hash, &approval.remote_bytes);
    let digest = hash.finalize();
    format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

impl ClientStore {
    pub async fn apply_frozen_content_approval(
        &mut self,
        approval: &FrozenContentApplyApproval,
    ) -> Result<String, StoreError> {
        self.apply_frozen_content_inner(approval, None).await
    }

    pub async fn apply_frozen_content_across_epochs(
        &mut self,
        approval: &FrozenContentApplyApproval,
        confirmation: &FrozenContentCrossEpochConfirmation,
    ) -> Result<String, StoreError> {
        self.apply_frozen_content_inner(approval, Some(confirmation))
            .await
    }

    async fn apply_frozen_content_inner(
        &mut self,
        approval: &FrozenContentApplyApproval,
        confirmation: Option<&FrozenContentCrossEpochConfirmation>,
    ) -> Result<String, StoreError> {
        if !valid_frozen_fingerprint(&approval.view_fingerprint) {
            return Err(StoreError::InvalidRequest);
        }
        let different_epoch = approval.old.workspace_epoch != approval.current.workspace_epoch;
        match (different_epoch, confirmation) {
            (true, Some(confirmation))
                if confirmation.explicitly_confirmed
                    && confirmation.selected_current_document == approval.document
                    && confirmation.old_epoch == approval.old.workspace_epoch
                    && confirmation.current_epoch == approval.current.workspace_epoch
                    && confirmation.remote_revision == approval.remote_revision
                    && confirmation.remote_hash == approval.remote_hash => {}
            (false, None) => {}
            _ => return Err(StoreError::ReconciliationRequired),
        }
        let (current_id, old_id) = frozen_review_registration_ids(
            self,
            &approval.current,
            &approval.old,
            &approval.document,
        )
        .await?;
        let old_storage = self
            .registered_storage_id(old_id, &approval.document)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        let current_storage = self
            .registered_storage_id(current_id, &approval.document)
            .await?;
        let has_open_session = self.sessions.contains_key(&old_storage)
            || current_storage
                .as_ref()
                .is_some_and(|id| self.sessions.contains_key(id));
        let digest = match confirmation {
            Some(confirmation) => frozen_cross_epoch_approval_hash(approval, confirmation),
            None => frozen_approval_hash(approval),
        };
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        validate_frozen_review_registration(&mut tx, current_id, old_id).await?;
        let dismissed: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM frozen_content_dismissals WHERE old_operation_id=?)",
        )
        .bind(&approval.old_operation_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        if dismissed != 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        if let Some(row) =
            sqlx::query("SELECT * FROM frozen_content_reconciliations WHERE old_operation_id=?")
                .bind(&approval.old_operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?
        {
            let stored_hash: String = row.try_get("approval_hash").map_err(storage_error)?;
            let fingerprint: String = row.try_get("view_fingerprint").map_err(storage_error)?;
            if stored_hash != digest || fingerprint != approval.view_fingerprint {
                return Err(StoreError::InvalidRequest);
            }
            let decision = validate_frozen_content_decision(
                &mut tx,
                &row,
                current_id,
                old_id,
                &approval.current,
                &approval.document,
                &approval.old_operation_id,
            )
            .await?;
            tx.commit().await.map_err(storage_error)?;
            return Ok(decision.operation_id);
        }
        if has_open_session {
            return Err(StoreError::SessionAlreadyOpen);
        }
        let current_mapping: Option<String> = sqlx::query_scalar(
            "SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?",
        )
        .bind(current_id)
        .bind(&approval.document)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        if current_mapping != current_storage {
            return Err(StoreError::StaleRegistration);
        }
        if let Some(storage) = &current_mapping {
            let occupied: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM draft_checkpoints WHERE document_id=? UNION ALL SELECT 1 FROM draft_bases WHERE document_id=?) OR EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=?)")
                .bind(storage).bind(storage).bind(current_id).bind(&approval.document).fetch_one(&mut *tx).await.map_err(storage_error)?;
            if occupied != 0 {
                return Err(StoreError::OutboxBlocked);
            }
        }
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_id=?")
            .bind(old_id).bind(&approval.document).bind(&approval.old_operation_id)
            .fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let summary = frozen_review_summary(&mut tx, &row, old_id, &approval.document).await?;
        let old = operation(&row, &approval.old, true)?;
        if summary.kind != FrozenDocumentOperationKind::Content
            || !matches!(old.status, OutboxStatus::Queued | OutboxStatus::Sent)
            || old.status != approval.old_operation_status
            || old.operation_seq != approval.old_operation_seq
            || old.payload_hash != approval.old_operation_hash
            || summary.dependency.is_some()
            || old.expected_revision.as_deref() != Some(approval.base_revision.as_str())
        {
            return Err(StoreError::ReconciliationRequired);
        }
        let draft_row = sqlx::query("SELECT c.generation,c.format_version,c.bytes,b.bytes AS base_bytes,b.revision FROM draft_checkpoints c LEFT JOIN draft_bases b USING(document_id) WHERE c.document_id=?")
            .bind(&old_storage).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::MissingDraft)?;
        let draft = decode_draft(&draft_row, &old_storage)?;
        if draft.generation < old.generation
            || (draft.generation == old.generation && draft.bytes != old.bytes)
            || draft.generation != approval.generation
            || draft.bytes != approval.draft
            || draft.base_bytes != approval.base
            || draft.base_revision != approval.base_revision
            || std::str::from_utf8(&draft.bytes).is_err()
        {
            return Err(StoreError::GenerationMismatch);
        }
        validate_relevant_dismissals(&mut tx, &approval.old.workspace_id, &approval.document)
            .await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_outbox x JOIN client_registrations r ON r.id=x.registration_id WHERE r.status='frozen' AND r.workspace_id=? AND x.document_id=? AND x.status IN ('queued','sent','conflict') AND (x.operation_kind<>'content' OR NOT EXISTS(SELECT 1 FROM frozen_content_dismissals z JOIN client_registrations current ON current.id=z.current_registration_id AND current.status='active' WHERE z.old_operation_id=x.operation_id))")
            .bind(&approval.old.workspace_id).bind(&approval.document).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let other_draft: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM registered_documents d JOIN client_registrations r ON r.id=d.registration_id JOIN draft_checkpoints c ON c.document_id=d.storage_id WHERE r.status='frozen' AND r.workspace_id=? AND d.document_id=? AND r.id<>?)")
            .bind(&approval.old.workspace_id).bind(&approval.document).bind(old_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let active: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND status IN ('queued','sent','conflict'))")
            .bind(current_id).bind(&approval.document).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if count != 1 || other_draft != 0 || active != 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        let index: Option<(String,)> = sqlx::query_as("SELECT cursor FROM remote_index WHERE registration_id=? AND NOT EXISTS(SELECT 1 FROM sync_bootstrap WHERE registration_id=?)")
            .bind(current_id).bind(current_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        if index.as_ref().map(|row| row.0.as_str()) != Some(approval.remote_cursor.as_str()) {
            return Err(StoreError::ReconciliationRequired);
        }
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(current_id)
        .bind(&approval.document)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let node: notryx_protocol::NodeState =
            serde_json::from_str(&raw.ok_or(StoreError::ReconciliationRequired)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if node.node_id.to_string() != approval.document
            || node.deleted
            || node.kind != notryx_protocol::NodeKind::File
            || node.content_rev.as_ref().map(|rev| rev.as_str())
                != Some(approval.remote_revision.as_str())
            || node.content_etag.as_deref()
                != Some(format!("\"{}\"", approval.remote_revision).as_str())
            || node.content_hash.as_deref() != Some(approval.remote_hash.as_str())
            || node
                .size
                .as_deref()
                .and_then(|size| size.parse::<u64>().ok())
                != Some(approval.remote_size)
            || approval.remote_size != approval.remote_bytes.len() as u64
            || payload_hash(&approval.remote_bytes) != approval.remote_hash
            || approval.remote_bytes.len() > super::MAX_DOCUMENT_BYTES
            || std::str::from_utf8(&approval.remote_bytes).is_err()
        {
            return Err(StoreError::ReconciliationRequired);
        }
        if let Some(confirmation) = confirmation
            && (node.parent_id.map(|id| id.to_string()).as_deref()
                != Some(confirmation.current_parent.as_str())
                || node.name != confirmation.current_name
                || frozen_current_document_path(&mut tx, current_id, &node).await?
                    != confirmation.displayed_path)
        {
            return Err(StoreError::ReconciliationRequired);
        }
        let storage = match current_mapping {
            Some(storage) => storage,
            None => {
                let storage = format!("r_{}", uuid::Uuid::new_v4().simple());
                sqlx::query("INSERT INTO registered_documents(registration_id,document_id,storage_id) VALUES (?,?,?)")
                    .bind(current_id).bind(&approval.document).bind(&storage).execute(&mut *tx).await.map_err(storage_error)?;
                storage
            }
        };
        sqlx::query("INSERT INTO draft_checkpoints(document_id,generation,format_version,bytes) VALUES (?,1,1,?)")
            .bind(&storage).bind(&approval.draft).execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("INSERT INTO draft_bases(document_id,bytes,revision) VALUES (?,?,?)")
            .bind(&storage)
            .bind(&approval.remote_bytes)
            .bind(&approval.remote_revision)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        sqlx::query("INSERT INTO outbox_streams(registration_id,next_seq,ack_through) VALUES (?,1,0) ON CONFLICT(registration_id) DO NOTHING")
            .bind(current_id).execute(&mut *tx).await.map_err(storage_error)?;
        let seq: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(current_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let next = seq
            .checked_add(1)
            .filter(|_| seq > 0)
            .ok_or(StoreError::InvalidGeneration)?;
        sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
            .bind(next)
            .bind(current_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let new_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,bytes,payload_hash,payload_len,status) VALUES (?,?,?,'content',?,?,1,?,?,?,?,'queued')")
            .bind(&new_id).bind(current_id).bind(seq).bind(&approval.document).bind(&storage).bind(&approval.remote_revision)
            .bind(&approval.draft).bind(payload_hash(&approval.draft)).bind(approval.draft.len() as i64).execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("INSERT INTO frozen_content_reconciliations(old_operation_id,current_registration_id,new_operation_id,approval_hash,history_hash,view_fingerprint,new_operation_seq,generation,draft_bytes,base_bytes,base_revision,old_payload,old_payload_hash,old_status,old_generation,old_seq,old_expected_revision,old_dependency,old_payload_len,old_payload_retired,new_payload_hash,new_expected_revision) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&approval.old_operation_id).bind(current_id).bind(&new_id).bind(digest).bind("").bind(&approval.view_fingerprint).bind(seq)
            .bind(i64::try_from(approval.generation).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(&draft.bytes).bind(&draft.base_bytes).bind(&draft.base_revision).bind(&old.bytes).bind(&old.payload_hash)
            .bind(if old.status == OutboxStatus::Sent { "sent" } else { "queued" })
            .bind(i64::try_from(old.generation).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(i64::try_from(old.operation_seq).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(&old.expected_revision).bind(&summary.dependency).bind(old.bytes.len() as i64).bind(0_i64)
            .bind(payload_hash(&draft.bytes)).bind(&approval.remote_revision)
            .execute(&mut *tx).await.map_err(storage_error)?;
        let history =
            sqlx::query("SELECT * FROM frozen_content_reconciliations WHERE old_operation_id=?")
                .bind(&approval.old_operation_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let history_hash = frozen_decision_history_hash(&history)?;
        sqlx::query(
            "UPDATE frozen_content_reconciliations SET history_hash=? WHERE old_operation_id=?",
        )
        .bind(history_hash)
        .bind(&approval.old_operation_id)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(new_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenContentDecision {
    pub operation_id: String,
    pub operation_seq: u64,
    pub status: OutboxStatus,
}

fn valid_frozen_fingerprint(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

fn frozen_decision_history_hash(row: &SqliteRow) -> Result<String, StoreError> {
    fn part(hash: &mut Sha256, bytes: &[u8]) {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    let mut hash = Sha256::new();
    part(&mut hash, b"notryx-frozen-content-history-v1");
    for name in [
        "old_operation_id",
        "new_operation_id",
        "approval_hash",
        "view_fingerprint",
        "base_revision",
        "old_payload_hash",
        "old_status",
        "new_payload_hash",
        "new_expected_revision",
    ] {
        let value: String = row.try_get(name).map_err(storage_error)?;
        part(&mut hash, value.as_bytes());
    }
    for name in [
        "current_registration_id",
        "new_operation_seq",
        "generation",
        "old_generation",
        "old_seq",
        "old_payload_len",
        "old_payload_retired",
    ] {
        let value: i64 = row.try_get(name).map_err(storage_error)?;
        part(&mut hash, &value.to_be_bytes());
    }
    for name in ["draft_bytes", "base_bytes", "old_payload"] {
        let value: Vec<u8> = row.try_get(name).map_err(storage_error)?;
        part(&mut hash, &value);
    }
    for name in ["old_expected_revision", "old_dependency"] {
        let value: Option<String> = row.try_get(name).map_err(storage_error)?;
        match value {
            Some(value) => {
                part(&mut hash, &[1]);
                part(&mut hash, value.as_bytes());
            }
            None => part(&mut hash, &[0]),
        }
    }
    Ok(format!(
        "sha256:{}",
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

async fn validate_frozen_content_decision(
    connection: &mut SqliteConnection,
    decision: &SqliteRow,
    current_id: i64,
    old_id: i64,
    current: &RegistrationBinding,
    document: &str,
    old_operation_id: &str,
) -> Result<FrozenContentDecision, StoreError> {
    let dismissed: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM frozen_content_dismissals WHERE old_operation_id=?)",
    )
    .bind(old_operation_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if dismissed != 0 {
        return Err(StoreError::UnsupportedFormat);
    }
    let history_hash: String = decision.try_get("history_hash").map_err(storage_error)?;
    if !valid_frozen_fingerprint(&history_hash)
        || frozen_decision_history_hash(decision)? != history_hash
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let saved_current: i64 = decision
        .try_get("current_registration_id")
        .map_err(storage_error)?;
    let id: String = decision
        .try_get("new_operation_id")
        .map_err(storage_error)?;
    let seq: i64 = decision
        .try_get("new_operation_seq")
        .map_err(storage_error)?;
    let saved_hash: String = decision
        .try_get("new_payload_hash")
        .map_err(storage_error)?;
    let saved_revision: String = decision
        .try_get("new_expected_revision")
        .map_err(storage_error)?;
    let draft: Vec<u8> = decision.try_get("draft_bytes").map_err(storage_error)?;
    let approval_hash: String = decision.try_get("approval_hash").map_err(storage_error)?;
    let parsed: notryx_domain::OperationId =
        id.parse().map_err(|_| StoreError::UnsupportedFormat)?;
    if saved_current != current_id
        || seq <= 0
        || parsed.to_string() != id
        || !valid_frozen_fingerprint(&approval_hash)
        || !valid_content_revision(&saved_revision)
        || draft.len() > super::MAX_DOCUMENT_BYTES
        || payload_hash(&draft) != saved_hash
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let original = sqlx::query("SELECT o.*,d.storage_id AS registered_storage FROM client_outbox o LEFT JOIN registered_documents d ON d.registration_id=o.registration_id AND d.document_id=o.document_id WHERE o.registration_id=? AND o.document_id=? AND o.operation_id=? AND o.operation_kind='content'")
        .bind(old_id).bind(document).bind(old_operation_id).fetch_optional(&mut *connection).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
    let old_storage: Option<String> = original.try_get("storage_id").map_err(storage_error)?;
    let registered: Option<String> = original
        .try_get("registered_storage")
        .map_err(storage_error)?;
    let original_seq: i64 = original.try_get("operation_seq").map_err(storage_error)?;
    let original_generation: i64 = original.try_get("generation").map_err(storage_error)?;
    let original_status: String = original.try_get("status").map_err(storage_error)?;
    let original_abandon: String = original.try_get("abandon_state").map_err(storage_error)?;
    let original_terminal: Option<String> =
        original.try_get("terminal_kind").map_err(storage_error)?;
    let original_expected: Option<String> = original
        .try_get("expected_revision")
        .map_err(storage_error)?;
    let original_dependency: Option<String> =
        original.try_get("dependency").map_err(storage_error)?;
    let original_length: i64 = original.try_get("payload_len").map_err(storage_error)?;
    let original_retired: i64 = original.try_get("payload_retired").map_err(storage_error)?;
    let original_bytes: Vec<u8> = original.try_get("bytes").map_err(storage_error)?;
    let original_hash: String = original.try_get("payload_hash").map_err(storage_error)?;
    if old_storage.is_none()
        || old_storage != registered
        || original_abandon != "none"
        || original_terminal.is_some()
        || original_seq
            != decision
                .try_get::<i64, _>("old_seq")
                .map_err(storage_error)?
        || original_generation
            != decision
                .try_get::<i64, _>("old_generation")
                .map_err(storage_error)?
        || original_status
            != decision
                .try_get::<String, _>("old_status")
                .map_err(storage_error)?
        || original_expected
            != decision
                .try_get::<Option<String>, _>("old_expected_revision")
                .map_err(storage_error)?
        || original_dependency
            != decision
                .try_get::<Option<String>, _>("old_dependency")
                .map_err(storage_error)?
        || original_length
            != decision
                .try_get::<i64, _>("old_payload_len")
                .map_err(storage_error)?
        || original_retired != 0
        || original_retired
            != decision
                .try_get::<i64, _>("old_payload_retired")
                .map_err(storage_error)?
        || original_bytes
            != decision
                .try_get::<Vec<u8>, _>("old_payload")
                .map_err(storage_error)?
        || original_hash
            != decision
                .try_get::<String, _>("old_payload_hash")
                .map_err(storage_error)?
        || original_length != original_bytes.len() as i64
        || payload_hash(&original_bytes) != original_hash
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let row = sqlx::query("SELECT o.*,d.storage_id AS registered_storage FROM client_outbox o LEFT JOIN registered_documents d ON d.registration_id=o.registration_id AND d.document_id=o.document_id WHERE o.registration_id=? AND o.operation_id=?")
        .bind(current_id).bind(&id).fetch_optional(&mut *connection).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
    let actual_document: String = row.try_get("document_id").map_err(storage_error)?;
    let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
    let actual_seq: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let generation: Option<i64> = row.try_get("generation").map_err(storage_error)?;
    let storage: Option<String> = row.try_get("storage_id").map_err(storage_error)?;
    let registered: Option<String> = row.try_get("registered_storage").map_err(storage_error)?;
    let expected: Option<String> = row.try_get("expected_revision").map_err(storage_error)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    let length: i64 = row.try_get("payload_len").map_err(storage_error)?;
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    if actual_document != document
        || kind != "content"
        || actual_seq != seq
        || generation != Some(1)
        || storage.is_none()
        || storage != registered
        || expected.as_deref() != Some(saved_revision.as_str())
        || hash != saved_hash
        || length != draft.len() as i64
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let status = if retired == 0 {
        let item = operation(&row, current, false)?;
        if item.bytes != draft {
            return Err(StoreError::UnsupportedFormat);
        }
        item.status
    } else if retired == 1 {
        let state: String = row.try_get("status").map_err(storage_error)?;
        let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
        let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
        let committed: Option<String> = row.try_get("committed_revision").map_err(storage_error)?;
        let confirmed: Option<i64> = sqlx::query_scalar(
            "SELECT confirmed_through FROM outbox_streams WHERE registration_id=?",
        )
        .bind(current_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage_error)?;
        if state != "committed"
            || terminal.is_some()
            || !bytes.is_empty()
            || confirmed.is_none_or(|value| value < seq)
            || !committed.as_deref().is_some_and(valid_content_revision)
        {
            return Err(StoreError::UnsupportedFormat);
        }
        OutboxStatus::Committed
    } else {
        return Err(StoreError::UnsupportedFormat);
    };
    Ok(FrozenContentDecision {
        operation_id: id,
        operation_seq: seq as u64,
        status,
    })
}

impl ClientStore {
    pub async fn lookup_frozen_content_decision(
        &mut self,
        current: &RegistrationBinding,
        old: &RegistrationBinding,
        document: &str,
        operation_id: &str,
        view_fingerprint: &str,
    ) -> Result<Option<FrozenContentDecision>, StoreError> {
        if !valid_frozen_fingerprint(view_fingerprint)
            || old == current
            || old.workspace_id != current.workspace_id
        {
            return Err(StoreError::InvalidRequest);
        }
        let node: notryx_domain::NodeId =
            document.parse().map_err(|_| StoreError::InvalidRequest)?;
        let operation: notryx_domain::OperationId = operation_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        if node.to_string() != document || operation.to_string() != operation_id {
            return Err(StoreError::InvalidRequest);
        }
        let current_id = self.registration_id(current, false).await?;
        let old_id = self.registration_id(old, false).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row =
            sqlx::query("SELECT * FROM frozen_content_reconciliations WHERE old_operation_id=?")
                .bind(operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
        let result = if let Some(row) = row {
            let saved_fingerprint: String =
                row.try_get("view_fingerprint").map_err(storage_error)?;
            if !valid_frozen_fingerprint(&saved_fingerprint)
                || saved_fingerprint != view_fingerprint
            {
                return Err(StoreError::InvalidRequest);
            }
            Some(
                validate_frozen_content_decision(
                    &mut tx,
                    &row,
                    current_id,
                    old_id,
                    current,
                    document,
                    operation_id,
                )
                .await?,
            )
        } else {
            None
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenContentDecisionStatus {
    pub operation_id: String,
    pub operation_seq: u64,
    pub status: OutboxStatus,
    pub approved_generation: u64,
    pub approved_hash: String,
}

impl ClientStore {
    pub async fn frozen_content_decision_status(
        &mut self,
        current: &RegistrationBinding,
        old: &RegistrationBinding,
        document: &str,
        operation_id: &str,
    ) -> Result<Option<FrozenContentDecisionStatus>, StoreError> {
        if old == current || old.workspace_id != current.workspace_id {
            return Err(StoreError::InvalidRequest);
        }
        let node: notryx_domain::NodeId =
            document.parse().map_err(|_| StoreError::InvalidRequest)?;
        let operation: notryx_domain::OperationId = operation_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        if node.to_string() != document || operation.to_string() != operation_id {
            return Err(StoreError::InvalidRequest);
        }
        let current_id = self.registration_id(current, false).await?;
        let old_id = self.registration_id(old, false).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row =
            sqlx::query("SELECT * FROM frozen_content_reconciliations WHERE old_operation_id=?")
                .bind(operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
        let result = if let Some(row) = row {
            let fingerprint: String = row.try_get("view_fingerprint").map_err(storage_error)?;
            if !valid_frozen_fingerprint(&fingerprint) {
                return Err(StoreError::UnsupportedFormat);
            }
            let decision = validate_frozen_content_decision(
                &mut tx,
                &row,
                current_id,
                old_id,
                current,
                document,
                operation_id,
            )
            .await?;
            let generation: i64 = row.try_get("generation").map_err(storage_error)?;
            let hash: String = row.try_get("new_payload_hash").map_err(storage_error)?;
            if generation <= 0 {
                return Err(StoreError::UnsupportedFormat);
            }
            Some(FrozenContentDecisionStatus {
                operation_id: decision.operation_id,
                operation_seq: decision.operation_seq,
                status: decision.status,
                approved_generation: generation as u64,
                approved_hash: hash,
            })
        } else {
            None
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }
}

#[derive(Clone)]
pub struct FrozenContentDismissApproval {
    pub current: RegistrationBinding,
    pub old: RegistrationBinding,
    pub document: String,
    pub operation_id: String,
    pub operation_seq: u64,
    pub operation_generation: u64,
    pub operation_status: OutboxStatus,
    pub abandon_state: OutboxAbandonState,
    pub expected_revision: Option<String>,
    pub dependency: Option<String>,
    pub payload_hash: String,
    pub payload: Vec<u8>,
    pub draft_generation: u64,
    pub draft: Vec<u8>,
    pub base: Vec<u8>,
    pub base_revision: String,
    pub view_fingerprint: String,
}

fn dismiss_approval_hash(approval: &FrozenContentDismissApproval) -> String {
    let mut hash = Sha256::new();
    fn part(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    part(&mut hash, b"notryx-frozen-content-dismiss-approval-v1");
    for binding in [&approval.current, &approval.old] {
        for value in [
            &binding.server_origin,
            &binding.workspace_id,
            &binding.workspace_epoch,
            &binding.device_id,
            &binding.device_stream_id,
        ] {
            part(&mut hash, value.as_bytes());
        }
    }
    for value in [
        &approval.document,
        &approval.operation_id,
        &approval.payload_hash,
        &approval.base_revision,
        &approval.view_fingerprint,
    ] {
        part(&mut hash, value.as_bytes());
    }
    for value in [
        approval.operation_seq,
        approval.operation_generation,
        approval.draft_generation,
    ] {
        part(&mut hash, &value.to_be_bytes());
    }
    part(
        &mut hash,
        format!("{:?}", approval.operation_status).as_bytes(),
    );
    part(
        &mut hash,
        format!("{:?}", approval.abandon_state).as_bytes(),
    );
    match &approval.expected_revision {
        Some(value) => {
            part(&mut hash, &[1]);
            part(&mut hash, value.as_bytes());
        }
        None => part(&mut hash, &[0]),
    }
    for value in [&approval.payload, &approval.draft, &approval.base] {
        part(&mut hash, value);
    }
    format!(
        "sha256:{}",
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn dismissal_history_hash(row: &SqliteRow) -> Result<String, StoreError> {
    let mut hash = Sha256::new();
    fn part(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    part(&mut hash, b"notryx-frozen-content-dismiss-history-v1");
    for name in [
        "old_operation_id",
        "approval_hash",
        "view_fingerprint",
        "operation_status",
        "abandon_state",
        "payload_hash",
        "base_revision",
    ] {
        let value: String = row.try_get(name).map_err(storage_error)?;
        part(&mut hash, value.as_bytes());
    }
    for name in [
        "current_registration_id",
        "operation_seq",
        "operation_generation",
        "draft_generation",
    ] {
        let value: i64 = row.try_get(name).map_err(storage_error)?;
        part(&mut hash, &value.to_be_bytes());
    }
    for name in ["expected_revision", "dependency"] {
        let value: Option<String> = row.try_get(name).map_err(storage_error)?;
        match value {
            Some(value) => {
                part(&mut hash, &[1]);
                part(&mut hash, value.as_bytes());
            }
            None => part(&mut hash, &[0]),
        }
    }
    for name in ["payload", "draft", "base"] {
        let value: Vec<u8> = row.try_get(name).map_err(storage_error)?;
        part(&mut hash, &value);
    }
    Ok(format!(
        "sha256:{}",
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn dismiss_status_string(status: OutboxStatus) -> Result<&'static str, StoreError> {
    match status {
        OutboxStatus::Queued => Ok("queued"),
        OutboxStatus::Sent => Ok("sent"),
        OutboxStatus::Abandoning => Ok("abandoning"),
        _ => Err(StoreError::ReconciliationRequired),
    }
}

fn dismiss_abandon_string(state: OutboxAbandonState) -> &'static str {
    match state {
        OutboxAbandonState::None => "none",
        OutboxAbandonState::Requested => "requested",
        OutboxAbandonState::Sent => "sent",
    }
}

async fn validate_dismissal(
    connection: &mut SqliteConnection,
    row: &SqliteRow,
    current_id: i64,
    old_id: i64,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
) -> Result<(), StoreError> {
    let applied: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM frozen_content_reconciliations WHERE old_operation_id=?)",
    )
    .bind(operation_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if applied != 0 {
        return Err(StoreError::UnsupportedFormat);
    }
    let saved_current: i64 = row
        .try_get("current_registration_id")
        .map_err(storage_error)?;
    let history: String = row.try_get("history_hash").map_err(storage_error)?;
    let approval: String = row.try_get("approval_hash").map_err(storage_error)?;
    let fingerprint: String = row.try_get("view_fingerprint").map_err(storage_error)?;
    if saved_current != current_id
        || !valid_frozen_fingerprint(&history)
        || !valid_frozen_fingerprint(&approval)
        || !valid_frozen_fingerprint(&fingerprint)
        || dismissal_history_hash(row)? != history
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let old_row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_id=? AND operation_kind='content'")
        .bind(old_id).bind(document).bind(operation_id).fetch_optional(&mut *connection).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
    let summary = frozen_review_summary(connection, &old_row, old_id, document).await?;
    let item = operation(&old_row, old, true)?;
    let saved_status: String = row.try_get("operation_status").map_err(storage_error)?;
    let saved_abandon: String = row.try_get("abandon_state").map_err(storage_error)?;
    let saved_seq: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let saved_generation: i64 = row.try_get("operation_generation").map_err(storage_error)?;
    let expected: Option<String> = row.try_get("expected_revision").map_err(storage_error)?;
    let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
    let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    let payload: Vec<u8> = row.try_get("payload").map_err(storage_error)?;
    let draft_generation: i64 = row.try_get("draft_generation").map_err(storage_error)?;
    if saved_seq <= 0
        || saved_generation <= 0
        || draft_generation <= 0
        || item.operation_seq != saved_seq as u64
        || item.generation != saved_generation as u64
        || dismiss_status_string(item.status)? != saved_status
        || dismiss_abandon_string(item.abandon_state) != saved_abandon
        || item.expected_revision != expected
        || summary.dependency != dependency
        || item.payload_hash != hash
        || item.bytes != payload
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(())
}

async fn validate_relevant_dismissals(
    connection: &mut SqliteConnection,
    workspace_id: &str,
    document_id: &str,
) -> Result<(), StoreError> {
    let records = sqlx::query("SELECT z.*,r.id AS old_registration_id,r.server_origin,r.workspace_id,r.workspace_epoch,r.device_id,r.device_stream_id FROM frozen_content_dismissals z JOIN client_outbox o ON o.operation_id=z.old_operation_id JOIN client_registrations r ON r.id=o.registration_id JOIN client_registrations current ON current.id=z.current_registration_id AND current.status='active' WHERE r.status='frozen' AND r.workspace_id=? AND o.document_id=?")
        .bind(workspace_id).bind(document_id).fetch_all(&mut *connection).await.map_err(storage_error)?;
    for record in records {
        let current_id: i64 = record
            .try_get("current_registration_id")
            .map_err(storage_error)?;
        let old_id: i64 = record
            .try_get("old_registration_id")
            .map_err(storage_error)?;
        let operation_id: String = record.try_get("old_operation_id").map_err(storage_error)?;
        let old = super::registration::binding_from_row(&record)?;
        validate_dismissal(
            connection,
            &record,
            current_id,
            old_id,
            &old,
            document_id,
            &operation_id,
        )
        .await?;
    }
    Ok(())
}

impl ClientStore {
    pub async fn dismiss_frozen_content(
        &mut self,
        approval: &FrozenContentDismissApproval,
    ) -> Result<(), StoreError> {
        if !valid_frozen_fingerprint(&approval.view_fingerprint) {
            return Err(StoreError::InvalidRequest);
        }
        let (current_id, old_id) = frozen_review_registration_ids(
            self,
            &approval.current,
            &approval.old,
            &approval.document,
        )
        .await?;
        let storage = self
            .registered_storage_id(old_id, &approval.document)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        let current_storage = self
            .registered_storage_id(current_id, &approval.document)
            .await?;
        let has_open_session = self.sessions.contains_key(&storage)
            || current_storage
                .as_ref()
                .is_some_and(|id| self.sessions.contains_key(id));
        let digest = dismiss_approval_hash(approval);
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        validate_frozen_review_registration(&mut tx, current_id, old_id).await?;
        let applied: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM frozen_content_reconciliations WHERE old_operation_id=?)",
        )
        .bind(&approval.operation_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        if applied != 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        if let Some(row) =
            sqlx::query("SELECT * FROM frozen_content_dismissals WHERE old_operation_id=?")
                .bind(&approval.operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?
        {
            let saved_hash: String = row.try_get("approval_hash").map_err(storage_error)?;
            let saved_dependency: Option<String> =
                row.try_get("dependency").map_err(storage_error)?;
            if saved_hash != digest || saved_dependency != approval.dependency {
                return Err(StoreError::InvalidRequest);
            }
            validate_dismissal(
                &mut tx,
                &row,
                current_id,
                old_id,
                &approval.old,
                &approval.document,
                &approval.operation_id,
            )
            .await?;
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        if has_open_session {
            return Err(StoreError::SessionAlreadyOpen);
        }
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_id=? AND operation_kind='content'")
            .bind(old_id).bind(&approval.document).bind(&approval.operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let summary = frozen_review_summary(&mut tx, &row, old_id, &approval.document).await?;
        let operation = operation(&row, &approval.old, true)?;
        dismiss_status_string(operation.status)?;
        if operation.operation_seq != approval.operation_seq
            || operation.generation != approval.operation_generation
            || operation.status != approval.operation_status
            || operation.abandon_state != approval.abandon_state
            || operation.expected_revision != approval.expected_revision
            || summary.dependency != approval.dependency
            || operation.payload_hash != approval.payload_hash
            || operation.bytes != approval.payload
        {
            return Err(StoreError::ReconciliationRequired);
        }
        let row = sqlx::query("SELECT c.generation,c.format_version,c.bytes,b.bytes AS base_bytes,b.revision FROM draft_checkpoints c LEFT JOIN draft_bases b USING(document_id) WHERE c.document_id=?")
            .bind(&storage).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::MissingDraft)?;
        let draft = decode_draft(&row, &storage)?;
        if draft.generation < operation.generation
            || (draft.generation == operation.generation && draft.bytes != operation.bytes)
            || (operation.status == OutboxStatus::Sent
                && operation.expected_revision.as_deref() != Some(draft.base_revision.as_str()))
            || draft.generation != approval.draft_generation
            || draft.bytes != approval.draft
            || draft.base_bytes != approval.base
            || draft.base_revision != approval.base_revision
        {
            return Err(StoreError::GenerationMismatch);
        }
        sqlx::query("INSERT INTO frozen_content_dismissals(old_operation_id,current_registration_id,approval_hash,history_hash,view_fingerprint,operation_seq,operation_generation,operation_status,abandon_state,expected_revision,dependency,payload_hash,payload,draft_generation,draft,base,base_revision) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&approval.operation_id).bind(current_id).bind(digest).bind("").bind(&approval.view_fingerprint)
            .bind(i64::try_from(operation.operation_seq).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(i64::try_from(operation.generation).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(dismiss_status_string(operation.status)?).bind(dismiss_abandon_string(operation.abandon_state))
            .bind(&operation.expected_revision).bind(&summary.dependency).bind(&operation.payload_hash).bind(&operation.bytes)
            .bind(i64::try_from(draft.generation).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(&draft.bytes).bind(&draft.base_bytes).bind(&draft.base_revision).execute(&mut *tx).await.map_err(storage_error)?;
        let record =
            sqlx::query("SELECT * FROM frozen_content_dismissals WHERE old_operation_id=?")
                .bind(&approval.operation_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let history = dismissal_history_hash(&record)?;
        sqlx::query("UPDATE frozen_content_dismissals SET history_hash=? WHERE old_operation_id=?")
            .bind(history)
            .bind(&approval.operation_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub async fn frozen_content_dismissal_status(
        &mut self,
        current: &RegistrationBinding,
        old: &RegistrationBinding,
        document: &str,
        operation_id: &str,
    ) -> Result<bool, StoreError> {
        if old == current || old.workspace_id != current.workspace_id {
            return Err(StoreError::InvalidRequest);
        }
        let node: notryx_domain::NodeId =
            document.parse().map_err(|_| StoreError::InvalidRequest)?;
        let parsed: notryx_domain::OperationId = operation_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        if node.to_string() != document || parsed.to_string() != operation_id {
            return Err(StoreError::InvalidRequest);
        }
        let current_id = self.registration_id(current, false).await?;
        let old_id = self.registration_id(old, false).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM frozen_content_dismissals WHERE old_operation_id=?")
            .bind(operation_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;
        let found = if let Some(row) = row {
            let applied: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM frozen_content_reconciliations WHERE old_operation_id=?)")
                .bind(operation_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
            if applied != 0 {
                return Err(StoreError::UnsupportedFormat);
            }
            validate_dismissal(
                &mut tx,
                &row,
                current_id,
                old_id,
                old,
                document,
                operation_id,
            )
            .await?;
            true
        } else {
            false
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(found)
    }
}

impl ClientStore {
    pub async fn lookup_frozen_content_dismissal(
        &mut self,
        current: &RegistrationBinding,
        old: &RegistrationBinding,
        document: &str,
        operation_id: &str,
        fingerprint: &str,
    ) -> Result<Option<()>, StoreError> {
        if old == current
            || old.workspace_id != current.workspace_id
            || !valid_frozen_fingerprint(fingerprint)
        {
            return Err(StoreError::InvalidRequest);
        }
        let node: notryx_domain::NodeId =
            document.parse().map_err(|_| StoreError::InvalidRequest)?;
        let parsed: notryx_domain::OperationId = operation_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        if node.to_string() != document || parsed.to_string() != operation_id {
            return Err(StoreError::InvalidRequest);
        }
        let current_id = self.registration_id(current, false).await?;
        let old_id = self.registration_id(old, false).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM frozen_content_dismissals WHERE old_operation_id=?")
            .bind(operation_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;
        let result = if let Some(row) = row {
            let stored: String = row.try_get("view_fingerprint").map_err(storage_error)?;
            if stored != fingerprint {
                return Err(StoreError::InvalidRequest);
            }
            let applied: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM frozen_content_reconciliations WHERE old_operation_id=?)")
                .bind(operation_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
            if applied != 0 {
                return Err(StoreError::UnsupportedFormat);
            }
            validate_dismissal(
                &mut tx,
                &row,
                current_id,
                old_id,
                old,
                document,
                operation_id,
            )
            .await?;
            Some(())
        } else {
            None
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }
}

#[derive(Clone)]
pub struct FrozenContentCrossEpochConfirmation {
    pub selected_current_document: String,
    pub current_parent: String,
    pub current_name: String,
    pub displayed_path: String,
    pub old_epoch: String,
    pub current_epoch: String,
    pub remote_revision: String,
    pub remote_hash: String,
    pub explicitly_confirmed: bool,
}

fn frozen_cross_epoch_approval_hash(
    approval: &FrozenContentApplyApproval,
    confirmation: &FrozenContentCrossEpochConfirmation,
) -> String {
    let mut hash = Sha256::new();
    fn part(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    part(&mut hash, b"notryx-frozen-content-cross-epoch-v1");
    part(&mut hash, frozen_approval_hash(approval).as_bytes());
    for value in [
        &confirmation.selected_current_document,
        &confirmation.current_parent,
        &confirmation.current_name,
        &confirmation.displayed_path,
        &confirmation.old_epoch,
        &confirmation.current_epoch,
        &confirmation.remote_revision,
        &confirmation.remote_hash,
    ] {
        part(&mut hash, value.as_bytes());
    }
    part(&mut hash, &[u8::from(confirmation.explicitly_confirmed)]);
    format!(
        "sha256:{}",
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

async fn frozen_current_document_path(
    connection: &mut SqliteConnection,
    registration: i64,
    node: &notryx_protocol::NodeState,
) -> Result<String, StoreError> {
    let root: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM remote_index WHERE registration_id=?")
            .bind(registration)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?;
    let root = root.ok_or(StoreError::ReconciliationRequired)?;
    let mut current = node.clone();
    let mut visited = std::collections::HashSet::new();
    let mut parts = Vec::new();
    loop {
        let id = current.node_id.to_string();
        if !visited.insert(id.clone()) || current.deleted {
            return Err(StoreError::UnsupportedFormat);
        }
        if id == root {
            if current.parent_id.is_some()
                || current.kind != notryx_protocol::NodeKind::Folder
                || !current.name.is_empty()
            {
                return Err(StoreError::UnsupportedFormat);
            }
            break;
        }
        if current.name.is_empty()
            || current.name == "."
            || current.name == ".."
            || current.name.contains('/')
        {
            return Err(StoreError::UnsupportedFormat);
        }
        parts.push(current.name.clone());
        let parent = current.parent_id.ok_or(StoreError::UnsupportedFormat)?;
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(parent.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage_error)?;
        let next: notryx_protocol::NodeState =
            serde_json::from_str(&raw.ok_or(StoreError::ReconciliationRequired)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if next.node_id != parent || next.kind != notryx_protocol::NodeKind::Folder {
            return Err(StoreError::UnsupportedFormat);
        }
        current = next;
    }
    parts.reverse();
    Ok(format!("/{}", parts.join("/")))
}
