use crate::{
    ClientStore, OutboxCommit, OutboxOperation, OutboxStatus, RegistrationBinding, StoreError,
    outbox::{operation, payload_hash, requires_reconciliation},
    storage_error, validate_document,
};
use notryx_domain::NodeId;
use sqlx::{Connection, Row};

pub(crate) const UPGRADE_V25: &str = "
    CREATE TABLE diagram_drafts (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        node_id TEXT NOT NULL,
        generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>0),
        xml TEXT NOT NULL CHECK(length(CAST(xml AS BLOB)) BETWEEN 1 AND 5242880),
        base_xml TEXT NOT NULL CHECK(length(CAST(base_xml AS BLOB)) BETWEEN 1 AND 5242880),
        base_revision TEXT NOT NULL CHECK(length(base_revision) BETWEEN 1 AND 256),
        PRIMARY KEY(registration_id, node_id)
    );
    PRAGMA user_version=25;
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagramDraft {
    pub generation: u64,
    pub xml: String,
    pub base_xml: String,
    pub base_revision: String,
}

#[derive(Clone, Debug)]
pub struct DiagramDraftCheckpoint {
    pub expected_generation: u64,
    pub xml: String,
    pub base_xml: String,
    pub base_revision: String,
}

pub(crate) fn validate_node(node_id: &str) -> Result<(), StoreError> {
    let node = node_id
        .parse::<NodeId>()
        .map_err(|_| StoreError::InvalidDocument)?;
    if node.to_string() != node_id {
        return Err(StoreError::InvalidDocument);
    }
    Ok(())
}

pub(crate) fn decode(
    row: sqlx::sqlite::SqliteRow,
    node_id: &str,
) -> Result<DiagramDraft, StoreError> {
    let generation: i64 = row.try_get("generation").map_err(storage_error)?;
    let xml: String = row.try_get("xml").map_err(storage_error)?;
    let base_xml: String = row.try_get("base_xml").map_err(storage_error)?;
    let base_revision: String = row.try_get("base_revision").map_err(storage_error)?;
    if generation <= 0 || xml.is_empty() || base_xml.is_empty() {
        return Err(StoreError::UnsupportedFormat);
    }
    validate_document(node_id, xml.as_bytes(), &base_revision)
        .and_then(|()| validate_document(node_id, base_xml.as_bytes(), &base_revision))
        .map_err(|_| StoreError::UnsupportedFormat)?;
    Ok(DiagramDraft {
        generation: generation as u64,
        xml,
        base_xml,
        base_revision,
    })
}

impl ClientStore {
    pub async fn queue_diagram_png(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
        expected_generation: u64,
        png: &[u8],
    ) -> Result<OutboxOperation, StoreError> {
        if !png.starts_with(b"\x89PNG\r\n\x1a\n") || png.len() > MAX_DIAGRAM_BYTES {
            return Err(StoreError::InvalidRequest);
        }
        self.queue_diagram(binding, node_id, expected_generation, Some(png))
            .await
    }

