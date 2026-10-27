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

fn payload_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

fn valid_content_revision(revision: &str) -> bool {
    notryx_protocol::ContentPrecondition::parse(&format!("\"{revision}\""))
        .and_then(|value| value.revision())
        .is_ok_and(|value| value.as_str() == revision)
}

async fn requires_reconciliation(
    connection: &mut SqliteConnection,
    workspace_id: &str,
    document_id: &str,
) -> Result<bool, StoreError> {
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
                      AND x.status = 'committed' AND x.terminal_kind IS NULL
                      AND x.generation = c.generation AND x.bytes = c.bytes
                      AND b.bytes = x.bytes AND b.revision = x.committed_revision
                ))
                OR EXISTS (
                    SELECT 1 FROM client_outbox x
                    WHERE x.registration_id = r.id AND x.document_id = d.document_id
                      AND x.status IN ('queued', 'sent', 'conflict')
                )
              )
        )",
    )
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

fn operation(
    row: &SqliteRow,
    binding: &RegistrationBinding,
    frozen: bool,
) -> Result<OutboxOperation, StoreError> {
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

impl ClientStore {
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
        sqlx::query("UPDATE outbox_streams SET confirmed_through = ? WHERE registration_id = ?")
            .bind(confirmed)
            .bind(registration_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let candidates = sqlx::query("SELECT x.operation_id, x.document_id, x.storage_id FROM client_outbox x WHERE x.registration_id = ? AND x.operation_seq <= ? AND x.status = 'committed' AND x.terminal_kind IS NULL AND x.payload_retired = 0 AND EXISTS (SELECT 1 FROM client_outbox n WHERE n.registration_id = x.registration_id AND n.document_id = x.document_id AND n.status = 'committed' AND n.terminal_kind IS NULL AND n.operation_seq > x.operation_seq) AND NOT EXISTS (SELECT 1 FROM client_outbox d WHERE d.registration_id = x.registration_id AND d.dependency = x.operation_id AND d.status IN ('queued', 'sent', 'conflict')) AND NOT EXISTS (SELECT 1 FROM draft_checkpoints c WHERE c.document_id = x.storage_id AND c.generation > (SELECT n.generation FROM client_outbox n WHERE n.registration_id = x.registration_id AND n.document_id = x.document_id AND n.status = 'committed' AND n.terminal_kind IS NULL ORDER BY n.operation_seq DESC LIMIT 1)) ORDER BY x.operation_seq LIMIT 32")
            .bind(registration_id).bind(confirmed).fetch_all(&mut *transaction).await.map_err(storage_error)?;
        let mut clean_documents = std::collections::HashMap::new();
        for candidate in candidates {
            let id: String = candidate.try_get("operation_id").map_err(storage_error)?;
            let document: String = candidate.try_get("document_id").map_err(storage_error)?;
            let storage_id: String = candidate.try_get("storage_id").map_err(storage_error)?;
            let clean = if let Some(clean) = clean_documents.get(&document) {
                *clean
            } else {
                let newest = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND document_id = ? AND status = 'committed' AND terminal_kind IS NULL ORDER BY operation_seq DESC LIMIT 1")
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
        let registration_status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id = ?")
                .bind(registration_id)
                .fetch_one(&mut self.connection)
                .await
                .map_err(storage_error)?;
        let row = sqlx::query("SELECT operation_id, operation_seq, document_id, generation, status, abandon_state, terminal_kind, payload_len, payload_retired FROM client_outbox WHERE registration_id = ? AND document_id = ? ORDER BY operation_seq DESC LIMIT 1")
            .bind(registration_id).bind(document_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        let latest = row
            .as_ref()
            .map(|row| summary(row, registration_status == "frozen"))
            .transpose()?;
        let conflict: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id = ? AND document_id = ? AND status = 'conflict' AND terminal_kind IS NULL)")
            .bind(registration_id).bind(document_id).fetch_one(&mut self.connection).await.map_err(storage_error)?;
        let failure: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id = ? AND document_id = ? AND status = 'conflict' AND terminal_kind IS NOT NULL)")
            .bind(registration_id).bind(document_id).fetch_one(&mut self.connection).await.map_err(storage_error)?;
        let confirmed = self.confirmed_receipt_through(binding).await?;
        Ok(OutboxDocumentState {
            latest,
            has_conflict: conflict != 0,
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
        let mut rows = sqlx::query("SELECT operation_id, operation_seq, document_id, generation, status, abandon_state, terminal_kind, payload_len, payload_retired FROM client_outbox WHERE registration_id = ? AND operation_seq > ? ORDER BY operation_seq LIMIT ?")
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
        if item.operation_seq != conflict.operation_seq {
            return Err(StoreError::InvalidReceipt);
        }
        if item.status == OutboxStatus::Conflict {
            let etag: Option<String> = row.try_get("conflict_etag").map_err(storage_error)?;
            if item.conflict_revision.as_deref() != Some(&conflict.current_revision)
                || etag.as_deref() != Some(&conflict.current_etag)
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
        let last = sqlx::query("SELECT * FROM client_outbox WHERE registration_id = ? AND document_id = ? ORDER BY operation_seq DESC LIMIT 1")
            .bind(registration_id).bind(document_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?;
        let mut dependency = None;
        if let Some(row) = last {
            let old = operation(&row, binding, false)?;
            if draft.generation < old.generation {
                return Err(StoreError::StaleGeneration);
            }
            if matches!(
                old.status,
                OutboxStatus::Conflict
                    | OutboxStatus::Failed
                    | OutboxStatus::Abandoning
                    | OutboxStatus::Abandoned
            ) {
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
            if old.status == OutboxStatus::Queued {
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
            if old.status == OutboxStatus::Sent {
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

    pub async fn prepare_outbox(
        &mut self,
        binding: &RegistrationBinding,
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
