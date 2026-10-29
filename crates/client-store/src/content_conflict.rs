use crate::{ClientStore, RegistrationBinding, StoreError, storage_error};
use notryx_domain::{ContentRevision, NodeId, OperationId};
use notryx_protocol::{ContentPrecondition, StrongEtag};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};

pub(crate) const UPGRADE_V16: &str = "
    CREATE TABLE content_conflict_intents (
        operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        document_id TEXT NOT NULL,
        generation INTEGER NOT NULL CHECK(generation > 0),
        base_revision TEXT NOT NULL,
        base_bytes BLOB NOT NULL CHECK(length(base_bytes) <= 5242880),
        local_bytes BLOB NOT NULL CHECK(length(local_bytes) <= 5242880),
        remote_revision TEXT NOT NULL,
        remote_hash TEXT NOT NULL,
        remote_bytes BLOB NOT NULL CHECK(length(remote_bytes) <= 5242880),
        choice TEXT NOT NULL CHECK(choice IN ('keep_local_copy', 'choose_remote', 'save_merged')),
        merged_bytes BLOB CHECK(merged_bytes IS NULL OR length(merged_bytes) <= 5242880),
        CHECK((choice = 'save_merged' AND merged_bytes IS NOT NULL) OR (choice <> 'save_merged' AND merged_bytes IS NULL))
    );
    PRAGMA user_version = 16;
";

pub(crate) const UPGRADE_V17: &str = "
    ALTER TABLE content_conflict_intents ADD COLUMN completed_generation INTEGER
        CHECK(completed_generation IS NULL OR (choice = 'choose_remote' AND completed_generation > generation));
    ALTER TABLE content_conflict_intents ADD COLUMN completed_through INTEGER
        CHECK((completed_generation IS NULL AND completed_through IS NULL) OR
              (completed_generation IS NOT NULL AND completed_through IS NOT NULL AND completed_through > 0));
    PRAGMA user_version = 17;
";

pub(crate) const UPGRADE_V18: &str = "
    ALTER TABLE content_conflict_intents ADD COLUMN intent_version INTEGER NOT NULL DEFAULT 0 CHECK(intent_version >= 0);
    CREATE TABLE content_conflict_intent_history (
        operation_id TEXT NOT NULL REFERENCES client_outbox(operation_id),
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        history_seq INTEGER NOT NULL CHECK(history_seq > 0),
        document_id TEXT NOT NULL,
        generation INTEGER NOT NULL CHECK(generation > 0),
        base_revision TEXT NOT NULL,
        base_bytes BLOB NOT NULL CHECK(length(base_bytes) <= 5242880),
        local_bytes BLOB NOT NULL CHECK(length(local_bytes) <= 5242880),
        remote_revision TEXT NOT NULL,
        remote_hash TEXT NOT NULL,
        remote_bytes BLOB NOT NULL CHECK(length(remote_bytes) <= 5242880),
        choice TEXT NOT NULL CHECK(choice IN ('keep_local_copy', 'choose_remote', 'save_merged')),
        merged_bytes BLOB CHECK(merged_bytes IS NULL OR length(merged_bytes) <= 5242880),
        CHECK((choice = 'save_merged' AND merged_bytes IS NOT NULL) OR (choice <> 'save_merged' AND merged_bytes IS NULL)),
        PRIMARY KEY(operation_id, history_seq)
    );
    PRAGMA user_version = 18;
";

pub(crate) const UPGRADE_V19: &str = "
    CREATE TABLE content_conflict_copies (
        operation_id TEXT PRIMARY KEY NOT NULL REFERENCES content_conflict_intents(operation_id),
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        intent_version INTEGER NOT NULL CHECK(intent_version >= 0),
        parent_id TEXT NOT NULL,
        name TEXT NOT NULL,
        node_id TEXT NOT NULL UNIQUE,
        create_operation_id TEXT NOT NULL UNIQUE,
        content_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id)
    );
    CREATE TABLE content_conflict_copy_attempts (
        old_create_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        operation_id TEXT NOT NULL REFERENCES content_conflict_intents(operation_id),
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        intent_version INTEGER NOT NULL CHECK(intent_version >= 0),
        old_parent_id TEXT NOT NULL, old_name TEXT NOT NULL, old_node_id TEXT NOT NULL,
        new_parent_id TEXT NOT NULL, new_name TEXT NOT NULL, new_node_id TEXT NOT NULL,
        new_create_operation_id TEXT NOT NULL UNIQUE
    );
    PRAGMA user_version = 19;
";

pub(crate) const UPGRADE_V20_ATTEMPTS: &str = "
    CREATE TABLE IF NOT EXISTS content_conflict_copy_attempts (
        old_create_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        operation_id TEXT NOT NULL REFERENCES content_conflict_intents(operation_id),
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        intent_version INTEGER NOT NULL CHECK(intent_version >= 0),
        old_parent_id TEXT NOT NULL, old_name TEXT NOT NULL, old_node_id TEXT NOT NULL,
        new_parent_id TEXT NOT NULL, new_name TEXT NOT NULL, new_node_id TEXT NOT NULL,
        new_create_operation_id TEXT NOT NULL UNIQUE
    );
";

#[derive(Clone, PartialEq, Eq)]
pub struct ContentConflictChooseRemoteResult {
    pub binding: RegistrationBinding,
    pub document_id: String,
    pub operation_id: String,
    pub generation: u64,
    pub remote_revision: String,
    pub remote_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentConflictResolutionState {
    NotFound,
    Pending {
        version: u64,
        generation: u64,
        choice: ContentConflictChoiceKind,
        needs_review: bool,
        frozen: bool,
    },
    Completed {
        generation: u64,
        frozen: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentConflictChoiceKind {
    KeepLocalCopy,
    ChooseRemote,
    SaveMerged,
}

impl std::fmt::Debug for ContentConflictChooseRemoteResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentConflictChooseRemoteResult")
            .field("binding", &self.binding)
            .field("document_id", &self.document_id)
            .field("operation_id", &self.operation_id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentConflictRef {
    pub operation_id: String,
    pub operation_seq: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ContentConflictCopyIntent {
    pub binding: RegistrationBinding,
    pub document_id: String,
    pub conflict_operation_id: String,
    pub intent_version: u64,
    pub generation: u64,
    pub parent_id: String,
    pub name: String,
    pub local_bytes: Vec<u8>,
    pub node_id: String,
    pub create_operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentConflictCopyContentState {
    NotQueued,
    Pending { operation_id: String },
    Saved { operation_id: String },
    NeedsReview { operation_id: String },
}

impl std::fmt::Debug for ContentConflictCopyIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentConflictCopyIntent")
            .field("binding", &self.binding)
            .field("document_id", &self.document_id)
            .field("conflict_operation_id", &self.conflict_operation_id)
            .field("intent_version", &self.intent_version)
            .field("generation", &self.generation)
            .field("parent_id", &self.parent_id)
            .field("name", &self.name)
            .field("node_id", &self.node_id)
            .field("create_operation_id", &self.create_operation_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ContentConflictChoice {
    KeepLocalCopy,
    ChooseRemote,
    SaveMerged { bytes: Vec<u8> },
}

impl std::fmt::Debug for ContentConflictChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeepLocalCopy => f.write_str("KeepLocalCopy"),
            Self::ChooseRemote => f.write_str("ChooseRemote"),
            Self::SaveMerged { .. } => f.write_str("SaveMerged { .. }"),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ContentConflictIntentRequest {
    pub generation: u64,
    pub remote_revision: String,
    pub remote_hash: String,
    pub remote_bytes: Vec<u8>,
    pub choice: ContentConflictChoice,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ContentConflictIntent {
    pub binding: RegistrationBinding,
    pub document_id: String,
    pub operation_id: String,
    pub version: u64,
    pub generation: u64,
    pub base_revision: String,
    pub base_bytes: Vec<u8>,
    pub local_bytes: Vec<u8>,
    pub remote_revision: String,
    pub remote_hash: String,
    pub remote_bytes: Vec<u8>,
    pub choice: ContentConflictChoice,
    pub merged_hash: Option<String>,
}

impl std::fmt::Debug for ContentConflictIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentConflictIntent")
            .field("binding", &self.binding)
            .field("document_id", &self.document_id)
            .field("operation_id", &self.operation_id)
            .field("version", &self.version)
            .field("generation", &self.generation)
            .field("choice", &self.choice)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ContentConflictSnapshot {
    pub binding: RegistrationBinding,
    pub document_id: String,
    pub operation_id: String,
    pub operation_seq: u64,
    pub expected_revision: String,
    pub conflict_revision: String,
    pub conflict_etag: String,
    pub payload: Vec<u8>,
    pub generation: u64,
    pub local: Vec<u8>,
    pub base: Vec<u8>,
    pub base_revision: String,
}

impl std::fmt::Debug for ContentConflictSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentConflictSnapshot")
            .field("binding", &self.binding)
            .field("document_id", &self.document_id)
            .field("operation_id", &self.operation_id)
            .field("operation_seq", &self.operation_seq)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

async fn record_intent_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    request: &ContentConflictIntentRequest,
    node: NodeId,
) -> Result<ContentConflictIntent, StoreError> {
    let active: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id=? AND status='active')",
    )
    .bind(registration)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if active != 1 {
        return Err(StoreError::StaleRegistration);
    }
    if let Some(existing) =
        read_intent(&mut *connection, registration, binding, operation_id).await?
    {
        verify_history_extent(
            &mut *connection,
            registration,
            operation_id,
            existing.version,
        )
        .await?;
        if existing.version == 0
            && existing.document_id == document_id
            && existing.generation == request.generation
            && existing.remote_revision == request.remote_revision
            && existing.remote_hash == request.remote_hash
            && existing.remote_bytes == request.remote_bytes
            && existing.choice == request.choice
        {
            return Ok(existing);
        }
        return Err(StoreError::InvalidRequest);
    }
    verify_history_extent(&mut *connection, registration, operation_id, 0).await?;
    let candidates: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT o.operation_id,i.operation_id FROM client_outbox o LEFT JOIN content_conflict_intents i ON i.registration_id=o.registration_id AND i.operation_id=o.operation_id WHERE o.registration_id=? AND o.document_id=? AND o.operation_kind='content' AND ((o.status='conflict' AND o.terminal_kind IS NULL) OR o.conflict_revision IS NOT NULL OR o.conflict_etag IS NOT NULL) ORDER BY o.operation_seq"
        ).bind(registration).bind(document_id).fetch_all(&mut *connection).await.map_err(storage_error)?;
    let mut first = None;
    for (candidate, existing_intent) in candidates {
        if existing_intent.is_some()
            && verified_completed_merge(&mut *connection, registration, binding, &candidate)
                .await?
                .is_some()
        {
            continue;
        }
        if existing_intent.is_some()
            && verified_completed_choose_remote(&mut *connection, registration, binding, &candidate)
                .await?
                .is_some()
        {
            continue;
        }
        if existing_intent.is_some() {
            let previous = read_intent(&mut *connection, registration, binding, &candidate)
                .await?
                .ok_or(StoreError::UnsupportedFormat)?;
            verify_history_extent(&mut *connection, registration, &candidate, previous.version)
                .await?;
            if merged_operation_on_connection(&mut *connection, registration, binding, &previous)
                .await?
                .is_some_and(|merged| {
                    merged.status == crate::OutboxStatus::Conflict && merged.failure_kind.is_none()
                })
            {
                continue;
            }
        }
        first = Some(candidate);
        break;
    }
    if first.as_deref() != Some(operation_id) {
        return Err(StoreError::InvalidRequest);
    }
    let snapshot = inspect_content_conflict_on_connection(
        &mut *connection,
        registration,
        binding,
        document_id,
        operation_id,
    )
    .await?;
    if snapshot.generation != request.generation {
        return Err(StoreError::InvalidRequest);
    }
    let remote = crate::content_cache::observe_remote_file_on_connection(
        &mut *connection,
        registration,
        node,
    )
    .await?;
    let crate::RemoteFileObservation::Live(remote) = remote else {
        return Err(StoreError::InvalidRequest);
    };
    let (revision, hash, size) = crate::content_cache::validated_content(&remote)
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if revision != request.remote_revision
        || hash != request.remote_hash
        || size != request.remote_bytes.len() as u64
        || digest(&request.remote_bytes) != request.remote_hash
    {
        return Err(StoreError::InvalidRequest);
    }
    let (choice, merged) = match &request.choice {
        ContentConflictChoice::KeepLocalCopy => ("keep_local_copy", None),
        ContentConflictChoice::ChooseRemote => ("choose_remote", None),
        ContentConflictChoice::SaveMerged { bytes } => ("save_merged", Some(bytes.as_slice())),
    };
    let merged_hash = merged.map(digest);
    sqlx::query("INSERT INTO content_conflict_intents(operation_id,registration_id,document_id,generation,base_revision,base_bytes,local_bytes,remote_revision,remote_hash,remote_bytes,choice,merged_bytes,merged_hash) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(operation_id).bind(registration).bind(document_id).bind(snapshot.generation as i64)
            .bind(&snapshot.base_revision).bind(&snapshot.base).bind(&snapshot.local)
            .bind(&request.remote_revision).bind(&request.remote_hash).bind(&request.remote_bytes)
            .bind(choice).bind(merged).bind(&merged_hash).execute(&mut *connection).await.map_err(storage_error)?;
    let intent = ContentConflictIntent {
        binding: binding.clone(),
        document_id: document_id.to_owned(),
        operation_id: operation_id.to_owned(),
        version: 0,
        generation: snapshot.generation,
        base_revision: snapshot.base_revision,
        base_bytes: snapshot.base,
        local_bytes: snapshot.local,
        remote_revision: request.remote_revision.clone(),
        remote_hash: request.remote_hash.clone(),
        remote_bytes: request.remote_bytes.clone(),
        choice: request.choice.clone(),
        merged_hash,
    };
    Ok(intent)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn replace_intent_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    expected: &ContentConflictIntent,
    request: &ContentConflictIntentRequest,
    node: NodeId,
) -> Result<ContentConflictIntent, StoreError> {
    let active: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id=? AND status='active')",
    )
    .bind(registration)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if active != 1 {
        return Err(StoreError::StaleRegistration);
    }
    let current = read_intent(&mut *connection, registration, binding, operation_id)
        .await?
        .ok_or(StoreError::InvalidRequest)?;
    if current.document_id != document_id
        || expected.binding != *binding
        || expected.document_id != document_id
        || expected.operation_id != operation_id
    {
        return Err(StoreError::InvalidRequest);
    }
    let (completed, through): (Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT completed_generation,completed_through FROM content_conflict_intents WHERE registration_id=? AND operation_id=?")
            .bind(registration).bind(operation_id).fetch_one(&mut *connection).await.map_err(storage_error)?;
    if completed.is_some() || through.is_some() {
        return Err(StoreError::InvalidRequest);
    }
    verify_history_extent(
        &mut *connection,
        registration,
        operation_id,
        current.version,
    )
    .await?;
    let matches_new = current.generation == request.generation
        && current.remote_revision == request.remote_revision
        && current.remote_hash == request.remote_hash
        && current.remote_bytes == request.remote_bytes
        && current.choice == request.choice;
    if current != *expected {
        let last = if current.version == 0 {
            None
        } else {
            read_history_entry(
                &mut *connection,
                registration,
                binding,
                operation_id,
                current.version,
            )
            .await?
        };
        if matches_new && last.as_ref() == Some(expected) {
            return Ok(current);
        }
        return Err(StoreError::InvalidRequest);
    }
    if matches_new
        && !(matches!(&current.choice, ContentConflictChoice::SaveMerged { .. })
            && current.merged_hash.is_none())
    {
        return Ok(current);
    }
    let merged_exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM content_conflict_merged_operations WHERE registration_id=? AND operation_id=?)",
    )
    .bind(registration)
    .bind(operation_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if merged_exists != 0 {
        return Err(StoreError::OutboxBlocked);
    }
    let copy_exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM content_conflict_copies WHERE operation_id=?)",
    )
    .bind(operation_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if copy_exists != 0 {
        let copy = read_copy_on_connection(connection, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::UnsupportedFormat)?;
        if !matches!(
            copy_content_state_on_connection(
                connection,
                registration,
                binding,
                operation_id,
                copy.intent_version
            )
            .await?,
            ContentConflictCopyContentState::Saved { .. }
        ) {
            return Err(StoreError::OutboxBlocked);
        }
    }
    let snapshot = inspect_content_conflict_on_connection(
        &mut *connection,
        registration,
        binding,
        document_id,
        operation_id,
    )
    .await?;
    if snapshot.generation != request.generation || snapshot.generation < current.generation {
        return Err(StoreError::InvalidRequest);
    }
    let remote = crate::content_cache::observe_remote_file_on_connection(
        &mut *connection,
        registration,
        node,
    )
    .await?;
    let crate::RemoteFileObservation::Live(remote) = remote else {
        return Err(StoreError::InvalidRequest);
    };
    let (revision, hash, size) = crate::content_cache::validated_content(&remote)
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if revision != request.remote_revision
        || hash != request.remote_hash
        || size != request.remote_bytes.len() as u64
        || digest(&request.remote_bytes) != request.remote_hash
    {
        return Err(StoreError::InvalidRequest);
    }
    let sequence = i64::try_from(current.version)
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or(StoreError::UnsupportedFormat)?;
    let inserted = sqlx::query("INSERT INTO content_conflict_intent_history(operation_id,registration_id,history_seq,document_id,generation,base_revision,base_bytes,local_bytes,remote_revision,remote_hash,remote_bytes,choice,merged_bytes,merged_hash) SELECT operation_id,registration_id,?,document_id,generation,base_revision,base_bytes,local_bytes,remote_revision,remote_hash,remote_bytes,choice,merged_bytes,merged_hash FROM content_conflict_intents WHERE registration_id=? AND operation_id=? AND completed_generation IS NULL AND completed_through IS NULL")
            .bind(sequence).bind(registration).bind(operation_id).execute(&mut *connection).await.map_err(storage_error)?.rows_affected();
    if inserted != 1 {
        return Err(StoreError::InvalidRequest);
    }
    let (choice, merged) = match &request.choice {
        ContentConflictChoice::KeepLocalCopy => ("keep_local_copy", None),
        ContentConflictChoice::ChooseRemote => ("choose_remote", None),
        ContentConflictChoice::SaveMerged { bytes } => ("save_merged", Some(bytes.as_slice())),
    };
    let merged_hash = merged.map(digest);
    let updated = sqlx::query("UPDATE content_conflict_intents SET generation=?,base_revision=?,base_bytes=?,local_bytes=?,remote_revision=?,remote_hash=?,remote_bytes=?,choice=?,merged_bytes=?,merged_hash=?,intent_version=? WHERE registration_id=? AND operation_id=? AND completed_generation IS NULL AND completed_through IS NULL AND intent_version=?")
            .bind(snapshot.generation as i64).bind(&snapshot.base_revision).bind(&snapshot.base).bind(&snapshot.local)
            .bind(&request.remote_revision).bind(&request.remote_hash).bind(&request.remote_bytes).bind(choice).bind(merged).bind(&merged_hash).bind(sequence)
            .bind(registration).bind(operation_id).bind(current.version as i64).execute(&mut *connection).await.map_err(storage_error)?.rows_affected();
    if updated != 1 {
        return Err(StoreError::InvalidRequest);
    }
    let result = ContentConflictIntent {
        binding: binding.clone(),
        document_id: document_id.to_owned(),
        operation_id: operation_id.to_owned(),
        version: sequence as u64,
        generation: snapshot.generation,
        base_revision: snapshot.base_revision,
        base_bytes: snapshot.base,
        local_bytes: snapshot.local,
        remote_revision: request.remote_revision.clone(),
        remote_hash: request.remote_hash.clone(),
        remote_bytes: request.remote_bytes.clone(),
        choice: request.choice.clone(),
        merged_hash,
    };
    Ok(result)
}

async fn content_followup_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
    approved: Option<&str>,
    mark: bool,
) -> Result<Option<crate::OutboxOperation>, StoreError> {
    let active: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id=? AND status='active')",
    )
    .bind(registration)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if active != 1 {
        return Err(StoreError::StaleRegistration);
    }
    let intent = read_intent(&mut *connection, registration, binding, operation_id)
        .await?
        .ok_or(StoreError::InvalidRequest)?;
    verify_history_extent(&mut *connection, registration, operation_id, intent.version).await?;
    if mark
        && matches!(&intent.choice, ContentConflictChoice::SaveMerged { .. })
        && intent.merged_hash.is_none()
    {
        return Err(StoreError::OutboxBlocked);
    }
    let snapshot = inspect_content_conflict_on_connection(
        &mut *connection,
        registration,
        binding,
        &intent.document_id,
        operation_id,
    )
    .await?;
    if snapshot.generation != intent.generation
        || snapshot.local != intent.local_bytes
        || snapshot.base != intent.base_bytes
        || snapshot.base_revision != intent.base_revision
    {
        return Err(StoreError::InvalidRequest);
    }
    let node = intent
        .document_id
        .parse::<NodeId>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let remote = crate::content_cache::observe_remote_file_on_connection(
        &mut *connection,
        registration,
        node,
    )
    .await?;
    let crate::RemoteFileObservation::Live(remote) = remote else {
        return Err(StoreError::InvalidRequest);
    };
    let (revision, hash, size) = crate::content_cache::validated_content(&remote)
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if revision != intent.remote_revision
        || hash != intent.remote_hash
        || size != intent.remote_bytes.len() as u64
    {
        return Err(StoreError::InvalidRequest);
    }
    let (ack, next): (i64, i64) =
        sqlx::query_as("SELECT ack_through,next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration)
            .fetch_one(&mut *connection)
            .await
            .map_err(storage_error)?;
    let mut previous_id = operation_id.to_owned();
    let mut after =
        i64::try_from(snapshot.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
    loop {
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' AND operation_seq>? ORDER BY operation_seq LIMIT 1")
                .bind(registration).bind(&intent.document_id).bind(after)
                .fetch_optional(&mut *connection).await.map_err(storage_error)?;
        let Some(row) = row else {
            return if approved.is_some() {
                Err(StoreError::InvalidRequest)
            } else {
                Ok(None)
            };
        };
        let item = crate::outbox::operation(&row, binding, false)?;
        if item.generation > intent.generation
            || (item.generation == intent.generation && item.bytes != intent.local_bytes)
        {
            return Err(StoreError::UnsupportedFormat);
        }
        let seq = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
        if seq <= after || seq >= next || dependency.as_deref() != Some(previous_id.as_str()) {
            return Err(StoreError::UnsupportedFormat);
        }
        if item.status == crate::OutboxStatus::Abandoned {
            if seq > ack {
                return Err(StoreError::UnsupportedFormat);
            }
            after = seq;
            previous_id = item.operation_id;
            continue;
        }
        if seq <= ack
            || !matches!(
                item.status,
                crate::OutboxStatus::Queued | crate::OutboxStatus::Abandoning
            )
        {
            return Err(StoreError::OutboxBlocked);
        }
        if !mark {
            return Ok(Some(item));
        }
        if approved.is_some_and(|approved| approved != item.operation_id) {
            return Err(StoreError::InvalidRequest);
        }
        if item.abandon_state == crate::OutboxAbandonState::None {
            let changed = sqlx::query("UPDATE client_outbox SET abandon_state='requested' WHERE registration_id=? AND operation_id=? AND status='queued' AND abandon_state='none'")
                    .bind(registration).bind(&item.operation_id).execute(&mut *connection).await.map_err(storage_error)?.rows_affected();
            if changed != 1 {
                return Err(StoreError::OutboxBlocked);
            }
        }
        let row =
            sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration)
                .bind(&item.operation_id)
                .fetch_one(&mut *connection)
                .await
                .map_err(storage_error)?;
        let requested = crate::outbox::operation(&row, binding, false)?;
        return Ok(Some(requested));
    }
}