    pub async fn read_diagram_operation_xml(
        &mut self,
        binding: &RegistrationBinding,
        operation_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let registration_id = self.registration_id(binding, false).await?;
        let row = sqlx::query("SELECT CASE WHEN s.operation_id IS NOT NULL THEN s.xml WHEN substr(x.bytes,1,8)=X'89504e470d0a1a0a' THEN NULL ELSE CAST(x.bytes AS TEXT) END AS xml,x.payload_retired FROM client_outbox x LEFT JOIN diagram_save_snapshots s USING(operation_id) WHERE x.registration_id=? AND x.operation_id=? AND x.operation_kind='content'")
            .bind(registration_id).bind(operation_id).fetch_optional(&mut self.connection)
            .await.map_err(storage_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let retired: i64 = row.try_get("payload_retired").map_err(storage_error)?;
        if retired != 0 {
            return Err(StoreError::PayloadRetired);
        }
        let xml: Option<String> = row.try_get("xml").map_err(storage_error)?;
        let xml = xml
            .filter(|xml| !xml.is_empty() && xml.len() <= crate::MAX_DOCUMENT_BYTES)
            .ok_or(StoreError::UnsupportedFormat)?;
        Ok(Some(xml))
    }

    pub async fn queue_diagram_checkpoint(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
        expected_generation: u64,
    ) -> Result<OutboxOperation, StoreError> {
        self.queue_diagram(binding, node_id, expected_generation, None)
            .await
    }

    async fn queue_diagram(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
        expected_generation: u64,
        png: Option<&[u8]>,
    ) -> Result<OutboxOperation, StoreError> {
        validate_node(node_id)?;
        let registration_id = self.registration_id(binding, true).await?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT generation,xml,base_xml,base_revision FROM diagram_drafts WHERE registration_id=? AND node_id=?")
            .bind(registration_id).bind(node_id).fetch_optional(&mut *transaction)
            .await.map_err(storage_error)?.ok_or(StoreError::MissingDraft)?;
        let draft = decode(row, node_id)?;
        if draft.generation != expected_generation {
            return Err(StoreError::GenerationMismatch);
        }
        notryx_protocol::ContentPrecondition::parse(&format!("\"{}\"", draft.base_revision))
            .and_then(|value| value.revision())
            .map_err(|_| StoreError::InvalidRequest)?;
        if requires_reconciliation(&mut transaction, &binding.workspace_id, node_id).await? {
            return Err(StoreError::ReconciliationRequired);
        }
        let storage_id: Option<String> = sqlx::query_scalar(
            "SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?",
        )
        .bind(registration_id)
        .bind(node_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let storage_id = match storage_id {
            Some(id) => {
                let checkpoint: i64 = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM draft_checkpoints WHERE document_id=?)",
                )
                .bind(&id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
                if checkpoint != 0 || self.sessions.contains_key(&id) {
                    return Err(StoreError::OutboxBlocked);
                }
                id
            }
            None => {
                let id = format!("r_{}", uuid::Uuid::new_v4().simple());
                sqlx::query("INSERT INTO registered_documents(registration_id,document_id,storage_id) VALUES (?,?,?)")
                    .bind(registration_id).bind(node_id).bind(&id).execute(&mut *transaction).await.map_err(storage_error)?;
                id
            }
        };
        let last = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' ORDER BY operation_seq DESC LIMIT 1")
            .bind(registration_id).bind(node_id).fetch_optional(&mut *transaction).await.map_err(storage_error)?;
        if let Some(row) = last {
            let old = operation(&row, binding, false)?;
            if draft.generation < old.generation {
                return Err(StoreError::StaleGeneration);
            }
            let old_xml = operation_xml(&mut transaction, &old).await?;
            if matches!(
                old.status,
                OutboxStatus::Queued | OutboxStatus::Sent | OutboxStatus::Committed
            ) && old.bytes.starts_with(b"\x89PNG\r\n\x1a\n") != png.is_some()
            {
                return Err(StoreError::InvalidRequest);
            }
            match old.status {
                OutboxStatus::Queued | OutboxStatus::Sent => {
                    if old_xml != draft.xml
                        || old.expected_revision.as_deref() != Some(draft.base_revision.as_str())
                    {
                        return Err(StoreError::OutboxBlocked);
                    }
                    transaction.commit().await.map_err(storage_error)?;
                    return Ok(old);
                }
                OutboxStatus::Committed => {
                    let revision: Option<String> =
                        row.try_get("committed_revision").map_err(storage_error)?;
                    if revision.as_deref() != Some(&draft.base_revision)
                        || old_xml != draft.base_xml
                    {
                        return Err(StoreError::UnsupportedFormat);
                    }
                    if old_xml == draft.xml {
                        transaction.commit().await.map_err(storage_error)?;
                        return Ok(old);
                    }
                    if draft.generation == old.generation {
                        return Err(StoreError::GenerationMismatch);
                    }
                }
                OutboxStatus::Conflict if old.failure_kind.is_none() => {
                    let Some((node, _, resolved)) = crate::diagram_conflicts::verified_completed(
                        &mut transaction,
                        registration_id,
                        binding,
                        &old.operation_id,
                    )
                    .await?
                    else {
                        return Err(StoreError::OutboxBlocked);
                    };
                    if node != node_id
                        || draft.generation < resolved.generation
                        || draft.base_xml != resolved.base_xml
                        || draft.base_revision != resolved.base_revision
                    {
                        return Err(StoreError::OutboxBlocked);
                    }
                }
                _ => return Err(StoreError::OutboxBlocked),
            }
        }
        sqlx::query("INSERT INTO outbox_streams(registration_id,next_seq,ack_through) VALUES (?,1,0) ON CONFLICT(registration_id) DO NOTHING")
            .bind(registration_id).execute(&mut *transaction).await.map_err(storage_error)?;
        let sequence: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(registration_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        let next = sequence
            .checked_add(1)
            .ok_or(StoreError::InvalidGeneration)?;
        sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
            .bind(next)
            .bind(registration_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let id = uuid::Uuid::now_v7().to_string();
        let bytes = png.unwrap_or(draft.xml.as_bytes());
        sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,bytes,payload_len,payload_hash,status) VALUES (?,?,?,?,?,?,?,?,?,?,'queued')")
            .bind(&id).bind(registration_id).bind(sequence).bind(node_id).bind(&storage_id)
            .bind(draft.generation as i64).bind(&draft.base_revision).bind(bytes)
            .bind(bytes.len() as i64).bind(payload_hash(bytes))
            .execute(&mut *transaction).await.map_err(storage_error)?;
        if png.is_some() {
            sqlx::query("INSERT INTO diagram_save_snapshots(operation_id,xml) VALUES (?,?)")
                .bind(&id)
                .bind(&draft.xml)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
        }
        let row = sqlx::query("SELECT * FROM client_outbox WHERE operation_id=?")
            .bind(&id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let queued = operation(&row, binding, false)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok(queued)
    }

    pub async fn read_diagram_draft(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
    ) -> Result<Option<DiagramDraft>, StoreError> {
        validate_node(node_id)?;
        let registration_id = self.registration_id(binding, true).await?;
        let row = sqlx::query("SELECT generation, xml, base_xml, base_revision FROM diagram_drafts WHERE registration_id=? AND node_id=?")
            .bind(registration_id).bind(node_id).fetch_optional(&mut self.connection)
            .await.map_err(storage_error)?;
        row.map(|row| decode(row, node_id)).transpose()
    }

    pub async fn save_diagram_draft(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
        checkpoint: &DiagramDraftCheckpoint,
    ) -> Result<u64, StoreError> {
        validate_node(node_id)?;
        validate_document(
            node_id,
            checkpoint.xml.as_bytes(),
            &checkpoint.base_revision,
        )?;
        validate_document(
            node_id,
            checkpoint.base_xml.as_bytes(),
            &checkpoint.base_revision,
        )?;
        if checkpoint.xml.is_empty() || checkpoint.base_xml.is_empty() {
            return Err(StoreError::InvalidRequest);
        }
        let generation = i64::try_from(checkpoint.expected_generation)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(StoreError::InvalidGeneration)?;
        let registration_id = self.registration_id(binding, true).await?;
        let mut transaction = self.connection.begin().await.map_err(storage_error)?;
        let current = sqlx::query("SELECT generation, xml, base_xml, base_revision FROM diagram_drafts WHERE registration_id=? AND node_id=?")
            .bind(registration_id).bind(node_id).fetch_optional(&mut *transaction)
            .await.map_err(storage_error)?
            .map(|row| decode(row, node_id)).transpose()?;
        if current.as_ref().map_or(0, |draft| draft.generation) != checkpoint.expected_generation {
            return Err(StoreError::StaleGeneration);
        }
        if current.as_ref().is_some_and(|draft| {
            draft.base_xml != checkpoint.base_xml || draft.base_revision != checkpoint.base_revision
        }) {
            return Err(StoreError::InvalidRequest);
        }
        sqlx::query("INSERT INTO diagram_drafts (registration_id,node_id,generation,xml,base_xml,base_revision) VALUES (?,?,?,?,?,?) ON CONFLICT(registration_id,node_id) DO UPDATE SET generation=excluded.generation,xml=excluded.xml")
            .bind(registration_id).bind(node_id).bind(generation).bind(&checkpoint.xml)
            .bind(&checkpoint.base_xml).bind(&checkpoint.base_revision)
            .execute(&mut *transaction).await.map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok(generation as u64)
    }
}

pub(crate) async fn apply_commit(
    connection: &mut sqlx::SqliteConnection,
    registration_id: i64,
    storage_id: &str,
    item: &OutboxOperation,
    commit: &OutboxCommit,
) -> Result<bool, StoreError> {
    let row = sqlx::query("SELECT generation,xml,base_xml,base_revision FROM diagram_drafts WHERE registration_id=? AND node_id=? AND NOT EXISTS(SELECT 1 FROM draft_checkpoints WHERE document_id=?)")
        .bind(registration_id).bind(&item.document_id).bind(storage_id)
        .fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some(row) = row else { return Ok(false) };
    let draft = decode(row, &item.document_id)?;
    let xml = operation_xml(connection, item).await?;
    if draft.generation < item.generation
        || item.expected_revision.as_deref() != Some(draft.base_revision.as_str())
    {
        return Err(StoreError::InvalidReceipt);
    }
    // A receipt advances the base, never a newer local edit.
    sqlx::query("UPDATE diagram_drafts SET base_xml=?,base_revision=? WHERE registration_id=? AND node_id=?")
        .bind(xml).bind(&commit.content_revision).bind(registration_id).bind(&item.document_id)
        .execute(&mut *connection).await.map_err(storage_error)?;
    Ok(true)
}

pub(crate) const MAX_DIAGRAM_BYTES: usize = 25 * 1024 * 1024;

pub(crate) async fn operation_xml(
    connection: &mut sqlx::SqliteConnection,
    item: &OutboxOperation,
) -> Result<String, StoreError> {
    let snapshot: Option<String> =
        sqlx::query_scalar("SELECT xml FROM diagram_save_snapshots WHERE operation_id=?")
            .bind(&item.operation_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(storage_error)?;
    let xml = if item.bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        snapshot.ok_or(StoreError::UnsupportedFormat)?
    } else {
        if snapshot.is_some() {
            return Err(StoreError::UnsupportedFormat);
        }
        String::from_utf8(item.bytes.clone()).map_err(|_| StoreError::UnsupportedFormat)?
    };
    if xml.is_empty() || xml.len() > crate::MAX_DOCUMENT_BYTES {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(xml)
}

pub(crate) async fn upgrade_v26(connection: &mut sqlx::SqliteConnection) -> Result<(), StoreError> {
    let attached: Vec<String> = sqlx::query_scalar("SELECT sql FROM sqlite_schema WHERE tbl_name='client_outbox' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name")
        .fetch_all(&mut *connection).await.map_err(storage_error)?;
    sqlx::raw_sql(r#"
    CREATE TABLE client_outbox_v26 (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore', 'create', 'delete', 'move')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox_v26(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 26214400),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder', 'node_id_conflict', 'node_precondition_failed', 'expected_parent_mismatch')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 26214400),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind IN ('restore', 'create', 'delete', 'move') AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
    INSERT INTO client_outbox_v26 SELECT * FROM client_outbox;
    DROP TABLE client_outbox;
    ALTER TABLE client_outbox_v26 RENAME TO client_outbox;
    CREATE TABLE diagram_save_snapshots (
        operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        xml TEXT NOT NULL CHECK(length(CAST(xml AS BLOB)) BETWEEN 1 AND 5242880)
    );
    PRAGMA user_version=26;
"#).execute(&mut *connection).await.map_err(storage_error)?;
    for definition in attached {
        // Preserve database-owned indexes and triggers; no document text enters this SQL.
        sqlx::raw_sql(sqlx::AssertSqlSafe(definition))
            .execute(&mut *connection)
            .await
            .map_err(storage_error)?;
    }
    Ok(())
}
