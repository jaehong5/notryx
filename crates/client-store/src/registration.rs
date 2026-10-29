use super::{
    ClientStore, OpenedDocument, PersistedDraft, SavedCheckpoint, StoreError, storage_error,
    validate_document,
};
use notryx_protocol::NodeState;
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row};
use std::net::IpAddr;
use url::Url;
use uuid::Uuid;

pub(crate) const CREATE_SCHEMA: &str = "
    CREATE TABLE client_registrations (
        id INTEGER PRIMARY KEY,
        server_origin TEXT NOT NULL,
        workspace_id TEXT NOT NULL,
        workspace_epoch TEXT NOT NULL,
        device_id TEXT NOT NULL,
        device_stream_id TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('active', 'frozen')),
        UNIQUE (server_origin, workspace_id, workspace_epoch, device_id, device_stream_id)
    );
    CREATE UNIQUE INDEX one_active_registration ON client_registrations(status) WHERE status = 'active';
    CREATE TABLE registered_documents (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        document_id TEXT NOT NULL,
        storage_id TEXT NOT NULL UNIQUE,
        PRIMARY KEY (registration_id, document_id)
    );
    PRAGMA user_version = 2;
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationBinding {
    pub server_origin: String,
    pub workspace_id: String,
    pub workspace_epoch: String,
    pub device_id: String,
    pub device_stream_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RegisteredDraftInfo {
    pub binding: RegistrationBinding,
    pub document_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyRegisteredSessionSnapshot {
    pub base_revision: String,
    pub checkpoint_generation: Option<u64>,
    pub has_sent: bool,
    pending: Vec<DirtyPendingOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirtyPendingOperation {
    operation_id: String,
    status: String,
    abandon_state: String,
    terminal_kind: Option<String>,
    generation: Option<i64>,
    expected_revision: Option<String>,
    dependency: Option<String>,
}

fn content_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut digest = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        let _ = write!(&mut digest, "{byte:02x}");
    }
    digest
}

pub(crate) fn valid_binding(binding: &RegistrationBinding) -> bool {
    let valid_origin = Url::parse(&binding.server_origin).is_ok_and(|url| {
        let local_ip = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        (url.scheme() == "https" || (url.scheme() == "http" && local_ip))
            && url.username().is_empty()
            && url.password().is_none()
            && url.origin().ascii_serialization() == binding.server_origin
    });
    let valid_id = |value: &str| {
        Uuid::parse_str(value).is_ok_and(|id| {
            id.get_version_num() == 4
                && id.get_variant() == uuid::Variant::RFC4122
                && id.to_string() == value
        })
    };
    valid_origin
        && valid_id(&binding.workspace_id)
        && valid_id(&binding.workspace_epoch)
        && valid_id(&binding.device_id)
        && valid_id(&binding.device_stream_id)
}

pub(crate) fn binding_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<RegistrationBinding, StoreError> {
    let binding = RegistrationBinding {
        server_origin: row.try_get("server_origin").map_err(storage_error)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_error)?,
        workspace_epoch: row.try_get("workspace_epoch").map_err(storage_error)?,
        device_id: row.try_get("device_id").map_err(storage_error)?,
        device_stream_id: row.try_get("device_stream_id").map_err(storage_error)?,
    };
    if !valid_binding(&binding) {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(binding)
}

impl ClientStore {
    pub async fn current_registration(
        &mut self,
    ) -> Result<Option<RegistrationBinding>, StoreError> {
        let row = sqlx::query("SELECT server_origin, workspace_id, workspace_epoch, device_id, device_stream_id FROM client_registrations WHERE status = 'active'")
            .fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        row.map(|row| binding_from_row(&row)).transpose()
    }

    pub async fn activate_registration(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<(), StoreError> {
        if !valid_binding(binding) {
            return Err(StoreError::InvalidRegistration);
        }
        if self.current_registration().await?.as_ref() == Some(binding) {
            return Ok(());
        }
        if !self.sessions.is_empty() {
            return Err(StoreError::OpenEditSessions);
        }
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let existing: Option<String> = sqlx::query_scalar("SELECT status FROM client_registrations WHERE server_origin = ? AND workspace_id = ? AND workspace_epoch = ? AND device_id = ? AND device_stream_id = ?")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .bind(&binding.device_id).bind(&binding.device_stream_id)
            .fetch_optional(&mut *transaction).await.map_err(storage_error)?;
        if existing.is_some() {
            return Err(StoreError::StaleRegistration);
        }
        sqlx::query("UPDATE client_registrations SET status = 'frozen' WHERE status = 'active'")
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        sqlx::query("INSERT INTO client_registrations (server_origin, workspace_id, workspace_epoch, device_id, device_stream_id, status) VALUES (?, ?, ?, ?, ?, 'active')")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .bind(&binding.device_id).bind(&binding.device_stream_id)
            .execute(&mut *transaction).await.map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok(())
    }

    pub(crate) async fn registration_id(
        &mut self,
        binding: &RegistrationBinding,
        active: bool,
    ) -> Result<i64, StoreError> {
        if !valid_binding(binding) {
            return Err(StoreError::InvalidRegistration);
        }
        let row = sqlx::query("SELECT id, status FROM client_registrations WHERE server_origin = ? AND workspace_id = ? AND workspace_epoch = ? AND device_id = ? AND device_stream_id = ?")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .bind(&binding.device_id).bind(&binding.device_stream_id)
            .fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        let row = row.ok_or(StoreError::StaleRegistration)?;
        let status: String = row.try_get("status").map_err(storage_error)?;
        if active && status != "active" {
            return Err(StoreError::StaleRegistration);
        }
        row.try_get("id").map_err(storage_error)
    }

    pub(crate) async fn registered_storage_id(
        &mut self,
        registration_id: i64,
        document_id: &str,
    ) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar("SELECT storage_id FROM registered_documents WHERE registration_id = ? AND document_id = ?")
            .bind(registration_id).bind(document_id)
            .fetch_optional(&mut self.connection).await.map_err(storage_error)
    }

    async fn delete_blocks_edit(
        &mut self,
        registration_id: i64,
        document_id: &str,
    ) -> Result<bool, StoreError> {
        let pending: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='delete' AND status IN ('queued','sent'))")
            .bind(registration_id).bind(document_id).fetch_one(&mut self.connection).await.map_err(storage_error)?;
        if pending != 0 {
            return Ok(true);
        }
        let receipt: Option<(Option<String>,)> = sqlx::query_as("SELECT receipt_json FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='delete' AND status='committed' AND terminal_kind IS NULL ORDER BY operation_seq DESC LIMIT 1")
            .bind(registration_id).bind(document_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        let Some((receipt,)) = receipt else {
            return Ok(false);
        };
        let receipt = receipt.ok_or(StoreError::UnsupportedFormat)?;
        let parsed: notryx_protocol::OperationReceipt =
            serde_json::from_str(&receipt).map_err(|_| StoreError::UnsupportedFormat)?;
        let notryx_protocol::OperationReceipt::Deleted {
            node, commit_seq, ..
        } = parsed
        else {
            return Err(StoreError::UnsupportedFormat);
        };
        if node.node_id.to_string() != document_id
            || !node.deleted
            || commit_seq.get() == 0
            || node.last_change_seq != commit_seq
        {
            return Err(StoreError::UnsupportedFormat);
        }
        let current: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration_id)
        .bind(document_id)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        let Some(current) = current else {
            return Ok(true);
        };
        let current: notryx_protocol::NodeState =
            serde_json::from_str(&current).map_err(|_| StoreError::UnsupportedFormat)?;
        if current.node_id.to_string() != document_id {
            return Err(StoreError::UnsupportedFormat);
        }
        Ok(current.deleted || current.last_change_seq.get() <= commit_seq.get())
    }

    pub async fn open_registered_document(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        base: &[u8],
        base_revision: &str,
    ) -> Result<OpenedDocument, StoreError> {
        validate_document(document_id, base, base_revision)?;
        let registration_id = self.registration_id(binding, true).await?;
        if self
            .delete_blocks_edit(registration_id, document_id)
            .await?
        {
            return Err(StoreError::OutboxBlocked);
        }
        let storage_id = match self
            .registered_storage_id(registration_id, document_id)
            .await?
        {
            Some(storage_id) => storage_id,
            None => loop {
                let candidate = format!("r_{}", Uuid::new_v4().simple());
                let used: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM draft_checkpoints WHERE document_id = ? UNION ALL SELECT 1 FROM registered_documents WHERE storage_id = ?)")
                    .bind(&candidate).bind(&candidate).fetch_one(&mut self.connection).await.map_err(storage_error)?;
                if used == 0 && !self.sessions.contains_key(&candidate) {
                    sqlx::query("INSERT INTO registered_documents (registration_id, document_id, storage_id) VALUES (?, ?, ?)")
                        .bind(registration_id).bind(document_id).bind(&candidate)
                        .execute(&mut self.connection).await.map_err(storage_error)?;
                    break candidate;
                }
            },
        };
        let mut opened = self.open_document(&storage_id, base, base_revision).await?;
        opened.document_id = document_id.to_owned();
        Ok(opened)
    }

    pub async fn save_registered_checkpoint(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
        generation: u64,
        bytes: &[u8],
    ) -> Result<SavedCheckpoint, StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let storage_id = self
            .registered_storage_id(registration_id, document_id)
            .await?
            .ok_or(StoreError::StaleSession)?;
        self.save_checkpoint(&storage_id, edit_session_id, generation, bytes)
            .await
    }

    pub async fn close_registered_document(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
    ) -> Result<(), StoreError> {
        let registration_id = self.registration_id(binding, true).await?;
        let storage_id = self
            .registered_storage_id(registration_id, document_id)
            .await?
            .ok_or(StoreError::StaleSession)?;
        self.close_document(&storage_id, edit_session_id)
    }

    /// Captures the active session and unresolved operations without changing stored state.
    pub async fn dirty_registered_session_snapshot(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
    ) -> Result<Option<DirtyRegisteredSessionSnapshot>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let Some(storage_id) = self
            .registered_storage_id(registration, document_id)
            .await?
        else {
            return Ok(None);
        };
        let Some(session) = self.sessions.get(&storage_id) else {
            return Ok(None);
        };
        if session.id != edit_session_id {
            return Ok(None);
        }
        let base_revision = session.base_revision.clone();
        let checkpoint = sqlx::query(
            "SELECT generation,format_version FROM draft_checkpoints WHERE document_id=?",
        )
        .bind(&storage_id)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        let checkpoint_generation = match checkpoint {
            Some(row) => {
                let generation: i64 = row.try_get("generation").map_err(storage_error)?;
                let version: i64 = row.try_get("format_version").map_err(storage_error)?;
                if generation <= 0 || version != 1 {
                    return Err(StoreError::UnsupportedFormat);
                }
                Some(u64::try_from(generation).map_err(|_| StoreError::UnsupportedFormat)?)
            }
            None => None,
        };
        let rows = sqlx::query(
            "SELECT operation_id,status,abandon_state,terminal_kind,generation,expected_revision,dependency
             FROM client_outbox WHERE registration_id=? AND document_id=?
                 AND status IN ('queued','sent','conflict') ORDER BY operation_seq",
        )
        .bind(registration)
        .bind(document_id)
        .fetch_all(&mut self.connection)
        .await
        .map_err(storage_error)?;
        let pending: Vec<DirtyPendingOperation> = rows
            .iter()
            .map(|row| {
                Ok(DirtyPendingOperation {
                    operation_id: row.try_get("operation_id").map_err(storage_error)?,
                    status: row.try_get("status").map_err(storage_error)?,
                    abandon_state: row.try_get("abandon_state").map_err(storage_error)?,
                    terminal_kind: row.try_get("terminal_kind").map_err(storage_error)?,
                    generation: row.try_get("generation").map_err(storage_error)?,
                    expected_revision: row.try_get("expected_revision").map_err(storage_error)?,
                    dependency: row.try_get("dependency").map_err(storage_error)?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        let has_sent = pending
            .iter()
            .any(|item| item.status == "sent" || item.abandon_state == "sent");
        Ok(Some(DirtyRegisteredSessionSnapshot {
            base_revision,
            checkpoint_generation,
            has_sent,
            pending,
        }))
    }

    /// Returns the base revision only for a never-edited active session without outbox history.
    pub async fn pristine_registered_session_revision(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let Some(storage_id) = self
            .registered_storage_id(registration, document_id)
            .await?
        else {
            return Ok(None);
        };
        let Some(session) = self.sessions.get(&storage_id) else {
            return Ok(None);
        };
        if session.id != edit_session_id {
            return Ok(None);
        }
        let base_revision = session.base_revision.clone();
        let occupied: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM draft_checkpoints WHERE document_id=?)
                OR EXISTS(SELECT 1 FROM draft_bases WHERE document_id=?)
                OR EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=?)",
        )
        .bind(&storage_id)
        .bind(&storage_id)
        .bind(registration)
        .bind(document_id)
        .fetch_one(&mut self.connection)
        .await
        .map_err(storage_error)?;
        Ok((occupied == 0).then_some(base_revision))
    }

    /// A confirmed checkpoint remains recoverable until a new local edit is saved.
    pub async fn refreshable_registered_session_revision(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
    ) -> Result<Option<String>, StoreError> {
        if let Some(revision) = self
            .pristine_registered_session_revision(binding, document_id, edit_session_id)
            .await?
        {
            return Ok(Some(revision));
        }
        let registration = self.registration_id(binding, true).await?;
        let Some(storage_id) = self
            .registered_storage_id(registration, document_id)
            .await?
        else {
            return Ok(None);
        };
        let Some(session) = self.sessions.get(&storage_id) else {
            return Ok(None);
        };
        if session.id != edit_session_id {
            return Ok(None);
        }
        let revision = session.base_revision.clone();
        let node_id = document_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        let Some(node) = self.current_remote_file(binding, node_id).await? else {
            return Ok(None);
        };
        let Some(remote_revision) = node.content_rev else {
            return Ok(None);
        };
        if !self
            .has_confirmed_older_checkpoint(binding, document_id, remote_revision.as_str())
            .await?
        {
            return Ok(None);
        }
        Ok(Some(revision))
    }

    /// Preserve the durable checkpoint; the next edit installs its new base atomically.
    pub async fn refresh_clean_registered_session(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
        expected_base_revision: &str,
        expected_node: &NodeState,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if expected_node.node_id.to_string() != document_id {
            return Err(StoreError::InvalidRequest);
        }
        let Some(base_revision) = self
            .refreshable_registered_session_revision(binding, document_id, edit_session_id)
            .await?
        else {
            return Ok(None);
        };
        let Some(revision) = expected_node.content_rev.as_ref() else {
            return Ok(None);
        };
        if base_revision != expected_base_revision || revision.as_str() == base_revision {
            return Ok(None);
        }
        if !self.remote_content_matches(binding, expected_node).await? {
            return Ok(None);
        }
        let registration = self.registration_id(binding, true).await?;
        let storage_id = self
            .registered_storage_id(registration, document_id)
            .await?
            .ok_or(StoreError::StaleSession)?;
        let cached = sqlx::query(
            "SELECT c.content_rev,c.content_hash,c.size,c.bytes,n.content_ready
             FROM cached_content c JOIN remote_nodes n
               ON n.registration_id=c.registration_id AND n.node_id=c.node_id
             WHERE c.registration_id=? AND c.node_id=?",
        )
        .bind(registration)
        .bind(document_id)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        let Some(cached) = cached else {
            return Ok(None);
        };
        let cached_revision: String = cached.try_get("content_rev").map_err(storage_error)?;
        let cached_hash: String = cached.try_get("content_hash").map_err(storage_error)?;
        let cached_size: String = cached.try_get("size").map_err(storage_error)?;
        let bytes: Vec<u8> = cached.try_get("bytes").map_err(storage_error)?;
        let ready: i64 = cached.try_get("content_ready").map_err(storage_error)?;
        let digest = content_digest(&bytes);
        if !matches!(ready, 0 | 1)
            || cached_size.parse::<usize>().ok() != Some(bytes.len())
            || digest != cached_hash
        {
            return Err(StoreError::UnsupportedFormat);
        }
        if ready != 1
            || cached_revision != revision.as_str()
            || expected_node.content_hash.as_deref() != Some(cached_hash.as_str())
            || expected_node.size.as_deref() != Some(cached_size.as_str())
            || validate_document(document_id, &bytes, revision.as_str()).is_err()
            || std::str::from_utf8(&bytes).is_err()
        {
            return Ok(None);
        }
        let session = self
            .sessions
            .get_mut(&storage_id)
            .ok_or(StoreError::StaleSession)?;
        if session.id != edit_session_id || session.base_revision != expected_base_revision {
            return Ok(None);
        }
        session.refreshed_base = true;
        session.base.clone_from(&bytes);
        session.base_revision = revision.as_str().to_owned();
        Ok(Some(bytes))
    }

    async fn recovery_storage_id(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<String, StoreError> {
        let registration_id = self.registration_id(binding, false).await?;
        let frozen: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id = ? AND status = 'frozen')",
        )
        .bind(registration_id)
        .fetch_one(&mut self.connection)
        .await
        .map_err(storage_error)?;
        if frozen == 0 {
            return Err(StoreError::StaleRegistration);
        }
        self.registered_storage_id(registration_id, document_id)
            .await?
            .ok_or(StoreError::MissingDraft)
    }

    async fn active_delete_blocks_clean_recovery(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<bool, StoreError> {
        let pending: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE r.status='active' AND r.workspace_id=? AND o.document_id=? AND o.operation_kind='delete' AND o.status IN ('queued','sent'))")
            .bind(&binding.workspace_id)
            .bind(document_id)
            .fetch_one(&mut self.connection)
            .await
            .map_err(storage_error)?;
        Ok(pending != 0)
    }

    pub async fn open_recovery_draft(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<OpenedDocument, StoreError> {
        let storage_id = self.recovery_storage_id(binding, document_id).await?;
        let draft = self
            .read_checkpoint(&storage_id)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        if draft.bytes == draft.base_bytes
            && self
                .active_delete_blocks_clean_recovery(binding, document_id)
                .await?
        {
            return Err(StoreError::OutboxBlocked);
        }
        let mut opened = self
            .open_document(&storage_id, &draft.base_bytes, &draft.base_revision)
            .await?;
        opened.document_id = document_id.to_owned();
        Ok(opened)
    }

    pub async fn save_recovery_checkpoint(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
        generation: u64,
        bytes: &[u8],
    ) -> Result<SavedCheckpoint, StoreError> {
        let storage_id = self.recovery_storage_id(binding, document_id).await?;
        self.save_checkpoint(&storage_id, edit_session_id, generation, bytes)
            .await
    }

    pub async fn close_recovery_draft(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
    ) -> Result<(), StoreError> {
        let storage_id = self.recovery_storage_id(binding, document_id).await?;
        self.close_document(&storage_id, edit_session_id)
    }

    pub async fn read_registered_checkpoint(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<Option<PersistedDraft>, StoreError> {
        let registration_id = self.registration_id(binding, false).await?;
        match self
            .registered_storage_id(registration_id, document_id)
            .await?
        {
            Some(storage_id) => self.read_checkpoint(&storage_id).await,
            None => Ok(None),
        }
    }

    pub async fn has_confirmed_older_checkpoint(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        revision: &str,
    ) -> Result<bool, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let active: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id=? AND status='active')",
        )
        .bind(registration)
        .fetch_one(&mut *transaction)
        .await
        .map_err(storage_error)?;
        if active != 1 {
            return Err(StoreError::StaleRegistration);
        }
        let confirmed: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(committed.operation_seq) FROM registered_documents d
                JOIN draft_checkpoints c ON c.document_id=d.storage_id
                JOIN draft_bases b ON b.document_id=d.storage_id
                JOIN client_outbox committed ON committed.registration_id=d.registration_id
                    AND committed.document_id=d.document_id AND committed.operation_kind='content'
                    AND committed.status='committed' AND committed.terminal_kind IS NULL
                    AND committed.generation=c.generation AND committed.payload_retired=0
                    AND committed.bytes=c.bytes AND committed.committed_revision=b.revision
                JOIN outbox_streams s ON s.registration_id=d.registration_id
                WHERE d.registration_id=? AND d.document_id=? AND c.bytes=b.bytes
                    AND committed.operation_seq<=s.confirmed_through AND b.revision<>?
                    AND NOT EXISTS(SELECT 1 FROM client_outbox earlier
                        WHERE earlier.registration_id=d.registration_id AND earlier.document_id=d.document_id
                        AND earlier.operation_kind='content' AND earlier.status='committed'
                        AND earlier.terminal_kind IS NULL AND earlier.expected_revision=?
                        AND earlier.operation_seq<=committed.operation_seq)",
        )
        .bind(registration)
        .bind(document_id)
        .bind(revision)
        .bind(revision)
        .fetch_one(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let Some(confirmed) = confirmed else {
            transaction.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        let confirmed = u64::try_from(confirmed)
            .ok()
            .filter(|value| *value > 0)
            .ok_or(StoreError::UnsupportedFormat)?;
        type PendingConflictRow = (
            String,
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
        );
        let pending: Vec<PendingConflictRow> = sqlx::query_as(
            "SELECT o.operation_id,o.operation_kind,o.status,o.terminal_kind,
                    i.completed_generation,i.completed_through
                 FROM client_outbox o LEFT JOIN content_conflict_intents i
                   ON i.registration_id=o.registration_id AND i.operation_id=o.operation_id
                 WHERE o.registration_id=? AND o.document_id=?
                   AND o.status IN ('queued','sent','conflict') ORDER BY o.operation_seq",
        )
        .bind(registration)
        .bind(document_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let mut blocked = false;
        for (operation_id, kind, status, terminal, generation, through) in pending {
            if kind != "content" || status != "conflict" || terminal.is_some() {
                blocked = true;
                continue;
            }
            if generation.is_none() && through.is_none() {
                blocked = true;
                continue;
            }
            let Some((resolved, completed_through)) =
                crate::content_conflict::verified_completed_choose_remote(
                    &mut transaction,
                    registration,
                    binding,
                    &operation_id,
                )
                .await?
            else {
                return Err(StoreError::UnsupportedFormat);
            };
            if resolved.document_id != document_id
                || resolved.operation_id != operation_id
                || completed_through >= confirmed
            {
                return Err(StoreError::UnsupportedFormat);
            }
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(!blocked)
    }

    pub async fn try_open_registered_draft(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<Option<(OpenedDocument, bool)>, StoreError> {
        let registration_id = self.registration_id(binding, false).await?;
        let Some(storage_id) = self
            .registered_storage_id(registration_id, document_id)
            .await?
        else {
            return Ok(None);
        };
        let Some(draft) = self.read_checkpoint(&storage_id).await? else {
            return Ok(None);
        };
        let active = self.current_registration().await?.as_ref() == Some(binding);
        if active {
            self.registration_id(binding, true).await?;
            validate_document(document_id, &draft.base_bytes, &draft.base_revision)?;
            if draft.bytes == draft.base_bytes
                && self
                    .delete_blocks_edit(registration_id, document_id)
                    .await?
            {
                return Err(StoreError::OutboxBlocked);
            }
        } else {
            self.recovery_storage_id(binding, document_id).await?;
            if draft.bytes == draft.base_bytes
                && self
                    .active_delete_blocks_clean_recovery(binding, document_id)
                    .await?
            {
                return Err(StoreError::OutboxBlocked);
            }
        }
        if self.sessions.contains_key(&storage_id) {
            return Err(StoreError::SessionAlreadyOpen);
        }
        let mut opened = self.install_edit_session(
            &storage_id,
            draft.generation,
            draft.bytes,
            draft.base_bytes,
            draft.base_revision,
        );
        opened.document_id = document_id.to_owned();
        Ok(Some((opened, active)))
    }

    pub async fn list_registered_drafts(&mut self) -> Result<Vec<RegisteredDraftInfo>, StoreError> {
        let rows = sqlx::query("SELECT r.server_origin, r.workspace_id, r.workspace_epoch, r.device_id, r.device_stream_id, d.document_id, c.generation FROM registered_documents d JOIN client_registrations r ON r.id = d.registration_id JOIN draft_checkpoints c ON c.document_id = d.storage_id ORDER BY r.id, d.document_id")
            .fetch_all(&mut self.connection).await.map_err(storage_error)?;
        rows.iter()
            .map(|row| {
                let generation: i64 = row.try_get("generation").map_err(storage_error)?;
                if generation <= 0 {
                    return Err(StoreError::UnsupportedFormat);
                }
                Ok(RegisteredDraftInfo {
                    binding: binding_from_row(row)?,
                    document_id: row.try_get("document_id").map_err(storage_error)?,
                    generation: generation as u64,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod pristine_refresh_tests {
    use super::*;
    use notryx_protocol::{InventoryPage, ServerInfo};
    use serde_json::json;

    const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
    const DOCUMENT: &str = "019a1000-0000-7000-8000-000000000002";

    async fn fixture() -> (
        tempfile::TempDir,
        ClientStore,
        RegistrationBinding,
        NodeState,
        String,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let binding = RegistrationBinding {
            server_origin: "https://workspace.example".into(),
            workspace_id: Uuid::new_v4().to_string(),
            workspace_epoch: Uuid::new_v4().to_string(),
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
        };
        let info: ServerInfo = serde_json::from_value(json!({
            "protocol": "mdw-sync/1.2", "workspace_id": binding.workspace_id,
            "workspace_epoch": binding.workspace_epoch, "root_id": ROOT,
            "head_seq": "1", "journal_floor_seq": "0", "ready": true,
            "max_document_bytes": 5242880, "max_attachment_bytes": 26214400
        }))
        .unwrap();
        let root: NodeState = serde_json::from_value(json!({
            "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder", "deleted": false,
            "content_rev": null, "content_etag": null, "meta_rev": "root-meta",
            "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"", "content_hash": null,
            "size": null, "children_generation": "1", "last_change_seq": "1"
        }))
        .unwrap();
        let remote = b"remote source\n";
        let node: NodeState = serde_json::from_value(json!({
            "node_id": DOCUMENT, "parent_id": ROOT, "name": "document.md", "kind": "file", "deleted": false,
            "content_rev": "content-2", "content_etag": "\"content-2\"", "meta_rev": "meta-1",
            "meta_etag": "\"meta-1\"", "node_etag": "\"node-2\"",
            "content_hash": content_digest(remote),
            "size": remote.len().to_string(), "children_generation": "0", "last_change_seq": "1"
        }))
        .unwrap();
        let mut store = ClientStore::open(directory.path().join("drafts.db"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let stage = store.begin_bootstrap(&binding, &info).await.unwrap();
        store
            .bootstrap_inventory(
                &binding,
                &stage,
                &InventoryPage {
                    workspace_epoch: info.workspace_epoch,
                    nodes: vec![root, node.clone()],
                    next_after_id: None,
                },
            )
            .await
            .unwrap();
        store
            .bootstrap_finish_inventory(&binding, &stage, &info)
            .await
            .unwrap();
        store
            .bootstrap_publish(&binding, &stage, &info)
            .await
            .unwrap();
        let opened = store
            .open_registered_document(&binding, DOCUMENT, b"old source\n", "content-1")
            .await
            .unwrap();
        (directory, store, binding, node, opened.edit_session_id)
    }

    #[tokio::test]
    async fn dirty_session_snapshot_detects_checkpoint_and_content_send_without_mutation() {
        let (_directory, mut store, binding, _node, session) = fixture().await;
        assert_eq!(
            store
                .dirty_registered_session_snapshot(&binding, DOCUMENT, "other")
                .await
                .unwrap(),
            None
        );
        let initial = store
            .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial.base_revision, "content-1");
        assert_eq!(initial.checkpoint_generation, None);
        assert!(!initial.has_sent);
        store
            .save_registered_checkpoint(&binding, DOCUMENT, &session, 1, b"local edit\n")
            .await
            .unwrap();
        let checkpoint = store
            .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.checkpoint_generation, Some(1));
        assert_ne!(checkpoint, initial);
        store.queue_checkpoint(&binding, DOCUMENT, 1).await.unwrap();
        let queued = store
            .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
            .await
            .unwrap()
            .unwrap();
        assert!(!queued.has_sent);
        assert_ne!(queued, checkpoint);
        store
            .prepare_pending_outbox(&binding)
            .await
            .unwrap()
            .unwrap();
        let before_draft = store
            .read_registered_checkpoint(&binding, DOCUMENT)
            .await
            .unwrap();
        let before_outbox = store.list_outbox(&binding, 0, 100).await.unwrap();
        let sent = store
            .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
            .await
            .unwrap()
            .unwrap();
        assert!(sent.has_sent);
        assert_ne!(sent, queued);
        assert_eq!(
            store
                .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
                .await
                .unwrap(),
            Some(sent)
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap(),
            before_draft
        );
        assert_eq!(
            store.list_outbox(&binding, 0, 100).await.unwrap(),
            before_outbox
        );
    }

    #[tokio::test]
    async fn dirty_session_snapshot_includes_sent_restore_for_the_same_node() {
        let (_directory, mut store, binding, node, session) = fixture().await;
        let request: notryx_protocol::RestoreNodeRequest = serde_json::from_value(json!({
            "expected_meta_rev": "meta-1", "parent_id": ROOT, "name": "document.md"
        }))
        .unwrap();
        store
            .queue_restore(&binding, node.node_id, &request)
            .await
            .unwrap();
        let queued = store
            .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
            .await
            .unwrap()
            .unwrap();
        assert!(!queued.has_sent);
        store
            .prepare_pending_outbox(&binding)
            .await
            .unwrap()
            .unwrap();
        let before = store
            .list_restore_operations(&binding, 0, 100)
            .await
            .unwrap();
        let sent = store
            .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
            .await
            .unwrap()
            .unwrap();
        assert!(sent.has_sent);
        assert_ne!(sent, queued);
        assert_eq!(
            store
                .list_restore_operations(&binding, 0, 100)
                .await
                .unwrap(),
            before
        );
        assert!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn dirty_session_snapshot_rejects_closed_or_frozen_session() {
        let (_directory, mut store, binding, _node, session) = fixture().await;
        store
            .close_registered_document(&binding, DOCUMENT, &session)
            .await
            .unwrap();
        assert_eq!(
            store
                .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
                .await
                .unwrap(),
            None
        );
        let replacement = RegistrationBinding {
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
            ..binding.clone()
        };
        store.activate_registration(&replacement).await.unwrap();
        assert_eq!(
            store
                .dirty_registered_session_snapshot(&binding, DOCUMENT, &session)
                .await,
            Err(StoreError::StaleRegistration)
        );
    }

    #[tokio::test]
    async fn confirmed_checkpoint_refresh_preserves_old_draft_and_rebases_next_edit() {
        let (_directory, mut store, binding, node, session) = fixture().await;
        store
            .save_registered_checkpoint(&binding, DOCUMENT, &session, 1, b"saved local\n")
            .await
            .unwrap();
        let op = store.queue_checkpoint(&binding, DOCUMENT, 1).await.unwrap();
        store
            .prepare_pending_outbox(&binding)
            .await
            .unwrap()
            .unwrap();
        store
            .apply_outbox_commit(
                &binding,
                &crate::OutboxCommit {
                    operation_id: op.operation_id.clone(),
                    operation_seq: op.operation_seq,
                    workspace_epoch: binding.workspace_epoch.clone(),
                    document_id: DOCUMENT.into(),
                    content_revision: "saved-revision".into(),
                    content_hash: op.payload_hash.clone(),
                },
            )
            .await
            .unwrap();
        store
            .cache_content(&binding, &node, b"remote source\n")
            .await
            .unwrap();
        assert!(
            store
                .refresh_clean_registered_session(
                    &binding,
                    DOCUMENT,
                    &session,
                    "saved-revision",
                    &node
                )
                .await
                .unwrap()
                .is_none()
        );
        store.confirm_receipt_ack(&binding, &serde_json::from_value(json!({
            "workspace_epoch": binding.workspace_epoch, "device_stream_id": binding.device_stream_id,
            "last_seq": "1", "retired_through": "1"
        })).unwrap()).await.unwrap();
        let preserved = store
            .read_registered_checkpoint(&binding, DOCUMENT)
            .await
            .unwrap();
        assert_eq!(
            store
                .refresh_clean_registered_session(
                    &binding,
                    DOCUMENT,
                    &session,
                    "saved-revision",
                    &node
                )
                .await
                .unwrap(),
            Some(b"remote source\n".to_vec())
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap(),
            preserved
        );
        sqlx::query("CREATE TRIGGER fail_remote_base BEFORE UPDATE ON draft_bases BEGIN SELECT RAISE(ABORT,'injected'); END")
            .execute(&mut store.connection).await.unwrap();
        assert!(
            store
                .save_registered_checkpoint(
                    &binding,
                    DOCUMENT,
                    &session,
                    2,
                    b"remote source plus local\n"
                )
                .await
                .is_err()
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap(),
            preserved
        );
        sqlx::query("DROP TRIGGER fail_remote_base")
            .execute(&mut store.connection)
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &binding,
                DOCUMENT,
                &session,
                2,
                b"remote source plus local\n",
            )
            .await
            .unwrap();
        let draft = store
            .read_registered_checkpoint(&binding, DOCUMENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.base_revision, "content-2");
        assert_eq!(draft.base_bytes, b"remote source\n");
        let next = store.queue_checkpoint(&binding, DOCUMENT, 2).await.unwrap();
        assert_eq!(next.expected_revision.as_deref(), Some("content-2"));
        assert!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-2", &node)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn pristine_refresh_updates_session_base_without_creating_a_checkpoint() {
        let (_directory, mut store, binding, node, session) = fixture().await;
        assert!(
            store
                .cache_content(&binding, &node, b"remote source\n")
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .pristine_registered_session_revision(&binding, DOCUMENT, &session)
                .await
                .unwrap(),
            Some("content-1".into())
        );
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &node)
                .await
                .unwrap(),
            Some(b"remote source\n".to_vec())
        );
        assert!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &node)
                .await
                .unwrap(),
            None
        );
        store
            .save_registered_checkpoint(&binding, DOCUMENT, &session, 1, b"local edit\n")
            .await
            .unwrap();
        let draft = store
            .read_registered_checkpoint(&binding, DOCUMENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local edit\n");
        assert_eq!(draft.base_bytes, b"remote source\n");
        assert_eq!(draft.base_revision, "content-2");
    }

    #[tokio::test]
    async fn stale_session_metadata_and_damaged_cache_cannot_change_the_base() {
        let (_directory, mut store, binding, node, session) = fixture().await;
        assert_eq!(
            store
                .pristine_registered_session_revision(&binding, DOCUMENT, "other")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &node)
                .await
                .unwrap(),
            None
        );
        assert!(
            store
                .cache_content(&binding, &node, b"remote source\n")
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "other", &node)
                .await
                .unwrap(),
            None
        );
        let mut stale = node.clone();
        stale.content_rev = Some(notryx_domain::ContentRevision::new("content-3").unwrap());
        stale.content_etag = Some("\"content-3\"".into());
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &stale)
                .await
                .unwrap(),
            None
        );
        sqlx::query("UPDATE cached_content SET bytes=? WHERE node_id=?")
            .bind(b"corrupt source".as_slice())
            .bind(DOCUMENT)
            .execute(&mut store.connection)
            .await
            .unwrap();
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &node)
                .await,
            Err(StoreError::UnsupportedFormat)
        );
        store
            .save_registered_checkpoint(&binding, DOCUMENT, &session, 1, b"local edit\n")
            .await
            .unwrap();
        let draft = store
            .read_registered_checkpoint(&binding, DOCUMENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.base_bytes, b"old source\n");
        assert_eq!(draft.base_revision, "content-1");
    }

    #[tokio::test]
    async fn checkpoint_and_outbox_history_both_block_pristine_refresh() {
        let (_directory, mut store, binding, node, session) = fixture().await;
        assert!(
            store
                .cache_content(&binding, &node, b"remote source\n")
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .pristine_registered_session_revision(&binding, DOCUMENT, &session)
                .await
                .unwrap(),
            Some("content-1".into())
        );
        store
            .save_registered_checkpoint(&binding, DOCUMENT, &session, 1, b"local edit\n")
            .await
            .unwrap();
        let before = store
            .read_registered_checkpoint(&binding, DOCUMENT)
            .await
            .unwrap();
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &node)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap(),
            before
        );
        store.queue_checkpoint(&binding, DOCUMENT, 1).await.unwrap();
        let outbox = store.list_outbox(&binding, 0, 100).await.unwrap();
        assert_eq!(
            store
                .pristine_registered_session_revision(&binding, DOCUMENT, &session)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &node)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap(),
            before
        );
        assert_eq!(store.list_outbox(&binding, 0, 100).await.unwrap(), outbox);
    }

    #[tokio::test]
    async fn queued_restore_without_checkpoint_blocks_pristine_refresh() {
        let (_directory, mut store, binding, node, session) = fixture().await;
        assert!(
            store
                .cache_content(&binding, &node, b"remote source\n")
                .await
                .unwrap()
        );
        let request: notryx_protocol::RestoreNodeRequest = serde_json::from_value(json!({
            "expected_meta_rev": "meta-1", "parent_id": ROOT, "name": "document.md"
        }))
        .unwrap();
        let operation = store
            .queue_restore(&binding, node.node_id, &request)
            .await
            .unwrap();
        assert!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .pristine_registered_session_revision(&binding, DOCUMENT, &session)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .refresh_clean_registered_session(&binding, DOCUMENT, &session, "content-1", &node)
                .await
                .unwrap(),
            None
        );
        let outbox = store
            .list_restore_operations(&binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(outbox.entries.len(), 1);
        assert_eq!(outbox.entries[0].operation, operation);
        assert!(outbox.entries[0].receipt.is_none());
        assert!(
            store
                .read_registered_checkpoint(&binding, DOCUMENT)
                .await
                .unwrap()
                .is_none()
        );
        let registration = store.registration_id(&binding, true).await.unwrap();
        let storage_id = store
            .registered_storage_id(registration, DOCUMENT)
            .await
            .unwrap()
            .unwrap();
        let session = store.sessions.get(&storage_id).unwrap();
        assert_eq!(session.base_revision, "content-1");
        assert_eq!(session.base, b"old source\n");
    }
}