async fn read_copy_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<Option<ContentConflictCopyIntent>, StoreError> {
    let row = sqlx::query("SELECT registration_id,intent_version,parent_id,name,node_id,create_operation_id FROM content_conflict_copies WHERE operation_id=?")
        .bind(operation_id).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some(row) = row else { return Ok(None) };
    let row_registration: i64 = row.try_get("registration_id").map_err(storage_error)?;
    if row_registration != registration {
        return Err(StoreError::StaleRegistration);
    }
    let current = read_intent(&mut *connection, registration, binding, operation_id)
        .await?
        .ok_or(StoreError::UnsupportedFormat)?;
    verify_history_extent(
        &mut *connection,
        registration,
        operation_id,
        current.version,
    )
    .await?;
    let version: i64 = row.try_get("intent_version").map_err(storage_error)?;
    let version = u64::try_from(version).map_err(|_| StoreError::UnsupportedFormat)?;
    let intent = if version == current.version {
        current
    } else if version < current.version {
        read_history_entry(
            &mut *connection,
            registration,
            binding,
            operation_id,
            version
                .checked_add(1)
                .ok_or(StoreError::UnsupportedFormat)?,
        )
        .await?
        .filter(|intent| intent.version == version && intent.document_id == current.document_id)
        .ok_or(StoreError::UnsupportedFormat)?
    } else {
        return Err(StoreError::UnsupportedFormat);
    };
    let parent_id: String = row.try_get("parent_id").map_err(storage_error)?;
    let name: String = row.try_get("name").map_err(storage_error)?;
    let node_id: String = row.try_get("node_id").map_err(storage_error)?;
    let create_operation_id: String = row.try_get("create_operation_id").map_err(storage_error)?;
    let new_node = uuid::Uuid::parse_str(&node_id).map_err(|_| StoreError::UnsupportedFormat)?;
    let new_operation =
        uuid::Uuid::parse_str(&create_operation_id).map_err(|_| StoreError::UnsupportedFormat)?;
    if intent.choice != ContentConflictChoice::KeepLocalCopy
        || version != intent.version
        || parent_id.parse::<NodeId>().is_err()
        || notryx_domain::mutation::validate_name(&name).is_err()
        || !name.to_ascii_lowercase().ends_with(".md")
        || new_node.get_version_num() != 7
        || new_operation.get_version_num() != 7
        || node_id == intent.document_id
        || node_id == create_operation_id
        || create_operation_id == operation_id
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(Some(ContentConflictCopyIntent {
        binding: binding.clone(),
        document_id: intent.document_id,
        conflict_operation_id: operation_id.to_owned(),
        intent_version: intent.version,
        generation: intent.generation,
        parent_id,
        name,
        local_bytes: intent.local_bytes,
        node_id,
        create_operation_id,
    }))
}

async fn copy_content_state_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
    expected_version: u64,
) -> Result<ContentConflictCopyContentState, StoreError> {
    let copy = read_copy_on_connection(&mut *connection, registration, binding, operation_id)
        .await?
        .ok_or(StoreError::InvalidRequest)?;
    if copy.intent_version != expected_version {
        return Err(StoreError::InvalidRequest);
    }
    let linked: Option<String> = sqlx::query_scalar("SELECT content_operation_id FROM content_conflict_copies WHERE operation_id=? AND registration_id=?")
            .bind(operation_id).bind(registration).fetch_one(&mut *connection).await.map_err(storage_error)?;
    copy_content_state(
        &mut *connection,
        registration,
        binding,
        &CopyDetails::from(&copy),
        linked,
    )
    .await
}

pub(crate) struct CopyDetails<'a> {
    pub node_id: &'a str,
    pub parent_id: &'a str,
    pub name: &'a str,
    pub local_bytes: &'a [u8],
    pub create_operation_id: &'a str,
}

impl<'a> From<&'a ContentConflictCopyIntent> for CopyDetails<'a> {
    fn from(copy: &'a ContentConflictCopyIntent) -> Self {
        Self {
            node_id: &copy.node_id,
            parent_id: &copy.parent_id,
            name: &copy.name,
            local_bytes: &copy.local_bytes,
            create_operation_id: &copy.create_operation_id,
        }
    }
}

