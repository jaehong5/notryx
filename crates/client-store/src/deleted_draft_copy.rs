use crate::{ClientStore, RegistrationBinding, StoreError, storage_error};
use notryx_domain::{ChangeSeq, NodeId, OperationId, mutation::normalized_name_key};
use notryx_protocol::NodeKind;
use sqlx::{Connection, Row, SqliteConnection};

pub(crate) const UPGRADE_V21: &str = "
    ALTER TABLE content_conflict_intents ADD COLUMN merged_hash TEXT;
    ALTER TABLE content_conflict_intent_history ADD COLUMN merged_hash TEXT;
    CREATE TABLE deleted_draft_copy_intents (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        document_id TEXT NOT NULL,
        generation INTEGER NOT NULL CHECK(generation > 0),
        local_bytes BLOB NOT NULL CHECK(length(local_bytes) <= 5242880),
        local_hash TEXT NOT NULL,
        parent_id TEXT NOT NULL,
        name TEXT NOT NULL,
        node_id TEXT PRIMARY KEY NOT NULL,
        create_operation_id TEXT NOT NULL UNIQUE,
        content_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        evidence_kind TEXT NOT NULL CHECK(evidence_kind IN ('tombstone', 'node_deleted')),
        tombstone_seq TEXT,
        terminal_operation_id TEXT REFERENCES client_outbox(operation_id),
        CHECK ((evidence_kind = 'tombstone' AND tombstone_seq IS NOT NULL AND terminal_operation_id IS NULL)
            OR (evidence_kind = 'node_deleted' AND tombstone_seq IS NULL AND terminal_operation_id IS NOT NULL)),
        UNIQUE(registration_id, document_id)
    );
    CREATE TABLE deleted_draft_copy_attempts (
        old_create_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        registration_id INTEGER NOT NULL,
        document_id TEXT NOT NULL,
        old_parent_id TEXT NOT NULL,
        old_name TEXT NOT NULL,
        old_node_id TEXT NOT NULL,
        new_parent_id TEXT NOT NULL,
        new_name TEXT NOT NULL,
        new_node_id TEXT NOT NULL,
        new_create_operation_id TEXT NOT NULL UNIQUE,
        FOREIGN KEY(registration_id, document_id) REFERENCES deleted_draft_copy_intents(registration_id, document_id)
    );
    CREATE TABLE content_conflict_merged_operations (
        operation_id TEXT PRIMARY KEY NOT NULL REFERENCES content_conflict_intents(operation_id),
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        intent_version INTEGER NOT NULL CHECK(intent_version >= 0),
        content_operation_id TEXT NOT NULL UNIQUE REFERENCES client_outbox(operation_id),
        abandoned_through INTEGER NOT NULL CHECK(abandoned_through > 0)
    );
    PRAGMA user_version = 21;
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletedDraftCopyEvidence {
    Tombstone { last_change_seq: ChangeSeq },
    NodeDeleted { operation_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletedDraftCopyCreateState {
    NotQueued,
    Pending,
    Created,
    NeedsReview { destination_exists: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedDraftCopyStatus {
    pub create: DeletedDraftCopyCreateState,
    pub content: crate::ContentConflictCopyContentState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedDraftCopyBlockingFollowup {
    pub blocker_operation_id: String,
    pub followup_operation_id: String,
    pub followup_operation_seq: u64,
    pub followup_generation: u64,
    pub followup_hash: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeletedDraftCopyRequest {
    pub edit_session_id: String,
    pub generation: u64,
    pub local_bytes: Vec<u8>,
    pub parent_id: String,
    pub name: String,
    pub evidence: DeletedDraftCopyEvidence,
}

impl std::fmt::Debug for DeletedDraftCopyRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeletedDraftCopyRequest")
            .field("edit_session_id", &self.edit_session_id)
            .field("generation", &self.generation)
            .field("local_bytes_len", &self.local_bytes.len())
            .field("parent_id", &self.parent_id)
            .field("name", &self.name)
            .field("evidence", &self.evidence)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeletedDraftCopyIntent {
    pub binding: RegistrationBinding,
    pub document_id: String,
    pub generation: u64,
    pub local_bytes: Vec<u8>,
    pub parent_id: String,
    pub name: String,
    pub node_id: String,
    pub create_operation_id: String,
    pub evidence: DeletedDraftCopyEvidence,
}

impl std::fmt::Debug for DeletedDraftCopyIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeletedDraftCopyIntent")
            .field("binding", &self.binding)
            .field("document_id", &self.document_id)
            .field("generation", &self.generation)
            .field("local_bytes_len", &self.local_bytes.len())
            .field("parent_id", &self.parent_id)
            .field("name", &self.name)
            .field("node_id", &self.node_id)
            .field("create_operation_id", &self.create_operation_id)
            .field("evidence", &self.evidence)
            .finish()
    }
}

impl ClientStore {
    pub async fn read_deleted_draft_copy_blocking_followup(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<Option<DeletedDraftCopyBlockingFollowup>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let candidate =
            blocking_followup_on_connection(&mut tx, registration, binding, document_id).await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(candidate.map(|(candidate, _)| candidate))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
    )]
    pub async fn request_deleted_draft_copy_followup_abandon(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        expected_blocker_operation_id: &str,
        expected_followup_operation_id: &str,
        expected_followup_operation_seq: u64,
        expected_followup_generation: u64,
        expected_followup_hash: &str,
    ) -> Result<crate::OutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        expected_blocker_operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        expected_followup_operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let terminal = verified_followup_on_connection(
            &mut tx,
            registration,
            binding,
            document_id,
            Some(expected_followup_operation_id),
        )
        .await?;
        let (candidate, item) = match terminal {
            Some(value) => value,
            None => blocking_followup_on_connection(&mut tx, registration, binding, document_id)
                .await?
                .ok_or(StoreError::InvalidRequest)?,
        };
        if candidate.blocker_operation_id != expected_blocker_operation_id
            || candidate.followup_operation_id != expected_followup_operation_id
            || candidate.followup_operation_seq != expected_followup_operation_seq
            || candidate.followup_generation != expected_followup_generation
            || candidate.followup_hash != expected_followup_hash
        {
            return Err(StoreError::InvalidRequest);
        }
        if item.abandon_state == crate::OutboxAbandonState::None {
            let row = sqlx::query("SELECT d.storage_id,c.generation,c.format_version,c.bytes,b.bytes AS base_bytes,b.revision FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.registration_id=? AND d.document_id=?")
                .bind(registration).bind(document_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::MissingDraft)?;
            let storage: String = row.try_get("storage_id").map_err(storage_error)?;
            let checkpoint = crate::decode_draft(&row, &storage)?;
            if checkpoint.generation != item.generation || checkpoint.bytes != item.bytes {
                return Err(StoreError::InvalidRequest);
            }
            let changed = sqlx::query("UPDATE client_outbox SET abandon_state='requested' WHERE registration_id=? AND operation_id=? AND status='queued' AND abandon_state='none'")
                .bind(registration).bind(&item.operation_id).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
            if changed != 1 {
                return Err(StoreError::OutboxBlocked);
            }
        }
        let row =
            sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration)
                .bind(&item.operation_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let result = crate::outbox::operation(&row, binding, false)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn read_deleted_draft_copy_status(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<DeletedDraftCopyStatus, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_intent(&mut tx, registration, binding, document_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let row = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(&copy.create_operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let create = if let Some(row) = row {
            let row_registration: i64 = row.try_get("registration_id").map_err(storage_error)?;
            let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
            if row_registration != registration || kind != "create" {
                return Err(StoreError::UnsupportedFormat);
            }
            let operation = crate::outbox::validated_create_record(&row, binding)?;
            if operation.request.node_id.to_string() != copy.node_id
                || operation.request.parent_id.to_string() != copy.parent_id
                || operation.request.name != copy.name
                || operation.request.kind != NodeKind::File
            {
                return Err(StoreError::UnsupportedFormat);
            }
            let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as("SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
            let seq = i64::try_from(operation.operation_seq)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            if confirmed < 0 || confirmed > ack || ack >= next || seq <= 0 || seq >= next {
                return Err(StoreError::UnsupportedFormat);
            }
            match operation.status {
                crate::OutboxStatus::Queued | crate::OutboxStatus::Sent => {
                    if ack >= seq {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    DeletedDraftCopyCreateState::Pending
                }
                crate::OutboxStatus::Abandoning => {
                    if ack >= seq {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    DeletedDraftCopyCreateState::NeedsReview {
                        destination_exists: false,
                    }
                }
                crate::OutboxStatus::Committed => {
                    if ack < seq {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    if confirmed < seq {
                        DeletedDraftCopyCreateState::Pending
                    } else {
                        DeletedDraftCopyCreateState::Created
                    }
                }
                crate::OutboxStatus::Failed | crate::OutboxStatus::Abandoned => {
                    if ack < seq {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    let raw: String = row.try_get("receipt_json").map_err(storage_error)?;
                    let receipt: notryx_protocol::OperationReceipt =
                        serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
                    if confirmed < seq {
                        DeletedDraftCopyCreateState::Pending
                    } else {
                        DeletedDraftCopyCreateState::NeedsReview {
                            destination_exists: matches!(
                                receipt,
                                notryx_protocol::OperationReceipt::DestinationExists { .. }
                            ),
                        }
                    }
                }
                crate::OutboxStatus::Conflict => return Err(StoreError::UnsupportedFormat),
            }
        } else {
            DeletedDraftCopyCreateState::NotQueued
        };
        let linked: Option<String> = sqlx::query_scalar("SELECT content_operation_id FROM deleted_draft_copy_intents WHERE registration_id=? AND document_id=?")
            .bind(registration).bind(document_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let content = crate::content_conflict::copy_content_state(
            &mut tx,
            registration,
            binding,
            &copy_details(&copy),
            linked,
        )
        .await?;
        if create != DeletedDraftCopyCreateState::Created
            && content != crate::ContentConflictCopyContentState::NotQueued
        {
            return Err(StoreError::UnsupportedFormat);
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(DeletedDraftCopyStatus { create, content })
    }

    pub async fn reconfirm_failed_deleted_draft_copy(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        expected_create_operation_id: &str,
        parent_id: &str,
        name: &str,
    ) -> Result<DeletedDraftCopyIntent, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        expected_create_operation_id
            .parse::<OperationId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let parent: NodeId = parent_id.parse().map_err(|_| StoreError::InvalidRequest)?;
        notryx_domain::mutation::validate_name(name).map_err(|_| StoreError::InvalidRequest)?;
        if !name.to_ascii_lowercase().ends_with(".md") {
            return Err(StoreError::InvalidRequest);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_intent(&mut tx, registration, binding, document_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let row = sqlx::query("SELECT registration_id,operation_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,expected_revision,bytes,payload_hash,payload_len,payload_retired,CASE WHEN status IN ('committed','conflict') THEN 'sent' ELSE status END AS status,abandon_state,NULL AS terminal_kind,status AS actual_status,terminal_kind AS actual_terminal,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(expected_create_operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
        let row_registration: i64 = row.try_get("registration_id").map_err(storage_error)?;
        let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
        if row_registration != registration || kind != "create" {
            return Err(StoreError::InvalidRequest);
        }
        let old = crate::outbox::validated_create_record(&row, binding)?;
        let raw: Option<String> = row.try_get("receipt_json").map_err(storage_error)?;
        let receipt: notryx_protocol::OperationReceipt =
            serde_json::from_str(raw.as_deref().ok_or(StoreError::InvalidRequest)?)
                .map_err(|_| StoreError::UnsupportedFormat)?;
        if old.status != crate::OutboxStatus::Failed
            || !matches!(
                receipt,
                notryx_protocol::OperationReceipt::DestinationExists { .. }
            )
        {
            return Err(StoreError::InvalidRequest);
        }
        let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as("SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let sequence =
            i64::try_from(old.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        if sequence <= 0 || confirmed < sequence || confirmed > ack || ack >= next {
            return Err(StoreError::InvalidRequest);
        }
        if copy.create_operation_id != expected_create_operation_id {
            let previous: Option<(String, String, String, String, String, String, String)> = sqlx::query_as("SELECT old_parent_id,old_name,old_node_id,new_parent_id,new_name,new_node_id,new_create_operation_id FROM deleted_draft_copy_attempts WHERE registration_id=? AND document_id=? AND old_create_operation_id=?")
                .bind(registration).bind(document_id).bind(expected_create_operation_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
            if copy.parent_id == parent_id
                && copy.name == name
                && old.request.kind == NodeKind::File
                && previous
                    == Some((
                        old.request.parent_id.to_string(),
                        old.request.name.clone(),
                        old.request.node_id.to_string(),
                        parent_id.to_owned(),
                        name.to_owned(),
                        copy.node_id.clone(),
                        copy.create_operation_id.clone(),
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
            || old.request.kind != NodeKind::File
            || (copy.parent_id == parent_id && copy.name == name)
        {
            return Err(StoreError::InvalidRequest);
        }
        let linked: Option<String> = sqlx::query_scalar("SELECT content_operation_id FROM deleted_draft_copy_intents WHERE registration_id=? AND document_id=?")
            .bind(registration).bind(document_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if linked.is_some() {
            return Err(StoreError::InvalidRequest);
        }
        let index = sqlx::query("SELECT root_id,cursor FROM remote_index WHERE registration_id=?")
            .bind(registration)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?
            .ok_or(StoreError::InvalidRequest)?;
        let root: String = index.try_get("root_id").map_err(storage_error)?;
        let cursor: String = index.try_get("cursor").map_err(storage_error)?;
        let cursor = ChangeSeq::try_from(cursor).map_err(|_| StoreError::UnsupportedFormat)?;
        let rows = sqlx::query(
            "SELECT node_id,state FROM remote_nodes WHERE registration_id=? ORDER BY node_id",
        )
        .bind(registration)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage_error)?;
        let nodes = crate::bootstrap::decode_rows(&rows)?;
        if nodes.iter().any(|node| node.last_change_seq > cursor)
            || crate::bootstrap::validate_tree(&root, &nodes).is_err()
        {
            return Err(StoreError::UnsupportedFormat);
        }
        if !nodes
            .iter()
            .any(|node| node.node_id == parent && !node.deleted && node.kind == NodeKind::Folder)
            || nodes.iter().any(|node| {
                !node.deleted
                    && node.parent_id == Some(parent)
                    && normalized_name_key(&node.name) == normalized_name_key(name)
            })
        {
            return Err(StoreError::InvalidRequest);
        }
        let node_id = uuid::Uuid::now_v7().to_string();
        let create_operation_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO deleted_draft_copy_attempts(old_create_operation_id,registration_id,document_id,old_parent_id,old_name,old_node_id,new_parent_id,new_name,new_node_id,new_create_operation_id) VALUES(?,?,?,?,?,?,?,?,?,?)")
            .bind(expected_create_operation_id).bind(registration).bind(document_id).bind(&copy.parent_id).bind(&copy.name).bind(&copy.node_id).bind(parent_id).bind(name).bind(&node_id).bind(&create_operation_id)
            .execute(&mut *tx).await.map_err(storage_error)?;
        let changed = sqlx::query("UPDATE deleted_draft_copy_intents SET parent_id=?,name=?,node_id=?,create_operation_id=? WHERE registration_id=? AND document_id=? AND create_operation_id=? AND content_operation_id IS NULL")
            .bind(parent_id).bind(name).bind(&node_id).bind(&create_operation_id).bind(registration).bind(document_id).bind(expected_create_operation_id).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidRequest);
        }
        let result = DeletedDraftCopyIntent {
            parent_id: parent_id.into(),
            name: name.into(),
            node_id,
            create_operation_id,
            ..copy
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn queue_deleted_draft_copy(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<crate::CreateOutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_intent(&mut tx, registration, binding, document_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let result = crate::content_conflict::queue_copy_create(
            &mut tx,
            registration,
            binding,
            &copy_details(&copy),
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(result)
    }

    pub async fn queue_deleted_draft_copy_content(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<crate::OutboxOperation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_intent(&mut tx, registration, binding, document_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let linked: Option<String> = sqlx::query_scalar("SELECT content_operation_id FROM deleted_draft_copy_intents WHERE registration_id=? AND document_id=?")
            .bind(registration).bind(document_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let status = crate::content_conflict::copy_content_state(
            &mut tx,
            registration,
            binding,
            &copy_details(&copy),
            linked.clone(),
        )
        .await?;
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
                    == (crate::ContentConflictCopyContentState::Saved {
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
        let hash = crate::outbox::payload_hash(&copy.local_bytes);
        sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,bytes,payload_hash,payload_len,status) VALUES(?,?,?,'content',?,?,1,?,?,?,?,'queued')")
            .bind(&id).bind(registration).bind(next).bind(&copy.node_id).bind(&storage).bind(&revision)
            .bind(&copy.local_bytes).bind(&hash).bind(copy.local_bytes.len() as i64).execute(&mut *tx).await.map_err(storage_error)?;
        let changed = sqlx::query("UPDATE deleted_draft_copy_intents SET content_operation_id=? WHERE document_id=? AND registration_id=? AND content_operation_id IS NULL")
            .bind(&id).bind(document_id).bind(registration).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
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

    pub async fn deleted_draft_copy_content_is_saved(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<bool, StoreError> {
        let registration = self.registration_id(binding, false).await?;
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let copy = read_intent(&mut tx, registration, binding, document_id)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let linked: Option<String> = sqlx::query_scalar("SELECT content_operation_id FROM deleted_draft_copy_intents WHERE registration_id=? AND document_id=?")
            .bind(registration).bind(document_id).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let state = crate::content_conflict::copy_content_state(
            &mut tx,
            registration,
            binding,
            &copy_details(&copy),
            linked,
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(matches!(
            state,
            crate::ContentConflictCopyContentState::Saved { .. }
        ))
    }

    pub async fn record_deleted_draft_copy(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        request: &DeletedDraftCopyRequest,
    ) -> Result<DeletedDraftCopyIntent, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let document: NodeId = document_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        let parent: NodeId = request
            .parent_id
            .parse()
            .map_err(|_| StoreError::InvalidRequest)?;
        notryx_domain::mutation::validate_name(&request.name)
            .map_err(|_| StoreError::InvalidRequest)?;
        if !request.name.to_ascii_lowercase().ends_with(".md") {
            return Err(StoreError::InvalidRequest);
        }
        let generation =
            i64::try_from(request.generation).map_err(|_| StoreError::InvalidGeneration)?;
        if generation <= 0 {
            return Err(StoreError::InvalidGeneration);
        }
        if request.local_bytes.len() > crate::MAX_DOCUMENT_BYTES
            || std::str::from_utf8(&request.local_bytes).is_err()
        {
            return Err(StoreError::InvalidRequest);
        }
        let storage_id = self
            .registered_storage_id(registration, document_id)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        let session = self
            .sessions
            .get(&storage_id)
            .ok_or(StoreError::StaleSession)?;
        if session.id != request.edit_session_id {
            return Err(StoreError::StaleSession);
        }
        let session_base = session.base.clone();
        let session_revision = session.base_revision.clone();
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT c.generation,c.format_version,c.bytes,b.bytes AS base_bytes,b.revision FROM draft_checkpoints c JOIN draft_bases b USING(document_id) WHERE c.document_id=?")
            .bind(&storage_id).fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::MissingDraft)?;
        let draft = crate::decode_draft(&row, &storage_id)?;
        if draft.base_bytes != session_base || draft.base_revision != session_revision {
            return Err(StoreError::StaleSession);
        }
        std::str::from_utf8(&draft.bytes).map_err(|_| StoreError::UnsupportedFormat)?;
        if let Some(existing) = read_intent(&mut tx, registration, binding, document_id).await? {
            if existing.generation != request.generation
                || existing.parent_id != request.parent_id
                || existing.name != request.name
                || existing.evidence != request.evidence
                || existing.local_bytes != request.local_bytes
                || draft.generation < existing.generation
                || (draft.generation == existing.generation && draft.bytes != existing.local_bytes)
            {
                return Err(StoreError::InvalidRequest);
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(existing);
        }
        let index = sqlx::query("SELECT root_id,cursor FROM remote_index WHERE registration_id=?")
            .bind(registration)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?
            .ok_or(StoreError::InvalidRequest)?;
        let root: String = index.try_get("root_id").map_err(storage_error)?;
        let cursor: String = index.try_get("cursor").map_err(storage_error)?;
        let cursor = ChangeSeq::try_from(cursor).map_err(|_| StoreError::UnsupportedFormat)?;
        let rows = sqlx::query(
            "SELECT node_id,state FROM remote_nodes WHERE registration_id=? ORDER BY node_id",
        )
        .bind(registration)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage_error)?;
        let nodes = crate::bootstrap::decode_rows(&rows)?;
        if nodes.iter().any(|node| node.last_change_seq > cursor)
            || crate::bootstrap::validate_tree(&root, &nodes).is_err()
        {
            return Err(StoreError::UnsupportedFormat);
        }
        let parent_node = nodes
            .iter()
            .find(|node| node.node_id == parent)
            .ok_or(StoreError::InvalidRequest)?;
        if parent_node.deleted || parent_node.kind != NodeKind::Folder {
            return Err(StoreError::InvalidRequest);
        }
        if nodes.iter().any(|node| {
            !node.deleted
                && node.parent_id == Some(parent)
                && normalized_name_key(&node.name) == normalized_name_key(&request.name)
        }) {
            return Err(StoreError::InvalidRequest);
        }
        let observation = crate::content_cache::observe_remote_file_on_connection(
            &mut tx,
            registration,
            document,
        )
        .await?;
        if !matches!(observation, crate::RemoteFileObservation::Deleted(_)) {
            return Err(StoreError::InvalidRequest);
        }
        validate_evidence(
            &mut tx,
            registration,
            binding,
            document,
            draft.generation,
            &request.evidence,
        )
        .await?;
        if draft.generation != request.generation {
            return Err(StoreError::StaleGeneration);
        }
        if draft.bytes != request.local_bytes {
            return Err(StoreError::InvalidRequest);
        }
        let node_id = uuid::Uuid::now_v7().to_string();
        let create_operation_id = uuid::Uuid::now_v7().to_string();
        let (kind, tombstone_seq, terminal_operation_id) = match &request.evidence {
            DeletedDraftCopyEvidence::Tombstone { last_change_seq } => {
                ("tombstone", Some(last_change_seq.get().to_string()), None)
            }
            DeletedDraftCopyEvidence::NodeDeleted { operation_id } => {
                ("node_deleted", None, Some(operation_id.as_str()))
            }
        };
        sqlx::query("INSERT INTO deleted_draft_copy_intents(registration_id,document_id,generation,local_bytes,local_hash,parent_id,name,node_id,create_operation_id,evidence_kind,tombstone_seq,terminal_operation_id) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(registration).bind(document_id).bind(generation).bind(&draft.bytes).bind(crate::outbox::payload_hash(&draft.bytes)).bind(&request.parent_id).bind(&request.name)
            .bind(&node_id).bind(&create_operation_id).bind(kind).bind(tombstone_seq).bind(terminal_operation_id)
            .execute(&mut *tx).await.map_err(storage_error)?;
        let intent = DeletedDraftCopyIntent {
            binding: binding.clone(),
            document_id: document_id.into(),
            generation: request.generation,
            local_bytes: draft.bytes,
            parent_id: request.parent_id.clone(),
            name: request.name.clone(),
            node_id,
            create_operation_id,
            evidence: request.evidence.clone(),
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(intent)
    }

    pub async fn read_deleted_draft_copy(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<Option<DeletedDraftCopyIntent>, StoreError> {
        document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::InvalidRequest)?;
        let registration = self.registration_id(binding, false).await?;
        read_intent(&mut self.connection, registration, binding, document_id).await
    }
}

pub(crate) async fn blocking_followup_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document_id: &str,
) -> Result<Option<(DeletedDraftCopyBlockingFollowup, crate::OutboxOperation)>, StoreError> {
    verified_followup_on_connection(connection, registration, binding, document_id, None).await
}

async fn verified_followup_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document_id: &str,
    terminal_id: Option<&str>,
) -> Result<Option<(DeletedDraftCopyBlockingFollowup, crate::OutboxOperation)>, StoreError> {
    let Some(copy) = read_intent(&mut *connection, registration, binding, document_id).await?
    else {
        return Ok(None);
    };
    let row = if let Some(id) = terminal_id {
        sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
            .bind(registration)
            .bind(id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?
    } else {
        sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND status IN ('queued','sent') ORDER BY operation_seq LIMIT 1")
            .bind(registration).fetch_optional(&mut *connection).await.map_err(storage_error)?
    };
    let Some(row) = row else {
        return Ok(None);
    };
    let kind: String = row.try_get("operation_kind").map_err(storage_error)?;
    if kind != "content" {
        return Ok(None);
    }
    let item = crate::outbox::operation(&row, binding, false)?;
    if item.document_id != document_id
        || item.expected_revision.is_some()
        || if terminal_id.is_some() {
            item.status != crate::OutboxStatus::Abandoned
        } else {
            !matches!(
                item.status,
                crate::OutboxStatus::Queued | crate::OutboxStatus::Abandoning
            )
        }
    {
        return Ok(None);
    }
    if crate::outbox::requires_reconciliation(&mut *connection, &binding.workspace_id, document_id)
        .await?
    {
        return Err(StoreError::ReconciliationRequired);
    }
    let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as(
        "SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?",
    )
    .bind(registration)
    .fetch_one(&mut *connection)
    .await
    .map_err(storage_error)?;
    let item_seq = i64::try_from(item.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
    if confirmed < 0
        || confirmed > ack
        || ack >= next
        || item_seq >= next
        || if terminal_id.is_some() {
            item_seq > confirmed
        } else {
            item_seq <= ack
        }
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let mut dependency: Option<String> = row.try_get("dependency").map_err(storage_error)?;
    let mut sequence = item.operation_seq;
    while let Some(id) = dependency {
        let previous_row =
            sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?")
                .bind(registration)
                .bind(&id)
                .fetch_optional(&mut *connection)
                .await
                .map_err(storage_error)?
                .ok_or(StoreError::UnsupportedFormat)?;
        let previous = crate::outbox::operation(&previous_row, binding, false)?;
        if previous.document_id != document_id || previous.operation_seq >= sequence {
            return Err(StoreError::UnsupportedFormat);
        }
        let prior_seq =
            i64::try_from(previous.operation_seq).map_err(|_| StoreError::UnsupportedFormat)?;
        if previous.status == crate::OutboxStatus::Failed
            && previous.failure_kind == Some(crate::OutboxFailureKind::NodeDeleted)
        {
            validate_evidence(
                &mut *connection,
                registration,
                binding,
                document_id
                    .parse()
                    .map_err(|_| StoreError::InvalidRequest)?,
                copy.generation,
                &DeletedDraftCopyEvidence::NodeDeleted {
                    operation_id: id.clone(),
                },
            )
            .await?;
            if confirmed < prior_seq {
                return Ok(None);
            }
            return Ok(Some((
                DeletedDraftCopyBlockingFollowup {
                    blocker_operation_id: id,
                    followup_operation_id: item.operation_id.clone(),
                    followup_operation_seq: item.operation_seq,
                    followup_generation: item.generation,
                    followup_hash: item.payload_hash.clone(),
                },
                item,
            )));
        }
        if previous.status != crate::OutboxStatus::Abandoned {
            return Ok(None);
        }
        if prior_seq > confirmed {
            return Ok(None);
        }
        sequence = previous.operation_seq;
        dependency = previous_row.try_get("dependency").map_err(storage_error)?;
    }
    Ok(None)
}

fn copy_details(copy: &DeletedDraftCopyIntent) -> crate::content_conflict::CopyDetails<'_> {
    crate::content_conflict::CopyDetails {
        node_id: &copy.node_id,
        parent_id: &copy.parent_id,
        name: &copy.name,
        local_bytes: &copy.local_bytes,
        create_operation_id: &copy.create_operation_id,
    }
}

async fn validate_evidence(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document: NodeId,
    draft_generation: u64,
    evidence: &DeletedDraftCopyEvidence,
) -> Result<(), StoreError> {
    match evidence {
        DeletedDraftCopyEvidence::Tombstone { last_change_seq } => {
            let observation = crate::content_cache::observe_remote_file_on_connection(
                connection,
                registration,
                document,
            )
            .await?;
            if !matches!(observation, crate::RemoteFileObservation::Deleted(node) if node.last_change_seq == *last_change_seq)
            {
                return Err(StoreError::InvalidRequest);
            }
        }
        DeletedDraftCopyEvidence::NodeDeleted { operation_id } => {
            operation_id
                .parse::<OperationId>()
                .map_err(|_| StoreError::InvalidRequest)?;
            let row = sqlx::query(
                "SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=?",
            )
            .bind(registration)
            .bind(operation_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?
            .ok_or(StoreError::InvalidRequest)?;
            let operation = crate::outbox::operation(&row, binding, false)?;
            if operation.document_id != document.to_string()
                || operation.status != crate::OutboxStatus::Failed
                || operation.failure_kind != Some(crate::OutboxFailureKind::NodeDeleted)
                || operation.generation > draft_generation
                || operation.conflict_revision.is_some()
                || row
                    .try_get::<Option<String>, _>("conflict_etag")
                    .map_err(storage_error)?
                    .is_some()
            {
                return Err(StoreError::InvalidRequest);
            }
            let revision = operation
                .expected_revision
                .as_deref()
                .ok_or(StoreError::UnsupportedFormat)?;
            let revision = notryx_domain::ContentRevision::new(revision)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            notryx_protocol::StrongEtag::from_content_revision(&revision)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            if let Some(raw) = row
                .try_get::<Option<String>, _>("receipt_json")
                .map_err(storage_error)?
            {
                if raw.len() > 65536 {
                    return Err(StoreError::UnsupportedFormat);
                }
                let receipt: notryx_protocol::OperationReceipt =
                    serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
                if !matches!(receipt, notryx_protocol::OperationReceipt::NodeDeleted { operation_id: id, operation_seq, workspace_epoch }
                    if id.to_string() == operation.operation_id && operation_seq.get() == operation.operation_seq && workspace_epoch.to_string() == binding.workspace_epoch)
                {
                    return Err(StoreError::UnsupportedFormat);
                }
            }
            let stream: Option<(i64,i64,i64)> = sqlx::query_as("SELECT next_seq,ack_through,confirmed_through FROM outbox_streams WHERE registration_id=?")
                .bind(registration).fetch_optional(&mut *connection).await.map_err(storage_error)?;
            let (next, ack, confirmed) = stream.ok_or(StoreError::UnsupportedFormat)?;
            if ack < 0
                || confirmed < 0
                || confirmed > ack
                || next <= ack
                || u64::try_from(ack)
                    .ok()
                    .is_none_or(|ack| ack < operation.operation_seq)
            {
                return Err(StoreError::UnsupportedFormat);
            }
        }
    }
    Ok(())
}

async fn read_intent(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    document_id: &str,
) -> Result<Option<DeletedDraftCopyIntent>, StoreError> {
    let row = sqlx::query("SELECT generation,length(local_bytes) AS local_len,CASE WHEN length(local_bytes)<=5242880 THEN local_bytes ELSE NULL END AS local_bytes,local_hash,parent_id,name,node_id,create_operation_id,evidence_kind,tombstone_seq,terminal_operation_id FROM deleted_draft_copy_intents WHERE registration_id=? AND document_id=?")
        .bind(registration).bind(document_id).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let generation: i64 = row.try_get("generation").map_err(storage_error)?;
    let length: i64 = row.try_get("local_len").map_err(storage_error)?;
    let bytes: Option<Vec<u8>> = row.try_get("local_bytes").map_err(storage_error)?;
    let bytes = bytes.ok_or(StoreError::UnsupportedFormat)?;
    let local_hash: String = row.try_get("local_hash").map_err(storage_error)?;
    if generation <= 0
        || length < 0
        || length as usize != bytes.len()
        || std::str::from_utf8(&bytes).is_err()
        || crate::outbox::payload_hash(&bytes) != local_hash
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let parent_id: String = row.try_get("parent_id").map_err(storage_error)?;
    let name: String = row.try_get("name").map_err(storage_error)?;
    let node_id: String = row.try_get("node_id").map_err(storage_error)?;
    let create_operation_id: String = row.try_get("create_operation_id").map_err(storage_error)?;
    parent_id
        .parse::<NodeId>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    node_id
        .parse::<NodeId>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    create_operation_id
        .parse::<OperationId>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if node_id == document_id
        || node_id == parent_id
        || notryx_domain::mutation::validate_name(&name).is_err()
        || !name.to_ascii_lowercase().ends_with(".md")
    {
        return Err(StoreError::UnsupportedFormat);
    }
    let kind: String = row.try_get("evidence_kind").map_err(storage_error)?;
    let seq: Option<String> = row.try_get("tombstone_seq").map_err(storage_error)?;
    let operation: Option<String> = row
        .try_get("terminal_operation_id")
        .map_err(storage_error)?;
    let evidence = match (kind.as_str(), seq, operation) {
        ("tombstone", Some(seq), None) => {
            let last_change_seq =
                ChangeSeq::try_from(seq).map_err(|_| StoreError::UnsupportedFormat)?;
            if last_change_seq.get() == 0 {
                return Err(StoreError::UnsupportedFormat);
            }
            DeletedDraftCopyEvidence::Tombstone { last_change_seq }
        }
        ("node_deleted", None, Some(operation_id)) => {
            operation_id
                .parse::<OperationId>()
                .map_err(|_| StoreError::UnsupportedFormat)?;
            DeletedDraftCopyEvidence::NodeDeleted { operation_id }
        }
        _ => return Err(StoreError::UnsupportedFormat),
    };
    if matches!(&evidence, DeletedDraftCopyEvidence::NodeDeleted { .. }) {
        let document = document_id
            .parse::<NodeId>()
            .map_err(|_| StoreError::UnsupportedFormat)?;
        validate_evidence(
            connection,
            registration,
            binding,
            document,
            generation as u64,
            &evidence,
        )
        .await
        .map_err(|_| StoreError::UnsupportedFormat)?;
    }
    Ok(Some(DeletedDraftCopyIntent {
        binding: binding.clone(),
        document_id: document_id.into(),
        generation: generation as u64,
        local_bytes: bytes,
        parent_id,
        name,
        node_id,
        create_operation_id,
        evidence,
    }))
}