pub(crate) async fn copy_content_state(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    copy: &CopyDetails<'_>,
    linked: Option<String>,
) -> Result<ContentConflictCopyContentState, StoreError> {
    let Some(id) = linked else {
        return Ok(ContentConflictCopyContentState::NotQueued);
    };
    let create_row = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(copy.create_operation_id).fetch_optional(&mut *connection).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
    let create_registration: i64 = create_row
        .try_get("registration_id")
        .map_err(storage_error)?;
    let kind: String = create_row
        .try_get("operation_kind")
        .map_err(storage_error)?;
    if create_registration != registration || kind != "create" {
        return Err(StoreError::UnsupportedFormat);
    }
    let create = crate::outbox::validated_create_record(&create_row, binding)?;
    if create.status != crate::OutboxStatus::Committed
        || create.request.node_id.to_string() != copy.node_id
        || create.request.parent_id.to_string() != copy.parent_id
        || create.request.name != copy.name
        || create.request.kind != notryx_protocol::NodeKind::File
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let raw: String = create_row.try_get("receipt_json").map_err(storage_error)?;
    let receipt: notryx_protocol::OperationReceipt =
        serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
    let notryx_protocol::OperationReceipt::Created { node, .. } = receipt else {
        return Err(StoreError::UnsupportedFormat);
    };
    let revision = node
        .content_rev
        .ok_or(StoreError::UnsupportedFormat)?
        .as_str()
        .to_owned();
    let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
        .bind(registration)
        .bind(&id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::UnsupportedFormat)?;
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    let item = if retired == 0 {
        crate::outbox::operation(&row, binding, false)?
    } else if retired == 1 {
        let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
        let status: String = row.try_get("status").map_err(storage_error)?;
        if !bytes.is_empty() || status != "committed" {
            return Err(StoreError::UnsupportedFormat);
        }
        let reconstructed = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,generation,expected_revision,conflict_revision,? AS bytes,payload_hash,payload_len,status,abandon_state,terminal_kind,0 AS payload_retired FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(copy.local_bytes).bind(registration).bind(&id).fetch_one(&mut *connection).await.map_err(storage_error)?;
        crate::outbox::operation(&reconstructed, binding, false)?
    } else {
        return Err(StoreError::UnsupportedFormat);
    };
    if item.document_id != copy.node_id
        || item.generation != 1
        || item.bytes.as_slice() != copy.local_bytes
        || item.expected_revision.as_deref() != Some(revision.as_str())
        || item.operation_seq <= create.operation_seq
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as(
        "SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?",
    )
    .bind(registration)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    let seq = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
    if confirmed < 0
        || confirmed > ack
        || ack >= next
        || seq >= next
        || confirmed
            < i64::try_from(create.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?
    {
        return Err(StoreError::UnsupportedFormat);
    }
    match item.status {
        crate::OutboxStatus::Queued | crate::OutboxStatus::Sent => {
            if ack >= seq {
                return Err(StoreError::UnsupportedFormat);
            }
            Ok(ContentConflictCopyContentState::Pending { operation_id: id })
        }
        crate::OutboxStatus::Committed => {
            if ack < seq {
                return Err(StoreError::UnsupportedFormat);
            }
            let committed: Option<String> =
                row.try_get("committed_revision").map_err(storage_error)?;
            let committed = committed.ok_or(StoreError::UnsupportedFormat)?;
            let etag = format!("\"{committed}\"");
            ContentPrecondition::parse(&etag)
                .and_then(|value| value.revision())
                .map_err(|_| StoreError::UnsupportedFormat)?;
            let storage: String = row.try_get("storage_id").map_err(storage_error)?;
            let actual: Option<String> = sqlx::query_scalar("SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?")
                    .bind(registration).bind(copy.node_id).fetch_optional(&mut *connection).await.map_err(storage_error)?;
            if actual.as_deref() != Some(storage.as_str()) {
                return Err(StoreError::UnsupportedFormat);
            }
            verify_durable_base_chain(
                &mut *connection,
                registration,
                binding,
                copy.node_id,
                1,
                seq,
                &committed,
                &digest(copy.local_bytes),
                copy.local_bytes.len() as i64,
                ack,
            )
            .await?;
            if confirmed < seq {
                Ok(ContentConflictCopyContentState::Pending { operation_id: id })
            } else {
                Ok(ContentConflictCopyContentState::Saved { operation_id: id })
            }
        }
        crate::OutboxStatus::Conflict
        | crate::OutboxStatus::Failed
        | crate::OutboxStatus::Abandoned => {
            if ack < seq {
                return Err(StoreError::UnsupportedFormat);
            }
            Ok(ContentConflictCopyContentState::NeedsReview { operation_id: id })
        }
        crate::OutboxStatus::Abandoning => {
            if ack >= seq {
                return Err(StoreError::UnsupportedFormat);
            }
            Ok(ContentConflictCopyContentState::NeedsReview { operation_id: id })
        }
    }
}

pub(crate) async fn queue_copy_create(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    copy: &CopyDetails<'_>,
) -> Result<crate::CreateOutboxOperation, StoreError> {
    let request = notryx_protocol::CreateNodeRequest {
        node_id: copy
            .node_id
            .parse()
            .map_err(|_| StoreError::UnsupportedFormat)?,
        parent_id: copy
            .parent_id
            .parse()
            .map_err(|_| StoreError::UnsupportedFormat)?,
        name: copy.name.to_owned(),
        kind: notryx_protocol::NodeKind::File,
        upload_id: None,
    };
    let existing = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(copy.create_operation_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?;
    let operation = if let Some(row) = existing {
        let row_registration: i64 = row.try_get("registration_id").map_err(storage_error)?;
        let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
        if row_registration != registration || kind != "create" {
            return Err(StoreError::UnsupportedFormat);
        }
        let operation = crate::outbox::validated_create_record(&row, binding)?;
        if operation.request != request {
            return Err(StoreError::UnsupportedFormat);
        }
        if matches!(
            operation.status,
            crate::OutboxStatus::Committed
                | crate::OutboxStatus::Failed
                | crate::OutboxStatus::Abandoned
        ) {
            let ack: i64 = sqlx::query_scalar(
                "SELECT ack_through FROM outbox_streams WHERE registration_id=?",
            )
            .bind(registration)
            .fetch_one(&mut *connection)
            .await
            .map_err(storage_error)?;
            if u64::try_from(ack)
                .ok()
                .is_none_or(|ack| ack < operation.operation_seq)
            {
                return Err(StoreError::UnsupportedFormat);
            }
        }
        operation
    } else {
        crate::outbox::queue_create_on_connection(
            &mut *connection,
            registration,
            binding,
            &request,
            copy.create_operation_id,
        )
        .await?
    };
    Ok(operation)
}

async fn verified_merge_predecessors(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    intent: &ContentConflictIntent,
    before: i64,
) -> Result<i64, StoreError> {
    let original: i64 = sqlx::query_scalar(
        "SELECT operation_seq FROM client_outbox WHERE registration_id=? AND operation_id=?",
    )
    .bind(registration)
    .bind(&intent.operation_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as(
        "SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?",
    )
    .bind(registration)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    if original <= 0
        || before <= original
        || before > next
        || confirmed < original
        || confirmed > ack
        || ack >= next
    {
        return Err(StoreError::OutboxBlocked);
    }
    let rows = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' AND operation_seq>? AND operation_seq<? ORDER BY operation_seq")
        .bind(registration).bind(&intent.document_id).bind(original).bind(before)
        .fetch_all(&mut *connection).await.map_err(storage_error)?;
    let mut through = original;
    let mut previous = intent.operation_id.clone();
    for row in rows {
        let item = crate::outbox::operation(&row, binding, false)?;
        let seq = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
        if seq <= through
            || dependency.as_deref() != Some(previous.as_str())
            || item.generation > intent.generation
            || (item.generation == intent.generation && item.bytes != intent.local_bytes)
        {
            return Err(StoreError::UnsupportedFormat);
        }
        if item.status != crate::OutboxStatus::Abandoned || seq > confirmed {
            return Err(StoreError::OutboxBlocked);
        }
        through = seq;
        previous = item.operation_id;
    }
    Ok(through)
}

async fn merged_operation_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    intent: &ContentConflictIntent,
) -> Result<Option<crate::OutboxOperation>, StoreError> {
    let link: Option<(i64, String, i64)> = sqlx::query_as("SELECT intent_version,content_operation_id,abandoned_through FROM content_conflict_merged_operations WHERE registration_id=? AND operation_id=?")
        .bind(registration).bind(&intent.operation_id).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some((version, id, through)) = link else {
        return Ok(None);
    };
    if u64::try_from(version).ok() != Some(intent.version) {
        return Err(StoreError::UnsupportedFormat);
    }
    let ContentConflictChoice::SaveMerged { bytes } = &intent.choice else {
        return Err(StoreError::UnsupportedFormat);
    };
    if intent.merged_hash.as_deref() != Some(digest(bytes).as_str()) {
        return Err(StoreError::UnsupportedFormat);
    }
    let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
        .bind(registration)
        .bind(&id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::UnsupportedFormat)?;
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    let item = if retired == 0 {
        crate::outbox::operation(&row, binding, false)?
    } else if retired == 1 {
        let stored: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
        let status: String = row.try_get("status").map_err(storage_error)?;
        if !stored.is_empty() || status != "committed" {
            return Err(StoreError::UnsupportedFormat);
        }
        let reconstructed = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,generation,expected_revision,conflict_revision,? AS bytes,payload_hash,payload_len,status,abandon_state,terminal_kind,0 AS payload_retired FROM client_outbox WHERE registration_id=? AND operation_id=?")
            .bind(bytes).bind(registration).bind(&id).fetch_one(&mut *connection).await.map_err(storage_error)?;
        crate::outbox::operation(&reconstructed, binding, false)?
    } else {
        return Err(StoreError::UnsupportedFormat);
    };
    if item.status == crate::OutboxStatus::Conflict {
        let etag: Option<String> = row.try_get("conflict_etag").map_err(storage_error)?;
        let conflict =
            ContentPrecondition::parse(etag.as_deref().ok_or(StoreError::UnsupportedFormat)?)
                .and_then(|value| value.revision())
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if item.conflict_revision.as_deref() != Some(conflict.as_str())
            || item.expected_revision.as_deref() == Some(conflict.as_str())
        {
            return Err(StoreError::UnsupportedFormat);
        }
    }
    let seq = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
    if item.document_id != intent.document_id
        || item.generation
            != intent
                .generation
                .checked_add(1)
                .ok_or(StoreError::UnsupportedFormat)?
        || item.expected_revision.as_deref() != Some(intent.remote_revision.as_str())
        || item.bytes != *bytes
        || item.operation_id == intent.operation_id
        || verified_merge_predecessors(connection, registration, binding, intent, seq).await?
            != through
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let storage: String = row.try_get("storage_id").map_err(storage_error)?;
    let expected_storage: Option<String> = sqlx::query_scalar(
        "SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?",
    )
    .bind(registration)
    .bind(&intent.document_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(storage_error)?;
    if expected_storage.as_deref() != Some(storage.as_str()) {
        return Err(StoreError::UnsupportedFormat);
    }
    let checkpoint = sqlx::query("SELECT generation,format_version,CASE WHEN length(bytes)<=5242880 THEN bytes END AS bytes FROM draft_checkpoints WHERE document_id=?")
        .bind(&storage).fetch_optional(&mut *connection).await.map_err(storage_error)?
        .ok_or(StoreError::UnsupportedFormat)?;
    let generation: i64 = checkpoint.try_get("generation").map_err(storage_error)?;
    let format: i64 = checkpoint
        .try_get("format_version")
        .map_err(storage_error)?;
    let bytes: Option<Vec<u8>> = checkpoint.try_get("bytes").map_err(storage_error)?;
    let bytes = bytes.ok_or(StoreError::UnsupportedFormat)?;
    validate_text(&bytes).map_err(|_| StoreError::UnsupportedFormat)?;
    let merged_generation =
        i64::try_from(item.generation).map_err(|_| StoreError::UnsupportedFormat)?;
    if format != 1
        || generation < merged_generation
        || (generation == merged_generation && bytes != item.bytes)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(Some(item))
}

pub(crate) async fn verified_completed_merge(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<Option<(String, u64)>, StoreError> {
    let Some(intent) = read_intent(&mut *connection, registration, binding, operation_id).await?
    else {
        return Ok(None);
    };
    verify_history_extent(&mut *connection, registration, operation_id, intent.version).await?;
    let Some(item) =
        merged_operation_on_connection(&mut *connection, registration, binding, &intent).await?
    else {
        return Ok(None);
    };
    let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as(
        "SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?",
    )
    .bind(registration)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    let seq = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
    if confirmed < 0 || confirmed > ack || ack >= next || seq >= next {
        return Err(StoreError::UnsupportedFormat);
    }
    match item.status {
        crate::OutboxStatus::Queued
        | crate::OutboxStatus::Sent
        | crate::OutboxStatus::Abandoning => {
            if ack >= seq {
                return Err(StoreError::UnsupportedFormat);
            }
            Ok(None)
        }
        crate::OutboxStatus::Conflict
        | crate::OutboxStatus::Failed
        | crate::OutboxStatus::Abandoned => {
            if ack < seq {
                return Err(StoreError::UnsupportedFormat);
            }
            Ok(None)
        }
        crate::OutboxStatus::Committed => {
            if ack < seq {
                return Err(StoreError::UnsupportedFormat);
            }
            let revision: Option<String> = sqlx::query_scalar("SELECT committed_revision FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration).bind(&item.operation_id).fetch_one(&mut *connection).await.map_err(storage_error)?;
            let revision = revision.ok_or(StoreError::UnsupportedFormat)?;
            let parsed =
                ContentRevision::new(&revision).map_err(|_| StoreError::UnsupportedFormat)?;
            StrongEtag::from_content_revision(&parsed)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            verify_durable_base_chain(
                &mut *connection,
                registration,
                binding,
                &intent.document_id,
                i64::try_from(item.generation).map_err(|_| StoreError::UnsupportedFormat)?,
                seq,
                &revision,
                &item.payload_hash,
                item.bytes.len() as i64,
                ack,
            )
            .await?;
            if confirmed < seq {
                Ok(None)
            } else {
                Ok(Some((intent.document_id, item.generation)))
            }
        }
    }
}

async fn merge_followup_candidate_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    intent: &ContentConflictIntent,
) -> Result<Option<crate::OutboxOperation>, StoreError> {
    let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND status IN ('queued','sent') ORDER BY operation_seq LIMIT 1")
        .bind(registration).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
    if kind != "content" {
        return Ok(None);
    }
    let item = crate::outbox::operation(&row, binding, false)?;
    if item.document_id != intent.document_id
        || item.expected_revision.is_some()
        || !matches!(
            item.status,
            crate::OutboxStatus::Queued | crate::OutboxStatus::Abandoning
        )
    {
        return Ok(None);
    }
    let sequence = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
    let (ack, next): (i64, i64) =
        sqlx::query_as("SELECT ack_through,next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration)
            .fetch_one(&mut *connection)
            .await
            .map_err(storage_error)?;
    if sequence <= ack
        || sequence >= next
        || item.generation > intent.generation
        || (item.generation == intent.generation && item.bytes != intent.local_bytes)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let through =
        verified_merge_predecessors(&mut *connection, registration, binding, intent, sequence)
            .await?;
    let previous: String = sqlx::query_scalar(
        "SELECT operation_id FROM client_outbox WHERE registration_id=? AND operation_seq=?",
    )
    .bind(registration)
    .bind(through)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
    if dependency.as_deref() != Some(previous.as_str()) {
        return Err(StoreError::UnsupportedFormat);
    }
    if item.abandon_state == crate::OutboxAbandonState::None {
        let current = content_followup_on_connection(
            &mut *connection,
            registration,
            binding,
            &intent.operation_id,
            None,
            false,
        )
        .await?
        .ok_or(StoreError::OutboxBlocked)?;
        if current != item {
            return Err(StoreError::UnsupportedFormat);
        }
    }
    Ok(Some(item))
}

async fn merged_followup_intent(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
    expected_version: u64,
) -> Result<ContentConflictIntent, StoreError> {
    let intent = read_intent(&mut *connection, registration, binding, operation_id)
        .await?
        .ok_or(StoreError::InvalidRequest)?;
    verify_history_extent(&mut *connection, registration, operation_id, intent.version).await?;
    if intent.version != expected_version {
        return Err(StoreError::InvalidRequest);
    }
    let ContentConflictChoice::SaveMerged { bytes } = &intent.choice else {
        return Err(StoreError::InvalidRequest);
    };
    if intent.merged_hash.as_deref() != Some(digest(bytes).as_str()) {
        return Err(StoreError::InvalidRequest);
    }
    Ok(intent)
}

pub(crate) async fn merged_followup_requires_approval(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    item: &crate::OutboxOperation,
) -> Result<bool, StoreError> {
    let mut row = sqlx::query(
        "SELECT dependency FROM client_outbox WHERE registration_id=? AND operation_id=?",
    )
    .bind(registration)
    .bind(&item.operation_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(storage_error)?
    .ok_or(StoreError::UnsupportedFormat)?;
    let mut sequence = item.operation_seq;
    loop {
        let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
        let Some(id) = dependency else {
            return Ok(false);
        };
        row = sqlx::query("SELECT operation_kind,document_id,operation_seq,status,terminal_kind,dependency FROM client_outbox WHERE registration_id=? AND operation_id=?")
            .bind(registration).bind(&id).fetch_optional(&mut *connection).await.map_err(storage_error)?
            .ok_or(StoreError::UnsupportedFormat)?;
        let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
        let document: String = row.try_get("document_id").map_err(storage_error)?;
        let previous: i64 = row.try_get("operation_seq").map_err(storage_error)?;
        let previous = u64::try_from(previous).map_err(|_| StoreError::UnsupportedFormat)?;
        if kind != "content"
            || document != item.document_id
            || previous == 0
            || previous >= sequence
        {
            return Err(StoreError::UnsupportedFormat);
        }
        let status: String = row.try_get("status").map_err(storage_error)?;
        let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
        if status == "conflict"
            && terminal.is_none()
            && let Some(intent) = read_intent(&mut *connection, registration, binding, &id).await?
        {
            verify_history_extent(&mut *connection, registration, &id, intent.version).await?;
            if matches!(intent.choice, ContentConflictChoice::SaveMerged { .. }) {
                return Ok(true);
            }
        }
        sequence = previous;
    }
}

impl ClientStore {
    pub async fn inspect_content_conflict_merge_followup(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
    ) -> Result<Option<crate::OutboxOperation>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let intent = merged_followup_intent(
            &mut tx,
            registration,
            binding,
            operation_id,
            expected_version,
        )
        .await?;
        let candidate =
            merge_followup_candidate_on_connection(&mut tx, registration, binding, &intent).await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(candidate)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
    )]
    pub async fn request_content_conflict_merge_followup(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
        followup_id: &str,
        followup_seq: u64,
        followup_generation: u64,
        followup_hash: &str,
    ) -> Result<crate::OutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        followup_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let intent = merged_followup_intent(
            &mut tx,
            registration,
            binding,
            operation_id,
            expected_version,
        )
        .await?;
        let row =
            sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration)
                .bind(followup_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
        if let Some(row) = row {
            let item = crate::outbox::operation(&row, binding, false)?;
            if item.status == crate::OutboxStatus::Abandoned {
                if item.document_id != intent.document_id
                    || item.operation_seq != followup_seq
                    || item.generation != followup_generation
                    || item.payload_hash != followup_hash
                    || item.expected_revision.is_some()
                {
                    return Err(StoreError::InvalidRequest);
                }
                let seq =
                    i64::try_from(item.operation_seq).map_err(|_| StoreError::InvalidRequest)?;
                let before = seq.checked_add(1).ok_or(StoreError::InvalidRequest)?;
                if verified_merge_predecessors(&mut tx, registration, binding, &intent, before)
                    .await?
                    != seq
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                tx.commit().await.map_err(storage_error)?;
                return Ok(item);
            }
        }
        let item = merge_followup_candidate_on_connection(&mut tx, registration, binding, &intent)
            .await?
            .ok_or(StoreError::OutboxBlocked)?;
        if item.operation_id != followup_id
            || item.operation_seq != followup_seq
            || item.generation != followup_generation
            || item.payload_hash != followup_hash
        {
            return Err(StoreError::InvalidRequest);
        }
        let result = if item.abandon_state == crate::OutboxAbandonState::None {
            content_followup_on_connection(
                &mut tx,
                registration,
                binding,
                operation_id,
                Some(followup_id),
                true,
            )
            .await?
            .ok_or(StoreError::OutboxBlocked)?
        } else {
            item
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn read_queued_content_conflict_merge(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
    ) -> Result<Option<crate::OutboxOperation>, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM content_conflict_merged_operations WHERE registration_id=? AND operation_id=?)")
            .bind(registration).bind(operation_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if exists == 0 {
            tx.commit().await.map_err(storage_error)?;
            return Ok(None);
        }
        let intent = read_intent(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::UnsupportedFormat)?;
        verify_history_extent(&mut tx, registration, operation_id, intent.version).await?;
        if intent.version != expected_version {
            return Err(StoreError::InvalidRequest);
        }
        verified_completed_merge(&mut tx, registration, binding, operation_id).await?;
        let mut item = merged_operation_on_connection(&mut tx, registration, binding, &intent)
            .await?
            .ok_or(StoreError::UnsupportedFormat)?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        item.frozen = match status.as_str() {
            "active" => false,
            "frozen" => true,
            _ => return Err(StoreError::UnsupportedFormat),
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(Some(item))
    }

    pub async fn queue_content_conflict_merge(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
    ) -> Result<crate::OutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let intent = read_intent(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        verify_history_extent(&mut tx, registration, operation_id, intent.version).await?;
        if intent.version != expected_version {
            return Err(StoreError::InvalidRequest);
        }
        if let Some(existing) =
            merged_operation_on_connection(&mut tx, registration, binding, &intent).await?
        {
            tx.commit().await.map_err(storage_error)?;
            return Ok(existing);
        }
        let ContentConflictChoice::SaveMerged { bytes } = &intent.choice else {
            return Err(StoreError::InvalidRequest);
        };
        if intent.merged_hash.as_deref() != Some(digest(bytes).as_str()) {
            return Err(StoreError::InvalidRequest);
        }
        let (completed, through): (Option<i64>, Option<i64>) = sqlx::query_as("SELECT completed_generation,completed_through FROM content_conflict_intents WHERE registration_id=? AND operation_id=?")
            .bind(registration).bind(operation_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if completed.is_some() || through.is_some() {
            return Err(StoreError::InvalidRequest);
        }
        let storage: Option<String> = sqlx::query_scalar(
            "SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?",
        )
        .bind(registration)
        .bind(&intent.document_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let storage = storage.ok_or(StoreError::MissingDraft)?;
        if self.sessions.contains_key(&storage) {
            return Err(StoreError::SessionAlreadyOpen);
        }
        let snapshot = inspect_content_conflict_on_connection(
            &mut tx,
            registration,
            binding,
            &intent.document_id,
            operation_id,
        )
        .await?;
        if snapshot.generation != intent.generation
            || snapshot.local != intent.local_bytes
            || snapshot.base != intent.base_bytes
            || snapshot.base_revision != intent.base_revision
        {
            return Err(StoreError::InvalidRequest);
        }
        let node = intent
            .document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::UnsupportedFormat)?;
        let remote =
            crate::content_cache::observe_remote_file_on_connection(&mut tx, registration, node)
                .await?;
        let crate::RemoteFileObservation::Live(remote) = remote else {
            return Err(StoreError::InvalidRequest);
        };
        let (revision, hash, size) = crate::content_cache::validated_content(&remote)
            .map_err(|_| StoreError::UnsupportedFormat)?;
        if revision != intent.remote_revision
            || hash != intent.remote_hash
            || size != intent.remote_bytes.len() as u64
        {
            return Err(StoreError::InvalidRequest);
        }
        if crate::outbox::requires_reconciliation(
            &mut tx,
            &binding.workspace_id,
            &intent.document_id,
        )
        .await?
        {
            return Err(StoreError::ReconciliationRequired);
        }
        let next: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let through =
            verified_merge_predecessors(&mut tx, registration, binding, &intent, next).await?;
        let generation = intent
            .generation
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(StoreError::InvalidGeneration)?;
        let changed = sqlx::query("UPDATE draft_checkpoints SET generation=?,bytes=? WHERE document_id=? AND generation=? AND format_version=1 AND bytes=?")
            .bind(generation).bind(bytes).bind(&storage).bind(intent.generation as i64).bind(&intent.local_bytes)
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidRequest);
        }
        let changed = sqlx::query("UPDATE draft_bases SET bytes=?,revision=? WHERE document_id=? AND bytes=? AND revision=?")
            .bind(&intent.remote_bytes).bind(&intent.remote_revision).bind(&storage).bind(&intent.base_bytes).bind(&intent.base_revision)
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidRequest);
        }
        let after = next.checked_add(1).ok_or(StoreError::InvalidGeneration)?;
        let changed = sqlx::query(
            "UPDATE outbox_streams SET next_seq=? WHERE registration_id=? AND next_seq=?",
        )
        .bind(after)
        .bind(registration)
        .bind(next)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::Storage);
        }
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,bytes,payload_hash,payload_len,status) VALUES(?,?,?,'content',?,?,?,?,?,?,?,'queued')")
            .bind(&id).bind(registration).bind(next).bind(&intent.document_id).bind(&storage).bind(generation)
            .bind(&intent.remote_revision).bind(bytes).bind(digest(bytes)).bind(bytes.len() as i64)
            .execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("INSERT INTO content_conflict_merged_operations(operation_id,registration_id,intent_version,content_operation_id,abandoned_through) VALUES(?,?,?,?,?)")
            .bind(operation_id).bind(registration).bind(i64::try_from(intent.version).map_err(|_| StoreError::InvalidRequest)?).bind(&id).bind(through)
            .execute(&mut *tx).await.map_err(storage_error)?;
        let item = merged_operation_on_connection(&mut tx, registration, binding, &intent)
            .await?
            .ok_or(StoreError::Storage)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(item)
    }

    pub async fn reconfirm_failed_content_conflict_copy(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
        expected_create_operation_id: &str,
        parent_id: &str,
        name: &str,
    ) -> Result<ContentConflictCopyIntent, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        expected_create_operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let parent = parent_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        notryx_domain::mutation::validate_name(name).map_err(|_| StoreError::InvalidRequest)?;
        if !name.to_ascii_lowercase().ends_with(".md") {
            return Err(StoreError::InvalidRequest);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_copy_on_connection(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let intent = read_intent(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        if copy.intent_version != expected_version
            || intent.version != expected_version
            || intent.choice != ContentConflictChoice::KeepLocalCopy
        {
            return Err(StoreError::InvalidRequest);
        }
        let old_row = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(expected_create_operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let old_registration: i64 = old_row.try_get("registration_id").map_err(storage_error)?;
        let kind: String = old_row.try_get("operation_kind").map_err(storage_error)?;
        if old_registration != registration || kind != "create" {
            return Err(StoreError::InvalidRequest);
        }
        let old = crate::outbox::validated_create_record(&old_row, binding)?;
        let raw: String = old_row.try_get("receipt_json").map_err(storage_error)?;
        let receipt: notryx_protocol::OperationReceipt =
            serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
        if old.status != crate::OutboxStatus::Failed
            || !matches!(
                receipt,
                notryx_protocol::OperationReceipt::DestinationExists { .. }
            )
        {
            return Err(StoreError::InvalidRequest);
        }
        let (ack, confirmed, next): (i64,i64,i64) = sqlx::query_as("SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let sequence =
            i64::try_from(old.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        if sequence <= 0 || confirmed < sequence || confirmed > ack || ack >= next {
            return Err(StoreError::InvalidRequest);
        }
        if copy.create_operation_id != expected_create_operation_id {
            type CopyAttemptRow = (String, String, String, String, String, String, String, i64);
            let previous: Option<CopyAttemptRow> = sqlx::query_as("SELECT old_parent_id,old_name,old_node_id,new_parent_id,new_name,new_node_id,new_create_operation_id,intent_version FROM content_conflict_copy_attempts WHERE registration_id=? AND operation_id=? AND old_create_operation_id=?")
                .bind(registration).bind(operation_id).bind(expected_create_operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
            if copy.parent_id == parent_id
                && copy.name == name
                && old.request.kind == notryx_protocol::NodeKind::File
                && previous
                    == Some((
                        old.request.parent_id.to_string(),
                        old.request.name.clone(),
                        old.request.node_id.to_string(),
                        parent_id.to_owned(),
                        name.to_owned(),
                        copy.node_id.clone(),
                        copy.create_operation_id.clone(),
                        i64::try_from(expected_version)
                            .map_err(|_| StoreError::InvalidGeneration)?,
                    ))
            {
                tx.commit().await.map_err(storage_error)?;
                return Ok(copy);
            }
            return Err(StoreError::InvalidRequest);
        }
        if old.request.node_id.to_string() != copy.node_id
            || old.request.parent_id.to_string() != copy.parent_id
            || old.request.name != copy.name
            || old.request.kind != notryx_protocol::NodeKind::File
            || (copy.parent_id == parent_id && copy.name == name)
        {
            return Err(StoreError::InvalidRequest);
        }
        let linked: Option<String> = sqlx::query_scalar("SELECT content_operation_id FROM content_conflict_copies WHERE registration_id=? AND operation_id=?")
            .bind(registration).bind(operation_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if linked.is_some() {
            return Err(StoreError::InvalidRequest);
        }
        let parent_state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(parent_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let parent_state: notryx_protocol::NodeState =
            serde_json::from_str(&parent_state.ok_or(StoreError::InvalidRequest)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if parent_state.node_id != parent
            || parent_state.deleted
            || parent_state.kind != notryx_protocol::NodeKind::Folder
        {
            return Err(StoreError::InvalidRequest);
        }
        let node_id = uuid::Uuid::now_v7().to_string();
        let create_operation_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO content_conflict_copy_attempts(old_create_operation_id,operation_id,registration_id,intent_version,old_parent_id,old_name,old_node_id,new_parent_id,new_name,new_node_id,new_create_operation_id) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
            .bind(expected_create_operation_id).bind(operation_id).bind(registration).bind(i64::try_from(expected_version).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(&copy.parent_id).bind(&copy.name).bind(&copy.node_id).bind(parent_id).bind(name).bind(&node_id).bind(&create_operation_id)
            .execute(&mut *tx).await.map_err(storage_error)?;
        let updated = sqlx::query("UPDATE content_conflict_copies SET parent_id=?,name=?,node_id=?,create_operation_id=? WHERE operation_id=? AND registration_id=? AND intent_version=? AND create_operation_id=? AND content_operation_id IS NULL")
            .bind(parent_id).bind(name).bind(&node_id).bind(&create_operation_id).bind(operation_id).bind(registration)
            .bind(i64::try_from(expected_version).map_err(|_| StoreError::InvalidGeneration)?).bind(expected_create_operation_id)
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if updated != 1 {
            return Err(StoreError::InvalidRequest);
        }
        let result = ContentConflictCopyIntent {
            parent_id: parent_id.to_owned(),
            name: name.to_owned(),
            node_id,
            create_operation_id,
            ..copy
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn inspect_content_conflict_copy_followup(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
    ) -> Result<Option<crate::OutboxOperation>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_copy_on_connection(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let intent = read_intent(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        if copy.intent_version != expected_version
            || intent.version != expected_version
            || intent.choice != ContentConflictChoice::KeepLocalCopy
        {
            return Err(StoreError::InvalidRequest);
        }
        let result = content_followup_on_connection(
            &mut tx,
            registration,
            binding,
            operation_id,
            None,
            false,
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn request_content_conflict_copy_followup(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
        approved_operation_id: &str,
    ) -> Result<crate::OutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        approved_operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_copy_on_connection(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let intent = read_intent(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        if copy.intent_version != expected_version
            || intent.version != expected_version
            || intent.choice != ContentConflictChoice::KeepLocalCopy
        {
            return Err(StoreError::InvalidRequest);
        }
        let result = content_followup_on_connection(
            &mut tx,
            registration,
            binding,
            operation_id,
            Some(approved_operation_id),
            true,
        )
        .await?
        .ok_or(StoreError::InvalidRequest)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn read_content_conflict_copy_content_state(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
    ) -> Result<ContentConflictCopyContentState, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let status = copy_content_state_on_connection(
            &mut tx,
            registration,
            binding,
            operation_id,
            expected_version,
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(status)
    }

    pub async fn queue_content_conflict_copy_content(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
    ) -> Result<crate::OutboxOperation, StoreError> {
        let status = self
            .read_content_conflict_copy_content_state(binding, operation_id, expected_version)
            .await?;
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_copy_on_connection(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        if copy.intent_version != expected_version {
            return Err(StoreError::InvalidRequest);
        }
        let row = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(&copy.create_operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let row_registration: i64 = row.try_get("registration_id").map_err(storage_error)?;
        let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
        if row_registration != registration || kind != "create" {
            return Err(StoreError::UnsupportedFormat);
        }
        let create = crate::outbox::validated_create_record(&row, binding)?;
        if create.status != crate::OutboxStatus::Committed
            || create.request.node_id.to_string() != copy.node_id
            || create.request.parent_id.to_string() != copy.parent_id
            || create.request.name != copy.name
            || create.request.kind != notryx_protocol::NodeKind::File
        {
            return Err(StoreError::InvalidRequest);
        }
        let raw: String = row.try_get("receipt_json").map_err(storage_error)?;
        let receipt: notryx_protocol::OperationReceipt =
            serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
        let notryx_protocol::OperationReceipt::Created { node, .. } = receipt else {
            return Err(StoreError::UnsupportedFormat);
        };
        let revision = node
            .content_rev
            .ok_or(StoreError::UnsupportedFormat)?
            .as_str()
            .to_owned();
        let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as("SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let create_seq =
            i64::try_from(create.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        if create_seq <= 0 || confirmed < create_seq || confirmed > ack || ack >= next {
            return Err(StoreError::InvalidRequest);
        }
        let linked: Option<String> = sqlx::query_scalar("SELECT content_operation_id FROM content_conflict_copies WHERE operation_id=? AND registration_id=?")
            .bind(operation_id).bind(registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if let Some(id) = linked {
            let row = sqlx::query(
                "SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?",
            )
            .bind(registration)
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?
            .ok_or(StoreError::UnsupportedFormat)?;
            let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
            let item = if retired == 0 {
                crate::outbox::operation(&row, binding, false)?
            } else if retired == 1
                && status
                    == (ContentConflictCopyContentState::Saved {
                        operation_id: id.clone(),
                    })
            {
                let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
                let stored_status: String = row.try_get("status").map_err(storage_error)?;
                if !bytes.is_empty() || stored_status != "committed" {
                    return Err(StoreError::UnsupportedFormat);
                }
                let reconstructed = sqlx::query("SELECT operation_id,operation_seq,operation_kind,document_id,generation,expected_revision,conflict_revision,? AS bytes,payload_hash,payload_len,status,abandon_state,terminal_kind,0 AS payload_retired FROM client_outbox WHERE registration_id=? AND operation_id=?")
                    .bind(&copy.local_bytes).bind(registration).bind(&id).fetch_one(&mut *tx).await.map_err(storage_error)?;
                crate::outbox::operation(&reconstructed, binding, false)?
            } else {
                return Err(StoreError::UnsupportedFormat);
            };
            if item.document_id != copy.node_id
                || item.generation != 1
                || item.bytes != copy.local_bytes
                || item.expected_revision.as_deref() != Some(revision.as_str())
                || item.operation_seq <= create.operation_seq
            {
                return Err(StoreError::UnsupportedFormat);
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(item);
        }
        let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content')")
            .bind(registration).bind(&copy.node_id)
            .fetch_one(&mut *tx).await.map_err(storage_error)?;
        if exists != 0 {
            return Err(StoreError::OutboxBlocked);
        }
        if crate::outbox::requires_reconciliation(&mut tx, &binding.workspace_id, &copy.node_id)
            .await?
        {
            return Err(StoreError::ReconciliationRequired);
        }
        let existing_storage: Option<String> = sqlx::query_scalar(
            "SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?",
        )
        .bind(registration)
        .bind(&copy.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let storage = if let Some(storage) = existing_storage {
            let stored: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM draft_checkpoints WHERE document_id=? UNION ALL SELECT 1 FROM draft_bases WHERE document_id=?)")
                .bind(&storage).bind(&storage).fetch_one(&mut *tx).await.map_err(storage_error)?;
            if self.sessions.contains_key(&storage) || stored != 0 {
                return Err(StoreError::OutboxBlocked);
            }
            storage
        } else {
            let storage = format!("r_{}", uuid::Uuid::new_v4().simple());
            sqlx::query("INSERT INTO registered_documents(registration_id,document_id,storage_id) VALUES(?,?,?)")
                .bind(registration).bind(&copy.node_id).bind(&storage).execute(&mut *tx).await.map_err(storage_error)?;
            storage
        };
        sqlx::query("INSERT INTO draft_checkpoints(document_id,generation,format_version,bytes) VALUES(?,1,1,?)")
            .bind(&storage).bind(&copy.local_bytes).execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("INSERT INTO draft_bases(document_id,bytes,revision) VALUES(?,X'',?)")
            .bind(&storage)
            .bind(&revision)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let after = next.checked_add(1).ok_or(StoreError::InvalidGeneration)?;
        sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
            .bind(after)
            .bind(registration)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let id = uuid::Uuid::now_v7().to_string();
        let hash = digest(&copy.local_bytes);
        sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,bytes,payload_hash,payload_len,status) VALUES(?,?,?,'content',?,?,1,?,?,?,?,'queued')")
            .bind(&id).bind(registration).bind(next).bind(&copy.node_id).bind(&storage).bind(&revision)
            .bind(&copy.local_bytes).bind(&hash).bind(copy.local_bytes.len() as i64).execute(&mut *tx).await.map_err(storage_error)?;
        let changed = sqlx::query("UPDATE content_conflict_copies SET content_operation_id=? WHERE operation_id=? AND registration_id=? AND content_operation_id IS NULL")
            .bind(&id).bind(operation_id).bind(registration).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::Storage);
        }
        let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id=?")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
        let item = crate::outbox::operation(&row, binding, false)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(item)
    }

    pub async fn queue_content_conflict_copy(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        expected_version: u64,
    ) -> Result<crate::CreateOutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_copy_on_connection(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        if copy.intent_version != expected_version {
            return Err(StoreError::InvalidRequest);
        }
        let operation =
            queue_copy_create(&mut tx, registration, binding, &CopyDetails::from(&copy)).await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(operation)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
    )]
    pub async fn record_content_conflict_copy(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        operation_id: &str,
        request: &ContentConflictIntentRequest,
        parent_id: &str,
        name: &str,
        expected_intent_version: Option<u64>,
    ) -> Result<ContentConflictCopyIntent, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let node = document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let parent = parent_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        notryx_domain::mutation::validate_name(name).map_err(|_| StoreError::InvalidRequest)?;
        if !name.to_ascii_lowercase().ends_with(".md")
            || request.choice != ContentConflictChoice::KeepLocalCopy
        {
            return Err(StoreError::InvalidRequest);
        }
        validate_text(&request.remote_bytes)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        if let Some(copy) =
            read_copy_on_connection(&mut tx, registration, binding, operation_id).await?
        {
            let intent = read_intent(&mut tx, registration, binding, operation_id)
                .await?
                .ok_or(StoreError::UnsupportedFormat)?;
            let version_matches = match expected_intent_version {
                None => copy.intent_version == 0,
                Some(version) if copy.intent_version == version => true,
                Some(version) if version.checked_add(1) == Some(copy.intent_version) => {
                    read_history_entry(
                        &mut tx,
                        registration,
                        binding,
                        operation_id,
                        copy.intent_version,
                    )
                    .await?
                    .is_some_and(|old| old.version == version)
                }
                _ => false,
            };
            if copy.document_id == document_id
                && copy.parent_id == parent_id
                && copy.name == name
                && version_matches
                && intent.generation == request.generation
                && intent.remote_revision == request.remote_revision
                && intent.remote_hash == request.remote_hash
                && intent.remote_bytes == request.remote_bytes
                && intent.choice == request.choice
            {
                tx.commit().await.map_err(storage_error)?;
                return Ok(copy);
            }
            return Err(StoreError::InvalidRequest);
        }
        let parent_state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(parent_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let parent_state: notryx_protocol::NodeState =
            serde_json::from_str(&parent_state.ok_or(StoreError::InvalidRequest)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if parent_state.node_id != parent
            || parent_state.deleted
            || parent_state.kind != notryx_protocol::NodeKind::Folder
        {
            return Err(StoreError::InvalidRequest);
        }
        let intent = if let Some(expected_version) = expected_intent_version {
            let current = read_intent(&mut tx, registration, binding, operation_id)
                .await?
                .ok_or(StoreError::InvalidRequest)?;
            verify_history_extent(&mut tx, registration, operation_id, current.version).await?;
            if current.version != expected_version || current.document_id != document_id {
                return Err(StoreError::InvalidRequest);
            }
            if current.choice != request.choice
                || current.generation != request.generation
                || current.remote_revision != request.remote_revision
                || current.remote_hash != request.remote_hash
                || current.remote_bytes != request.remote_bytes
            {
                replace_intent_on_connection(
                    &mut tx,
                    registration,
                    binding,
                    document_id,
                    operation_id,
                    &current,
                    request,
                    node,
                )
                .await?
            } else {
                let snapshot = inspect_content_conflict_on_connection(
                    &mut tx,
                    registration,
                    binding,
                    document_id,
                    operation_id,
                )
                .await?;
                if snapshot.generation != current.generation
                    || snapshot.base_revision != current.base_revision
                    || snapshot.base != current.base_bytes
                    || snapshot.local != current.local_bytes
                {
                    return Err(StoreError::InvalidRequest);
                }
                let remote = crate::content_cache::observe_remote_file_on_connection(
                    &mut tx,
                    registration,
                    node,
                )
                .await?;
                let crate::RemoteFileObservation::Live(remote) = remote else {
                    return Err(StoreError::InvalidRequest);
                };
                let (revision, hash, size) = crate::content_cache::validated_content(&remote)
                    .map_err(|_| StoreError::UnsupportedFormat)?;
                if revision != current.remote_revision
                    || hash != current.remote_hash
                    || size != current.remote_bytes.len() as u64
                    || digest(&current.remote_bytes) != current.remote_hash
                {
                    return Err(StoreError::InvalidRequest);
                }
                current
            }
        } else {
            record_intent_on_connection(
                &mut tx,
                registration,
                binding,
                document_id,
                operation_id,
                request,
                node,
            )
            .await?
        };
        let node_id = uuid::Uuid::now_v7().to_string();
        let create_operation_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO content_conflict_copies(operation_id,registration_id,intent_version,parent_id,name,node_id,create_operation_id) VALUES(?,?,?,?,?,?,?)")
            .bind(operation_id).bind(registration).bind(i64::try_from(intent.version).map_err(|_| StoreError::InvalidGeneration)?)
            .bind(parent_id).bind(name).bind(&node_id).bind(&create_operation_id)
            .execute(&mut *tx).await.map_err(storage_error)?;
        let copy = ContentConflictCopyIntent {
            binding: binding.clone(),
            document_id: document_id.to_owned(),
            conflict_operation_id: operation_id.to_owned(),
            intent_version: intent.version,
            generation: intent.generation,
            parent_id: parent_id.to_owned(),
            name: name.to_owned(),
            local_bytes: intent.local_bytes,
            node_id,
            create_operation_id,
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(copy)
    }

    pub async fn read_content_conflict_copy(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
    ) -> Result<Option<ContentConflictCopyIntent>, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        read_copy_on_connection(&mut self.connection, registration, binding, operation_id).await
    }
    pub async fn content_conflict_blocking_fifo(
        &mut self,
        binding: &RegistrationBinding,
        expected: &crate::OutboxOperation,
    ) -> Result<Option<ContentConflictRef>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND status IN ('queued','sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
        if kind != "content" {
            return Ok(None);
        }
        let item = crate::outbox::operation(&row, binding, false)?;
        if item != *expected
            || item.status != crate::OutboxStatus::Queued
            || item.expected_revision.is_some()
        {
            return Ok(None);
        }
        if crate::outbox::requires_reconciliation(&mut tx, &binding.workspace_id, &item.document_id)
            .await?
        {
            return Err(StoreError::ReconciliationRequired);
        }
        if let Some((candidate, candidate_item)) =
            crate::deleted_draft_copy::blocking_followup_on_connection(
                &mut tx,
                registration,
                binding,
                &item.document_id,
            )
            .await?
            && candidate_item == item
        {
            let sequence: i64 = sqlx::query_scalar("SELECT operation_seq FROM client_outbox WHERE registration_id=? AND operation_id=?")
                    .bind(registration).bind(&candidate.blocker_operation_id)
                    .fetch_one(&mut *tx).await.map_err(storage_error)?;
            let result = ContentConflictRef {
                operation_id: candidate.blocker_operation_id,
                operation_seq: u64::try_from(sequence)
                    .map_err(|_| StoreError::UnsupportedFormat)?,
            };
            tx.commit().await.map_err(storage_error)?;
            return Ok(Some(result));
        }
        let mut dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
        let mut sequence = item.operation_seq;
        while let Some(id) = dependency {
            let row = sqlx::query(
                "SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?",
            )
            .bind(registration)
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;
            let Some(row) = row else {
                return Err(StoreError::UnsupportedFormat);
            };
            let previous = crate::outbox::operation(&row, binding, false)?;
            if previous.document_id != item.document_id || previous.operation_seq >= sequence {
                return Err(StoreError::UnsupportedFormat);
            }
            if previous.status == crate::OutboxStatus::Conflict {
                let snapshot = inspect_content_conflict_on_connection(
                    &mut tx,
                    registration,
                    binding,
                    &item.document_id,
                    &id,
                )
                .await?;
                let intent_exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM content_conflict_intents WHERE registration_id=? AND operation_id=?)")
                    .bind(registration).bind(&id).fetch_one(&mut *tx).await.map_err(storage_error)?;
                if intent_exists != 0
                    && verified_completed_choose_remote(&mut tx, registration, binding, &id)
                        .await?
                        .is_some()
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                tx.commit().await.map_err(storage_error)?;
                return Ok(Some(ContentConflictRef {
                    operation_id: snapshot.operation_id,
                    operation_seq: snapshot.operation_seq,
                }));
            }
            if previous.status != crate::OutboxStatus::Abandoned {
                return Ok(None);
            }
            let ack: i64 = sqlx::query_scalar(
                "SELECT ack_through FROM outbox_streams WHERE registration_id=?",
            )
            .bind(registration)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
            if i64::try_from(previous.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?
                > ack
            {
                return Err(StoreError::UnsupportedFormat);
            }
            sequence = previous.operation_seq;
            dependency = row.try_get("dependency").map_err(storage_error)?;
        }
        Ok(None)
    }

    /// Reads durable local evidence without checking the current server state.
    pub async fn read_content_conflict_resolution_state(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
    ) -> Result<ContentConflictResolutionState, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM client_registrations WHERE id=?")
                .bind(registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let frozen = match status.as_str() {
            "active" => false,
            "frozen" => true,
            _ => return Err(StoreError::UnsupportedFormat),
        };
        let Some(intent) = read_intent(&mut tx, registration, binding, operation_id).await? else {
            verify_history_extent(&mut tx, registration, operation_id, 0).await?;
            tx.commit().await.map_err(storage_error)?;
            return Ok(ContentConflictResolutionState::NotFound);
        };
        verify_history_extent(&mut tx, registration, operation_id, intent.version).await?;
        if let Some((result, _)) =
            verified_completed_choose_remote(&mut tx, registration, binding, operation_id).await?
        {
            tx.commit().await.map_err(storage_error)?;
            return Ok(ContentConflictResolutionState::Completed {
                generation: result.generation,
                frozen,
            });
        }
        if let Some((document, generation)) =
            verified_completed_merge(&mut tx, registration, binding, operation_id).await?
        {
            if document != intent.document_id {
                return Err(StoreError::UnsupportedFormat);
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(ContentConflictResolutionState::Completed { generation, frozen });
        }
        if let Some(merged) =
            merged_operation_on_connection(&mut tx, registration, binding, &intent).await?
        {
            let confirmed: i64 = sqlx::query_scalar(
                "SELECT confirmed_through FROM outbox_streams WHERE registration_id=?",
            )
            .bind(registration)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
            let needs_review = matches!(
                merged.status,
                crate::OutboxStatus::Conflict
                    | crate::OutboxStatus::Failed
                    | crate::OutboxStatus::Abandoned
            ) && u64::try_from(confirmed)
                .is_ok_and(|value| value >= merged.operation_seq);
            tx.commit().await.map_err(storage_error)?;
            return Ok(ContentConflictResolutionState::Pending {
                version: intent.version,
                generation: intent.generation,
                choice: ContentConflictChoiceKind::SaveMerged,
                needs_review,
                frozen,
            });
        }
        let snapshot = inspect_content_conflict_on_connection(
            &mut tx,
            registration,
            binding,
            &intent.document_id,
            operation_id,
        )
        .await?;
        if snapshot.generation < intent.generation
            || (snapshot.generation == intent.generation
                && (snapshot.local != intent.local_bytes
                    || snapshot.base != intent.base_bytes
                    || snapshot.base_revision != intent.base_revision))
        {
            return Err(StoreError::UnsupportedFormat);
        }
        let needs_review = snapshot.generation != intent.generation
            || snapshot.local != intent.local_bytes
            || snapshot.base != intent.base_bytes
            || snapshot.base_revision != intent.base_revision
            || (matches!(&intent.choice, ContentConflictChoice::SaveMerged { .. })
                && intent.merged_hash.is_none());
        tx.commit().await.map_err(storage_error)?;
        let choice = match intent.choice {
            ContentConflictChoice::KeepLocalCopy => ContentConflictChoiceKind::KeepLocalCopy,
            ContentConflictChoice::ChooseRemote => ContentConflictChoiceKind::ChooseRemote,
            ContentConflictChoice::SaveMerged { .. } => ContentConflictChoiceKind::SaveMerged,
        };
        Ok(ContentConflictResolutionState::Pending {
            version: intent.version,
            generation: intent.generation,
            choice,
            needs_review,
            frozen,
        })
    }

    pub async fn read_content_conflict_intent_history(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
        version: u64,
    ) -> Result<Option<ContentConflictIntent>, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let current = read_intent(&mut tx, registration, binding, operation_id).await?;
        let limit = current.as_ref().map_or(0, |intent| intent.version);
        verify_history_extent(&mut tx, registration, operation_id, limit).await?;
        let entry = if version >= limit {
            None
        } else {
            let sequence = version
                .checked_add(1)
                .ok_or(StoreError::UnsupportedFormat)?;
            let entry = read_history_entry(&mut tx, registration, binding, operation_id, sequence)
                .await?
                .ok_or(StoreError::UnsupportedFormat)?;
            if entry.version != version {
                return Err(StoreError::UnsupportedFormat);
            }
            Some(entry)
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(entry)
    }

    pub async fn replace_pending_content_conflict_intent(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        operation_id: &str,
        expected: &ContentConflictIntent,
        request: &ContentConflictIntentRequest,
    ) -> Result<ContentConflictIntent, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let node = document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        validate_text(&request.remote_bytes)?;
        if let ContentConflictChoice::SaveMerged { bytes } = &request.choice {
            validate_text(bytes)?;
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let result = replace_intent_on_connection(
            &mut tx,
            registration,
            binding,
            document_id,
            operation_id,
            expected,
            request,
            node,
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn queue_content_conflict_merge_closing_session(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
        operation_id: &str,
        version: u64,
    ) -> Result<crate::OutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let storage = self
            .registered_storage_id(registration, document_id)
            .await?
            .ok_or(StoreError::StaleSession)?;
        let document: Option<String> = sqlx::query_scalar("SELECT document_id FROM content_conflict_intents WHERE registration_id=? AND operation_id=?")
            .bind(registration).bind(operation_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        if document.as_deref() != Some(document_id) {
            return Err(StoreError::InvalidRequest);
        }
        if !self
            .sessions
            .get(&storage)
            .is_some_and(|session| session.id == edit_session_id)
        {
            return Err(StoreError::StaleSession);
        }
        let before = self
            .read_checkpoint(&storage)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        let session = self
            .sessions
            .remove(&storage)
            .ok_or(StoreError::StaleSession)?;
        match self
            .queue_content_conflict_merge(binding, operation_id, version)
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                if matches!(self.read_checkpoint(&storage).await, Ok(Some(current)) if current == before)
                {
                    self.sessions.insert(storage, session);
                }
                Err(error)
            }
        }
    }

    pub async fn complete_content_conflict_choose_remote_closing_session(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        edit_session_id: &str,
        operation_id: &str,
    ) -> Result<ContentConflictChooseRemoteResult, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let storage = self
            .registered_storage_id(registration, document_id)
            .await?
            .ok_or(StoreError::StaleSession)?;
        let document: Option<String> = sqlx::query_scalar("SELECT document_id FROM content_conflict_intents WHERE registration_id=? AND operation_id=?")
            .bind(registration).bind(operation_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        if document.as_deref() != Some(document_id) {
            return Err(StoreError::InvalidRequest);
        }
        if !self
            .sessions
            .get(&storage)
            .is_some_and(|session| session.id == edit_session_id)
        {
            return Err(StoreError::StaleSession);
        }
        let before = self
            .read_checkpoint(&storage)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        let session = self
            .sessions
            .remove(&storage)
            .ok_or(StoreError::StaleSession)?;
        match self
            .complete_content_conflict_choose_remote(binding, operation_id)
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                if matches!(self.read_checkpoint(&storage).await, Ok(Some(current)) if current == before)
                {
                    self.sessions.insert(storage, session);
                }
                Err(error)
            }
        }
    }

    pub async fn complete_content_conflict_choose_remote(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
    ) -> Result<ContentConflictChooseRemoteResult, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let storage: Option<String> = sqlx::query_scalar("SELECT d.storage_id FROM content_conflict_intents i JOIN registered_documents d ON d.registration_id=i.registration_id AND d.document_id=i.document_id WHERE i.registration_id=? AND i.operation_id=?")
            .bind(registration).bind(operation_id).fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        let storage = storage.ok_or(StoreError::InvalidRequest)?;
        if self.sessions.contains_key(&storage) {
            return Err(StoreError::SessionAlreadyOpen);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let active: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id=? AND status='active')",
        )
        .bind(registration)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        if active != 1 {
            return Err(StoreError::StaleRegistration);
        }
        let intent = read_intent(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        verify_history_extent(&mut tx, registration, operation_id, intent.version).await?;
        if intent.choice != ContentConflictChoice::ChooseRemote {
            return Err(StoreError::InvalidRequest);
        }
        let (completed, through): (Option<i64>, Option<i64>) = sqlx::query_as("SELECT completed_generation,completed_through FROM content_conflict_intents WHERE registration_id=? AND operation_id=?")
            .bind(registration).bind(operation_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if completed.is_none() && through.is_some() {
            return Err(StoreError::UnsupportedFormat);
        }
        let generation = intent
            .generation
            .checked_add(1)
            .and_then(|n| i64::try_from(n).ok())
            .ok_or(StoreError::InvalidGeneration)?;
        let result = ContentConflictChooseRemoteResult {
            binding: binding.clone(),
            document_id: intent.document_id.clone(),
            operation_id: operation_id.to_owned(),
            generation: generation as u64,
            remote_revision: intent.remote_revision.clone(),
            remote_bytes: intent.remote_bytes.clone(),
        };
        if let Some(value) = completed {
            if value != generation {
                return Err(StoreError::UnsupportedFormat);
            }
            let (saved, _) =
                verified_completed_choose_remote(&mut tx, registration, binding, operation_id)
                    .await?
                    .ok_or(StoreError::UnsupportedFormat)?;
            tx.commit().await.map_err(storage_error)?;
            return Ok(saved);
        }
        let snapshot = inspect_content_conflict_on_connection(
            &mut tx,
            registration,
            binding,
            &intent.document_id,
            operation_id,
        )
        .await?;
        if snapshot.generation != intent.generation
            || snapshot.local != intent.local_bytes
            || snapshot.base != intent.base_bytes
            || snapshot.base_revision != intent.base_revision
        {
            return Err(StoreError::InvalidRequest);
        }
        let node = intent
            .document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::UnsupportedFormat)?;
        let remote =
            crate::content_cache::observe_remote_file_on_connection(&mut tx, registration, node)
                .await?;
        let crate::RemoteFileObservation::Live(remote) = remote else {
            return Err(StoreError::InvalidRequest);
        };
        let (revision, hash, size) = crate::content_cache::validated_content(&remote)
            .map_err(|_| StoreError::UnsupportedFormat)?;
        if revision != intent.remote_revision
            || hash != intent.remote_hash
            || size != intent.remote_bytes.len() as u64
        {
            return Err(StoreError::InvalidRequest);
        }
        let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as("SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let mut after =
            i64::try_from(snapshot.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        if confirmed < after || confirmed > ack || ack >= next {
            return Err(StoreError::OutboxBlocked);
        }
        let mut previous = operation_id.to_owned();
        loop {
            let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' AND operation_seq>? ORDER BY operation_seq LIMIT 1")
                .bind(registration).bind(&intent.document_id).bind(after).fetch_optional(&mut *tx).await.map_err(storage_error)?;
            let Some(row) = row else {
                break;
            };
            let item = crate::outbox::operation(&row, binding, false)?;
            let seq =
                i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
            let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
            if dependency.as_deref() != Some(previous.as_str())
                || seq <= after
                || seq >= next
                || item.generation > intent.generation
                || (item.generation == intent.generation && item.bytes != intent.local_bytes)
            {
                return Err(StoreError::UnsupportedFormat);
            }
            if item.status != crate::OutboxStatus::Abandoned || seq > ack || seq > confirmed {
                return Err(StoreError::OutboxBlocked);
            }
            after = seq;
            previous = item.operation_id;
        }
        let changed = sqlx::query("UPDATE draft_checkpoints SET generation=?,bytes=? WHERE document_id=? AND generation=? AND format_version=1 AND bytes=?")
            .bind(generation).bind(&intent.remote_bytes).bind(&storage).bind(intent.generation as i64).bind(&intent.local_bytes)
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidRequest);
        }
        let changed = sqlx::query("UPDATE draft_bases SET bytes=?,revision=? WHERE document_id=? AND bytes=? AND revision=?")
            .bind(&intent.remote_bytes).bind(&intent.remote_revision).bind(&storage).bind(&intent.base_bytes).bind(&intent.base_revision)
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidRequest);
        }
        let changed = sqlx::query("UPDATE content_conflict_intents SET completed_generation=?,completed_through=? WHERE registration_id=? AND operation_id=? AND completed_generation IS NULL AND completed_through IS NULL")
            .bind(generation).bind(after).bind(registration).bind(operation_id).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidRequest);
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn request_content_conflict_followup_abandon(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
    ) -> Result<Option<crate::OutboxOperation>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let intent = read_intent(&mut tx, registration, binding, operation_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        if matches!(intent.choice, ContentConflictChoice::SaveMerged { .. }) {
            return Err(StoreError::InvalidRequest);
        }
        let result = content_followup_on_connection(
            &mut tx,
            registration,
            binding,
            operation_id,
            None,
            true,
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn record_content_conflict_intent(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        operation_id: &str,
        request: &ContentConflictIntentRequest,
    ) -> Result<ContentConflictIntent, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let node = document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        validate_text(&request.remote_bytes)?;
        if let ContentConflictChoice::SaveMerged { bytes } = &request.choice {
            validate_text(bytes)?;
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let intent = record_intent_on_connection(
            &mut tx,
            registration,
            binding,
            document_id,
            operation_id,
            request,
            node,
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(intent)
    }

    pub async fn read_content_conflict_intent(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
    ) -> Result<Option<ContentConflictIntent>, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        read_intent(&mut self.connection, registration, binding, operation_id).await
    }

    pub async fn first_content_conflict(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<Option<ContentConflictRef>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let rows = sqlx::query(
            "SELECT o.operation_id, i.operation_id AS intent_id FROM client_outbox o LEFT JOIN content_conflict_intents i ON i.registration_id=o.registration_id AND i.operation_id=o.operation_id WHERE o.registration_id=? AND o.document_id=? AND o.operation_kind='content' AND ((o.status='conflict' AND o.terminal_kind IS NULL) OR o.conflict_revision IS NOT NULL OR o.conflict_etag IS NOT NULL) ORDER BY o.operation_seq",
        )
        .bind(registration)
        .bind(document_id)
        .fetch_all(&mut self.connection)
        .await
        .map_err(storage_error)?;
        for row in rows {
            let operation_id: String = row.try_get("operation_id").map_err(storage_error)?;
            let intent_id: Option<String> = row.try_get("intent_id").map_err(storage_error)?;
            if intent_id.is_some()
                && verified_completed_merge(
                    &mut self.connection,
                    registration,
                    binding,
                    &operation_id,
                )
                .await?
                .is_some()
            {
                continue;
            }
            if intent_id.is_some()
                && verified_completed_choose_remote(
                    &mut self.connection,
                    registration,
                    binding,
                    &operation_id,
                )
                .await?
                .is_some()
            {
                continue;
            }
            if intent_id.is_some() {
                let intent =
                    read_intent(&mut self.connection, registration, binding, &operation_id)
                        .await?
                        .ok_or(StoreError::UnsupportedFormat)?;
                verify_history_extent(
                    &mut self.connection,
                    registration,
                    &operation_id,
                    intent.version,
                )
                .await?;
                if let Some(merged) = merged_operation_on_connection(
                    &mut self.connection,
                    registration,
                    binding,
                    &intent,
                )
                .await?
                {
                    if merged.status == crate::OutboxStatus::Conflict
                        && merged.failure_kind.is_none()
                    {
                        continue;
                    }
                    let sequence: i64 = sqlx::query_scalar("SELECT operation_seq FROM client_outbox WHERE registration_id=? AND operation_id=?")
                        .bind(registration).bind(&operation_id).fetch_one(&mut self.connection).await.map_err(storage_error)?;
                    return Ok(Some(ContentConflictRef {
                        operation_id,
                        operation_seq: u64::try_from(sequence)
                            .map_err(|_| StoreError::UnsupportedFormat)?,
                    }));
                }
            }
            let snapshot = self
                .inspect_content_conflict(binding, document_id, &operation_id)
                .await?;
            return Ok(Some(ContentConflictRef {
                operation_id: snapshot.operation_id,
                operation_seq: snapshot.operation_seq,
            }));
        }
        Ok(None)
    }

    pub async fn inspect_content_conflict(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        operation_id: &str,
    ) -> Result<ContentConflictSnapshot, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        inspect_content_conflict_on_connection(
            &mut self.connection,
            registration,
            binding,
            document_id,
            operation_id,
        )
        .await
    }
}

async fn inspect_content_conflict_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
) -> Result<ContentConflictSnapshot, StoreError> {
    document_id
        .parse::<NodeId>()
        .map_err(|_| StoreError::InvalidRequest)?;
    operation_id
        .parse::<OperationId>()
        .map_err(|_| StoreError::InvalidRequest)?;
    let row = sqlx::query(
            "SELECT o.operation_seq,o.document_id,o.operation_kind,o.generation AS operation_generation,
                    o.expected_revision,o.conflict_revision,o.conflict_etag,o.status,o.abandon_state,
                    o.terminal_kind,o.payload_retired,o.payload_len,o.payload_hash,
                    CASE WHEN length(o.bytes)<=5242880 THEN o.bytes END AS payload,
                    c.format_version,c.generation AS draft_generation,
                    CASE WHEN length(c.bytes)<=5242880 THEN c.bytes END AS local,
                    CASE WHEN length(b.bytes)<=5242880 THEN b.bytes END AS base,
                    b.revision AS base_revision,s.ack_through,s.next_seq
             FROM client_outbox o
             JOIN registered_documents d ON d.registration_id=o.registration_id
                 AND d.document_id=o.document_id AND d.storage_id=o.storage_id
             JOIN draft_checkpoints c ON c.document_id=d.storage_id
             JOIN draft_bases b ON b.document_id=d.storage_id
             JOIN outbox_streams s ON s.registration_id=o.registration_id
             WHERE o.registration_id=? AND o.operation_id=?",
        )
        .bind(registration)
        .bind(operation_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::InvalidRequest)?;
    let stored_document: String = row.try_get("document_id").map_err(storage_error)?;
    let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
    let status: String = row.try_get("status").map_err(storage_error)?;
    let abandon: String = row.try_get("abandon_state").map_err(storage_error)?;
    let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
    let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
    let seq: i64 = row.try_get("operation_seq").map_err(storage_error)?;
    let op_generation: i64 = row.try_get("operation_generation").map_err(storage_error)?;
    let generation: i64 = row.try_get("draft_generation").map_err(storage_error)?;
    let format: i64 = row.try_get("format_version").map_err(storage_error)?;
    let ack: i64 = row.try_get("ack_through").map_err(storage_error)?;
    let next_seq: i64 = row.try_get("next_seq").map_err(storage_error)?;
    if stored_document != document_id
        || kind != "content"
        || status != "conflict"
        || abandon != "none"
        || terminal.is_some()
        || retired != 0
        || seq <= 0
        || ack < seq
        || ack >= next_seq
        || op_generation <= 0
        || generation < op_generation
        || format != 1
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let payload: Option<Vec<u8>> = row.try_get("payload").map_err(storage_error)?;
    let local: Option<Vec<u8>> = row.try_get("local").map_err(storage_error)?;
    let base: Option<Vec<u8>> = row.try_get("base").map_err(storage_error)?;
    let payload = payload.ok_or(StoreError::UnsupportedFormat)?;
    let local = local.ok_or(StoreError::UnsupportedFormat)?;
    let base = base.ok_or(StoreError::UnsupportedFormat)?;
    let payload_len: i64 = row.try_get("payload_len").map_err(storage_error)?;
    let payload_hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    let digest: String = Sha256::digest(&payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if payload_len != payload.len() as i64
        || payload_hash != format!("sha256:{digest}")
        || (generation == op_generation && local != payload)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let expected: Option<String> = row.try_get("expected_revision").map_err(storage_error)?;
    let conflict: Option<String> = row.try_get("conflict_revision").map_err(storage_error)?;
    let etag: Option<String> = row.try_get("conflict_etag").map_err(storage_error)?;
    let base_revision: String = row.try_get("base_revision").map_err(storage_error)?;
    let expected_revision = expected.ok_or(StoreError::UnsupportedFormat)?;
    let conflict_revision = conflict.ok_or(StoreError::UnsupportedFormat)?;
    let conflict_etag = etag.ok_or(StoreError::UnsupportedFormat)?;
    let expected =
        ContentRevision::new(&expected_revision).map_err(|_| StoreError::UnsupportedFormat)?;
    StrongEtag::from_content_revision(&expected).map_err(|_| StoreError::UnsupportedFormat)?;
    let remote = ContentPrecondition::parse(&conflict_etag)
        .and_then(|value| value.revision())
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if expected_revision != base_revision
        || remote.as_str() != conflict_revision
        || expected_revision == conflict_revision
    {
        return Err(StoreError::UnsupportedFormat);
    }
    crate::validate_document(document_id, &local, &base_revision)
        .map_err(|_| StoreError::UnsupportedFormat)?;
    crate::validate_document(document_id, &base, &base_revision)
        .map_err(|_| StoreError::UnsupportedFormat)?;
    Ok(ContentConflictSnapshot {
        binding: binding.clone(),
        document_id: document_id.to_owned(),
        operation_id: operation_id.to_owned(),
        operation_seq: seq as u64,
        expected_revision,
        conflict_revision,
        conflict_etag,
        payload,
        generation: generation as u64,
        local,
        base,
        base_revision,
    })
}

fn digest(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

fn validate_text(bytes: &[u8]) -> Result<(), StoreError> {
    if bytes.len() > 5 * 1024 * 1024 || std::str::from_utf8(bytes).is_err() {
        return Err(StoreError::InvalidRequest);
    }
    Ok(())
}

async fn verified_completion_evidence(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<Option<(ContentConflictIntent, i64, i64)>, StoreError> {
    let intent = read_intent(connection, registration, binding, operation_id)
        .await?
        .ok_or(StoreError::UnsupportedFormat)?;
    verify_history_extent(connection, registration, operation_id, intent.version).await?;
    let (completed, through): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT completed_generation,completed_through FROM content_conflict_intents WHERE registration_id=? AND operation_id=?",
    ).bind(registration).bind(operation_id).fetch_one(&mut *connection).await.map_err(storage_error)?;
    let (generation, through) = match (completed, through) {
        (None, None) => return Ok(None),
        (Some(generation), Some(through)) => (generation, through),
        _ => return Err(StoreError::UnsupportedFormat),
    };
    if intent.choice != ContentConflictChoice::ChooseRemote
        || intent
            .generation
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            != Some(generation)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let (original, ack, confirmed, next): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT o.operation_seq,s.ack_through,s.confirmed_through,s.next_seq FROM client_outbox o JOIN outbox_streams s ON s.registration_id=o.registration_id WHERE o.registration_id=? AND o.operation_id=?",
    ).bind(registration).bind(operation_id).fetch_one(&mut *connection).await.map_err(storage_error)?;
    if original <= 0 || through < original || confirmed < through || confirmed > ack || ack >= next
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let mut after = original;
    let mut previous = operation_id.to_owned();
    while after < through {
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' AND operation_seq>? AND operation_seq<=? ORDER BY operation_seq LIMIT 1")
            .bind(registration).bind(&intent.document_id).bind(after).bind(through)
            .fetch_optional(&mut *connection).await.map_err(storage_error)?.ok_or(StoreError::UnsupportedFormat)?;
        let item = crate::outbox::operation(&row, binding, false)?;
        let seq = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        let dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
        if seq <= after
            || dependency.as_deref() != Some(previous.as_str())
            || item.status != crate::OutboxStatus::Abandoned
            || item.generation > intent.generation
            || (item.generation == intent.generation && item.bytes != intent.local_bytes)
        {
            return Err(StoreError::UnsupportedFormat);
        }
        after = seq;
        previous = item.operation_id;
    }
    let omitted: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' AND operation_seq>? AND generation<=?)")
        .bind(registration).bind(&intent.document_id).bind(through).bind(intent.generation as i64)
        .fetch_one(&mut *connection).await.map_err(storage_error)?;
    if omitted != 0 {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(Some((intent, generation, through)))
}

pub(crate) async fn verified_completed_choose_remote(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<Option<(ContentConflictChooseRemoteResult, u64)>, StoreError> {
    let Some((intent, generation, through)) =
        verified_completion_evidence(connection, registration, binding, operation_id).await?
    else {
        return Ok(None);
    };
    let result = ContentConflictChooseRemoteResult {
        binding: binding.clone(),
        document_id: intent.document_id.clone(),
        operation_id: operation_id.to_owned(),
        generation: generation as u64,
        remote_revision: intent.remote_revision.clone(),
        remote_bytes: intent.remote_bytes.clone(),
    };
    let (ack,): (i64,) =
        sqlx::query_as("SELECT ack_through FROM outbox_streams WHERE registration_id=?")
            .bind(registration)
            .fetch_one(&mut *connection)
            .await
            .map_err(storage_error)?;
    verify_durable_base_chain(
        connection,
        registration,
        binding,
        &intent.document_id,
        generation,
        through,
        &intent.remote_revision,
        &intent.remote_hash,
        intent.remote_bytes.len() as i64,
        ack,
    )
    .await?;
    Ok(Some((result, through as u64)))
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn verify_durable_base_chain(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document_id: &str,
    initial_generation: i64,
    initial_through: i64,
    initial_revision: &str,
    initial_hash: &str,
    initial_length: i64,
    ack: i64,
) -> Result<(), StoreError> {
    let draft = sqlx::query("SELECT d.storage_id,c.generation,c.format_version,
        CASE WHEN length(c.bytes)<=5242880 THEN c.bytes END AS local,
        CASE WHEN length(b.bytes)<=5242880 THEN b.bytes END AS base,b.revision
        FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id
        JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.registration_id=? AND d.document_id=?")
        .bind(registration).bind(document_id).fetch_optional(&mut *connection).await.map_err(storage_error)?
        .ok_or(StoreError::UnsupportedFormat)?;
    let draft_generation: i64 = draft.try_get("generation").map_err(storage_error)?;
    let format: i64 = draft.try_get("format_version").map_err(storage_error)?;
    let local: Vec<u8> = draft
        .try_get::<Option<Vec<u8>>, _>("local")
        .map_err(storage_error)?
        .ok_or(StoreError::UnsupportedFormat)?;
    let base: Vec<u8> = draft
        .try_get::<Option<Vec<u8>>, _>("base")
        .map_err(storage_error)?
        .ok_or(StoreError::UnsupportedFormat)?;
    let base_revision: String = draft.try_get("revision").map_err(storage_error)?;
    let storage: String = draft.try_get("storage_id").map_err(storage_error)?;
    if format != 1 || draft_generation < initial_generation {
        return Err(StoreError::UnsupportedFormat);
    }
    crate::validate_document(document_id, &local, &base_revision)
        .map_err(|_| StoreError::UnsupportedFormat)?;
    crate::validate_document(document_id, &base, &base_revision)
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let mut cursor = initial_through;
    let mut state_generation = initial_generation;
    let mut state_revision = initial_revision.to_owned();
    let mut state_hash = initial_hash.to_owned();
    let mut state_length = initial_length;
    loop {
        let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' AND operation_seq>? ORDER BY operation_seq LIMIT 1")
            .bind(registration).bind(document_id).bind(cursor).fetch_optional(&mut *connection).await.map_err(storage_error)?;
        let Some(row) = row else {
            break;
        };
        let seq: i64 = row.try_get("operation_seq").map_err(storage_error)?;
        let status: String = row.try_get("status").map_err(storage_error)?;
        let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
        let id: String = row.try_get("operation_id").map_err(storage_error)?;
        if seq <= cursor {
            return Err(StoreError::UnsupportedFormat);
        }
        if status == "conflict" && terminal.is_none() {
            let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM content_conflict_intents WHERE registration_id=? AND operation_id=?)")
                .bind(registration).bind(&id).fetch_one(&mut *connection).await.map_err(storage_error)?;
            if exists == 0 {
                break;
            }
            let later_intent = read_intent(&mut *connection, registration, binding, &id)
                .await?
                .ok_or(StoreError::UnsupportedFormat)?;
            verify_history_extent(&mut *connection, registration, &id, later_intent.version)
                .await?;
            if let Some(merge) = merged_operation_on_connection(
                &mut *connection,
                registration,
                binding,
                &later_intent,
            )
            .await?
            {
                let original_generation: i64 = row.try_get("generation").map_err(storage_error)?;
                let through: i64 = sqlx::query_scalar("SELECT abandoned_through FROM content_conflict_merged_operations WHERE registration_id=? AND operation_id=?")
                    .bind(registration).bind(&id).fetch_one(&mut *connection).await.map_err(storage_error)?;
                let merge_seq = i64::try_from(merge.operation_seq)
                    .map_err(|_| StoreError::UnsupportedFormat)?;
                if later_intent.document_id != document_id
                    || original_generation <= state_generation
                    || later_intent.generation < state_generation as u64
                    || later_intent.base_revision != state_revision
                    || later_intent.base_bytes.len() as i64 != state_length
                    || digest(&later_intent.base_bytes) != state_hash
                    || through < seq
                    || through >= merge_seq
                    || through <= cursor
                    || i64::try_from(merge.generation).map_err(|_| StoreError::UnsupportedFormat)?
                        > draft_generation
                {
                    return Err(StoreError::UnsupportedFormat);
                }
                match merge.status {
                    crate::OutboxStatus::Queued
                    | crate::OutboxStatus::Sent
                    | crate::OutboxStatus::Abandoning
                        if ack >= merge_seq =>
                    {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    crate::OutboxStatus::Committed
                    | crate::OutboxStatus::Conflict
                    | crate::OutboxStatus::Failed
                    | crate::OutboxStatus::Abandoned
                        if ack < merge_seq =>
                    {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    _ => {}
                }
                state_generation = i64::try_from(later_intent.generation)
                    .map_err(|_| StoreError::UnsupportedFormat)?;
                state_revision = later_intent.remote_revision;
                state_hash = later_intent.remote_hash;
                state_length = later_intent.remote_bytes.len() as i64;
                cursor = through;
                continue;
            }
            let Some((later, later_generation, later_through)) =
                verified_completion_evidence(connection, registration, binding, &id).await?
            else {
                break;
            };
            if later.document_id != document_id
                || later_generation <= state_generation
                || later.generation < state_generation as u64
                || later.base_revision != state_revision
                || later.base_bytes.len() as i64 != state_length
                || digest(&later.base_bytes) != state_hash
                || later_through < seq
                || later_generation > draft_generation
            {
                return Err(StoreError::UnsupportedFormat);
            }
            state_generation = later_generation;
            state_revision = later.remote_revision;
            state_hash = later.remote_hash;
            state_length = later.remote_bytes.len() as i64;
            cursor = later_through;
            continue;
        }
        if status == "committed" && terminal.as_deref() == Some("abandoned") {
            let item = crate::outbox::operation(&row, binding, false)?;
            if item.status != crate::OutboxStatus::Abandoned || seq > ack {
                return Err(StoreError::UnsupportedFormat);
            }
            cursor = seq;
            continue;
        }
        if status != "committed" || terminal.is_some() {
            break;
        }
        let item_generation: i64 = row.try_get("generation").map_err(storage_error)?;
        let expected: Option<String> = row.try_get("expected_revision").map_err(storage_error)?;
        let revision: Option<String> = row.try_get("committed_revision").map_err(storage_error)?;
        let hash: String = row.try_get("payload_hash").map_err(storage_error)?;
        let length: i64 = row.try_get("payload_len").map_err(storage_error)?;
        let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
        let stored: String = row.try_get("storage_id").map_err(storage_error)?;
        let revision = revision.ok_or(StoreError::UnsupportedFormat)?;
        let parsed = ContentRevision::new(&revision).map_err(|_| StoreError::UnsupportedFormat)?;
        StrongEtag::from_content_revision(&parsed).map_err(|_| StoreError::UnsupportedFormat)?;
        let valid_hash = hash.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if seq > ack
            || item_generation <= state_generation
            || item_generation > draft_generation
            || expected.as_deref() != Some(state_revision.as_str())
            || stored != storage
            || !(0..=5242880).contains(&length)
            || !valid_hash
            || !matches!(retired, 0 | 1)
        {
            return Err(StoreError::UnsupportedFormat);
        }
        if retired == 0 {
            let item = crate::outbox::operation(&row, binding, false)?;
            if item.status != crate::OutboxStatus::Committed {
                return Err(StoreError::UnsupportedFormat);
            }
        } else {
            let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
            if !bytes.is_empty() {
                return Err(StoreError::UnsupportedFormat);
            }
        }
        state_generation = item_generation;
        state_revision = revision;
        state_hash = hash;
        state_length = length;
        cursor = seq;
    }
    if base_revision != state_revision
        || base.len() as i64 != state_length
        || digest(&base) != state_hash
        || draft_generation < state_generation
        || (draft_generation == state_generation && local != base)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(())
}

async fn read_history_entry(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
    sequence: u64,
) -> Result<Option<ContentConflictIntent>, StoreError> {
    let sequence = i64::try_from(sequence).map_err(|_| StoreError::UnsupportedFormat)?;
    let row = sqlx::query("SELECT i.history_seq,i.history_seq-1 AS intent_version,i.document_id,i.generation,i.base_revision,
        CASE WHEN length(i.base_bytes)<=5242880 THEN i.base_bytes END AS base_bytes,
        CASE WHEN length(i.local_bytes)<=5242880 THEN i.local_bytes END AS local_bytes,
        i.remote_revision,i.remote_hash,CASE WHEN length(i.remote_bytes)<=5242880 THEN i.remote_bytes END AS remote_bytes,
        i.choice,i.merged_hash,length(i.merged_bytes) AS merged_length,CASE WHEN length(i.merged_bytes)<=5242880 THEN i.merged_bytes END AS merged_bytes,
        o.document_id AS operation_document,o.operation_kind,o.status,o.abandon_state,o.terminal_kind,
        o.generation AS operation_generation,o.expected_revision,o.conflict_revision,o.conflict_etag,
        o.payload_retired,o.payload_len,o.payload_hash,CASE WHEN length(o.bytes)<=5242880 THEN o.bytes END AS payload,
        o.operation_seq,s.ack_through,s.next_seq,s.confirmed_through
        FROM content_conflict_intent_history i LEFT JOIN client_outbox o ON o.operation_id=i.operation_id AND o.registration_id=i.registration_id
        LEFT JOIN outbox_streams s ON s.registration_id=i.registration_id
        WHERE i.registration_id=? AND i.operation_id=? AND i.history_seq=?")
        .bind(registration).bind(operation_id).bind(sequence).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    row.as_ref()
        .map(|row| decode_intent_row(row, binding, operation_id))
        .transpose()
}

async fn verify_history_extent(
    connection: &mut SqliteConnection,
    registration: i64,
    operation_id: &str,
    version: u64,
) -> Result<(), StoreError> {
    let (count, min, max): (i64, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT count(*),min(history_seq),max(history_seq) FROM content_conflict_intent_history WHERE registration_id=? AND operation_id=?",
    ).bind(registration).bind(operation_id).fetch_one(&mut *connection).await.map_err(storage_error)?;
    let version = i64::try_from(version).map_err(|_| StoreError::UnsupportedFormat)?;
    if count != version
        || (version == 0 && (min.is_some() || max.is_some()))
        || (version > 0 && (min != Some(1) || max != Some(version)))
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(())
}

async fn read_intent(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<Option<ContentConflictIntent>, StoreError> {
    let row = sqlx::query("SELECT i.intent_version,i.document_id,i.generation,i.base_revision,
        CASE WHEN length(i.base_bytes)<=5242880 THEN i.base_bytes END AS base_bytes,
        CASE WHEN length(i.local_bytes)<=5242880 THEN i.local_bytes END AS local_bytes,
        i.remote_revision,i.remote_hash,CASE WHEN length(i.remote_bytes)<=5242880 THEN i.remote_bytes END AS remote_bytes,
        i.choice,i.merged_hash,length(i.merged_bytes) AS merged_length,CASE WHEN length(i.merged_bytes)<=5242880 THEN i.merged_bytes END AS merged_bytes,
        o.document_id AS operation_document,o.operation_kind,o.status,o.abandon_state,o.terminal_kind,
        o.generation AS operation_generation,o.expected_revision,o.conflict_revision,o.conflict_etag,
        o.payload_retired,o.payload_len,o.payload_hash,CASE WHEN length(o.bytes)<=5242880 THEN o.bytes END AS payload,
        o.operation_seq,s.ack_through,s.next_seq,s.confirmed_through
        FROM content_conflict_intents i LEFT JOIN client_outbox o ON o.operation_id=i.operation_id AND o.registration_id=i.registration_id
        LEFT JOIN outbox_streams s ON s.registration_id=i.registration_id
        WHERE i.registration_id=? AND i.operation_id=?")
        .bind(registration).bind(operation_id).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    decode_intent_row(&row, binding, operation_id).map(Some)
}

fn decode_intent_row(
    row: &sqlx::sqlite::SqliteRow,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<ContentConflictIntent, StoreError> {
    let version: i64 = row.try_get("intent_version").map_err(storage_error)?;
    let version = u64::try_from(version).map_err(|_| StoreError::UnsupportedFormat)?;
    let document_id: String = row.try_get("document_id").map_err(storage_error)?;
    document_id
        .parse::<NodeId>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let generation: i64 = row.try_get("generation").map_err(storage_error)?;
    let op_generation: i64 = row
        .try_get("operation_generation")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let seq: i64 = row
        .try_get("operation_seq")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let ack: i64 = row
        .try_get("ack_through")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let next: i64 = row
        .try_get("next_seq")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let confirmed: i64 = row
        .try_get("confirmed_through")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let op_document: String = row
        .try_get("operation_document")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let kind: String = row
        .try_get("operation_kind")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let status: String = row
        .try_get("status")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let abandon: String = row
        .try_get("abandon_state")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let terminal: Option<String> = row.try_get("terminal_kind").map_err(storage_error)?;
    let retired: i64 = row
        .try_get("payload_retired")
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if op_document != document_id
        || kind != "content"
        || status != "conflict"
        || abandon != "none"
        || terminal.is_some()
        || retired != 0
        || op_generation <= 0
        || generation < op_generation
        || seq <= 0
        || ack < seq
        || ack >= next
        || confirmed < 0
        || confirmed > ack
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let get_bytes = |name| -> Result<Vec<u8>, StoreError> {
        row.try_get::<Option<Vec<u8>>, _>(name)
            .map_err(storage_error)?
            .ok_or(StoreError::UnsupportedFormat)
    };
    let base_bytes = get_bytes("base_bytes")?;
    let local_bytes = get_bytes("local_bytes")?;
    let remote_bytes = get_bytes("remote_bytes")?;
    let payload = get_bytes("payload")?;
    for bytes in [&base_bytes, &local_bytes, &remote_bytes, &payload] {
        validate_text(bytes).map_err(|_| StoreError::UnsupportedFormat)?;
    }
    let payload_len: i64 = row.try_get("payload_len").map_err(storage_error)?;
    let payload_hash: String = row.try_get("payload_hash").map_err(storage_error)?;
    if payload_len != payload.len() as i64
        || payload_hash != digest(&payload)
        || (generation == op_generation && payload != local_bytes)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let base_revision: String = row.try_get("base_revision").map_err(storage_error)?;
    let expected: Option<String> = row.try_get("expected_revision").map_err(storage_error)?;
    let conflict: Option<String> = row.try_get("conflict_revision").map_err(storage_error)?;
    let etag: Option<String> = row.try_get("conflict_etag").map_err(storage_error)?;
    let expected_rev =
        ContentRevision::new(&base_revision).map_err(|_| StoreError::UnsupportedFormat)?;
    StrongEtag::from_content_revision(&expected_rev).map_err(|_| StoreError::UnsupportedFormat)?;
    let conflict_rev =
        ContentPrecondition::parse(etag.as_deref().ok_or(StoreError::UnsupportedFormat)?)
            .and_then(|value| value.revision())
            .map_err(|_| StoreError::UnsupportedFormat)?;
    if expected.as_deref() != Some(base_revision.as_str())
        || conflict.as_deref() != Some(conflict_rev.as_str())
        || conflict_rev.as_str() == base_revision
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let remote_revision: String = row.try_get("remote_revision").map_err(storage_error)?;
    let remote_rev =
        ContentRevision::new(&remote_revision).map_err(|_| StoreError::UnsupportedFormat)?;
    StrongEtag::from_content_revision(&remote_rev).map_err(|_| StoreError::UnsupportedFormat)?;
    let remote_hash: String = row.try_get("remote_hash").map_err(storage_error)?;
    if digest(&remote_bytes) != remote_hash {
        return Err(StoreError::UnsupportedFormat);
    }
    let choice: String = row.try_get("choice").map_err(storage_error)?;
    let merged_length: Option<i64> = row.try_get("merged_length").map_err(storage_error)?;
    if merged_length.is_some_and(|length| !(0..=5242880).contains(&length)) {
        return Err(StoreError::UnsupportedFormat);
    }
    let merged: Option<Vec<u8>> = row.try_get("merged_bytes").map_err(storage_error)?;
    let merged_hash: Option<String> = row.try_get("merged_hash").map_err(storage_error)?;
    let choice = match (choice.as_str(), merged, &merged_hash) {
        ("keep_local_copy", None, None) => ContentConflictChoice::KeepLocalCopy,
        ("choose_remote", None, None) => ContentConflictChoice::ChooseRemote,
        ("save_merged", Some(bytes), hash) => {
            validate_text(&bytes).map_err(|_| StoreError::UnsupportedFormat)?;
            if hash.as_ref().is_some_and(|hash| *hash != digest(&bytes)) {
                return Err(StoreError::UnsupportedFormat);
            }
            ContentConflictChoice::SaveMerged { bytes }
        }
        _ => return Err(StoreError::UnsupportedFormat),
    };
    Ok(ContentConflictIntent {
        binding: binding.clone(),
        document_id,
        operation_id: operation_id.to_owned(),
        version,
        generation: generation as u64,
        base_revision,
        base_bytes,
        local_bytes,
        remote_revision,
        remote_hash,
        remote_bytes,
        choice,
        merged_hash,
    })
}
