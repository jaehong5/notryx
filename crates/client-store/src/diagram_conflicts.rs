use crate::{
    ClientStore, DiagramDraft, OutboxOperation, OutboxStatus, RegistrationBinding, StoreError,
    diagram_drafts::{decode, operation_xml, validate_node},
    outbox::operation,
    storage_error, validate_document,
};
use sqlx::{Connection, Row, SqliteConnection};

pub(crate) const UPGRADE_V27: &str = "
    CREATE TABLE diagram_conflict_copies (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_id TEXT NOT NULL REFERENCES client_outbox(operation_id),
        node_id TEXT NOT NULL,
        generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>0),
        xml TEXT NOT NULL CHECK(length(CAST(xml AS BLOB)) BETWEEN 1 AND 5242880),
        base_xml TEXT NOT NULL CHECK(length(CAST(base_xml AS BLOB)) BETWEEN 1 AND 5242880),
        base_revision TEXT NOT NULL CHECK(length(base_revision) BETWEEN 1 AND 256),
        png_hash TEXT NOT NULL CHECK(length(png_hash)=71),
        selected_generation INTEGER,
        remote_xml TEXT CHECK(remote_xml IS NULL OR length(CAST(remote_xml AS BLOB)) BETWEEN 1 AND 5242880),
        remote_revision TEXT CHECK(remote_revision IS NULL OR length(remote_revision) BETWEEN 1 AND 256),
        completed_generation INTEGER,
        PRIMARY KEY(operation_id,generation),
        FOREIGN KEY(registration_id,node_id) REFERENCES diagram_drafts(registration_id,node_id),
        CHECK((selected_generation IS NULL AND remote_xml IS NULL AND remote_revision IS NULL AND completed_generation IS NULL)
            OR (selected_generation IS NOT NULL AND remote_xml IS NOT NULL AND remote_revision IS NOT NULL
                AND completed_generation IS NOT NULL AND selected_generation>=generation AND completed_generation=selected_generation+1))
    );
    CREATE UNIQUE INDEX diagram_conflict_completed ON diagram_conflict_copies(operation_id) WHERE completed_generation IS NOT NULL;
    PRAGMA user_version=27;
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagramConflictCopy {
    pub operation_id: String,
    pub generation: u64,
    pub png_hash: String,
}

struct SavedCopy {
    summary: DiagramConflictCopy,
    node_id: String,
    xml: String,
    base_xml: String,
    base_revision: String,
    completed: Option<(u64, DiagramDraft)>,
}

fn valid_hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn saved_copy(row: sqlx::sqlite::SqliteRow) -> Result<SavedCopy, StoreError> {
    let generation: i64 = row.try_get("generation").map_err(storage_error)?;
    let node_id: String = row.try_get("node_id").map_err(storage_error)?;
    let xml: String = row.try_get("xml").map_err(storage_error)?;
    let base_xml: String = row.try_get("base_xml").map_err(storage_error)?;
    let base_revision: String = row.try_get("base_revision").map_err(storage_error)?;
    let png_hash: String = row.try_get("png_hash").map_err(storage_error)?;
    if generation <= 0 || xml.is_empty() || base_xml.is_empty() || !valid_hash(&png_hash) {
        return Err(StoreError::UnsupportedFormat);
    }
    validate_document(&node_id, xml.as_bytes(), &base_revision)
        .and_then(|()| validate_document(&node_id, base_xml.as_bytes(), &base_revision))
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let selected: Option<i64> = row.try_get("selected_generation").map_err(storage_error)?;
    let completed: Option<i64> = row.try_get("completed_generation").map_err(storage_error)?;
    let remote_xml: Option<String> = row.try_get("remote_xml").map_err(storage_error)?;
    let remote_revision: Option<String> = row.try_get("remote_revision").map_err(storage_error)?;
    let completed = match (selected, completed, remote_xml, remote_revision) {
        (None, None, None, None) => None,
        (Some(selected), Some(completed), Some(xml), Some(revision))
            if selected >= generation
                && selected.checked_add(1) == Some(completed)
                && !xml.is_empty() =>
        {
            validate_document(&node_id, xml.as_bytes(), &revision)
                .map_err(|_| StoreError::UnsupportedFormat)?;
            Some((
                selected as u64,
                DiagramDraft {
                    generation: completed as u64,
                    base_xml: xml.clone(),
                    xml,
                    base_revision: revision,
                },
            ))
        }
        _ => return Err(StoreError::UnsupportedFormat),
    };
    Ok(SavedCopy {
        summary: DiagramConflictCopy {
            operation_id: row.try_get("operation_id").map_err(storage_error)?,
            generation: generation as u64,
            png_hash,
        },
        node_id,
        xml,
        base_xml,
        base_revision,
        completed,
    })
}

async fn draft_on(
    connection: &mut SqliteConnection,
    registration: i64,
    node_id: &str,
) -> Result<Option<DiagramDraft>, StoreError> {
    sqlx::query("SELECT generation,xml,base_xml,base_revision FROM diagram_drafts WHERE registration_id=? AND node_id=?")
        .bind(registration).bind(node_id).fetch_optional(connection).await.map_err(storage_error)?
        .map(|row| decode(row, node_id)).transpose()
}

async fn conflict_operation(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<OutboxOperation, StoreError> {
    let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND operation_id=? AND operation_kind='content'")
        .bind(registration).bind(operation_id).fetch_optional(connection).await.map_err(storage_error)?.ok_or(StoreError::InvalidRequest)?;
    let item = operation(&row, binding, false)?;
    if item.status != OutboxStatus::Conflict || item.failure_kind.is_some() {
        return Err(StoreError::InvalidRequest);
    }
    Ok(item)
}

async fn validate_copy(
    connection: &mut SqliteConnection,
    item: &OutboxOperation,
    copy: &SavedCopy,
) -> Result<(), StoreError> {
    if item.document_id != copy.node_id
        || item.operation_id != copy.summary.operation_id
        || item.generation > copy.summary.generation
        || item.expected_revision.as_deref() != Some(copy.base_revision.as_str())
        || (item.generation == copy.summary.generation
            && operation_xml(connection, item).await? != copy.xml)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(())
}

pub(crate) async fn verified_completed(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<Option<(String, u64, DiagramDraft)>, StoreError> {
    let row = sqlx::query("SELECT * FROM diagram_conflict_copies WHERE registration_id=? AND operation_id=? AND completed_generation IS NOT NULL")
        .bind(registration).bind(operation_id).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some(row) = row else { return Ok(None) };
    let copy = saved_copy(row)?;
    let item = conflict_operation(connection, registration, binding, operation_id).await?;
    validate_copy(connection, &item, &copy).await?;
    let (selected, result) = copy.completed.ok_or(StoreError::UnsupportedFormat)?;
    let draft = draft_on(connection, registration, &copy.node_id)
        .await?
        .ok_or(StoreError::UnsupportedFormat)?;
    let confirmed: i64 =
        sqlx::query_scalar("SELECT confirmed_through FROM outbox_streams WHERE registration_id=?")
            .bind(registration)
            .fetch_one(connection)
            .await
            .map_err(storage_error)?;
    if confirmed < item.operation_seq as i64
        || draft.generation < result.generation
        || (draft.generation == result.generation && draft != result)
    {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(Some((copy.node_id, selected, result)))
}

async fn latest_conflict(
    connection: &mut SqliteConnection,
    registration: i64,
    binding: &RegistrationBinding,
    node_id: &str,
) -> Result<Option<OutboxOperation>, StoreError> {
    let row = sqlx::query("SELECT * FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_kind='content' ORDER BY operation_seq DESC LIMIT 1")
        .bind(registration).bind(node_id).fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let Some(row) = row else { return Ok(None) };
    let item = operation(&row, binding, false)?;
    if item.status != OutboxStatus::Conflict
        || item.failure_kind.is_some()
        || verified_completed(connection, registration, binding, &item.operation_id)
            .await?
            .is_some()
    {
        return Ok(None);
    }
    Ok(Some(item))
}

async fn matching_copy(
    connection: &mut SqliteConnection,
    registration: i64,
    item: &OutboxOperation,
    draft: &DiagramDraft,
) -> Result<Option<SavedCopy>, StoreError> {
    let row = sqlx::query("SELECT * FROM diagram_conflict_copies WHERE registration_id=? AND operation_id=? AND generation<=? AND xml=? AND base_xml=? AND base_revision=? AND completed_generation IS NULL ORDER BY generation DESC LIMIT 1")
        .bind(registration).bind(&item.operation_id).bind(draft.generation as i64).bind(&draft.xml).bind(&draft.base_xml).bind(&draft.base_revision)
        .fetch_optional(&mut *connection).await.map_err(storage_error)?;
    let copy = row.map(saved_copy).transpose()?;
    if let Some(copy) = &copy {
        validate_copy(connection, item, copy).await?;
    }
    Ok(copy)
}

impl ClientStore {
    // The native caller validates embedded XML and durably exports the PNG before recording proof.
    pub async fn record_diagram_conflict_copy(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
        expected_generation: u64,
        png_hash: &str,
    ) -> Result<Option<DiagramConflictCopy>, StoreError> {
        validate_node(node_id)?;
        if !valid_hash(png_hash) {
            return Err(StoreError::InvalidRequest);
        }
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let draft = draft_on(&mut tx, registration, node_id)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        if draft.generation != expected_generation {
            return Err(StoreError::StaleGeneration);
        }
        let Some(item) = latest_conflict(&mut tx, registration, binding, node_id).await? else {
            return Ok(None);
        };
        if item.generation > draft.generation
            || item.expected_revision.as_deref() != Some(draft.base_revision.as_str())
        {
            return Err(StoreError::UnsupportedFormat);
        }
        if let Some(copy) = matching_copy(&mut tx, registration, &item, &draft).await? {
            return Ok(Some(copy.summary));
        }
        let copy = SavedCopy {
            summary: DiagramConflictCopy {
                operation_id: item.operation_id.clone(),
                generation: draft.generation,
                png_hash: png_hash.into(),
            },
            node_id: node_id.into(),
            xml: draft.xml,
            base_xml: draft.base_xml,
            base_revision: draft.base_revision,
            completed: None,
        };
        validate_copy(&mut tx, &item, &copy).await?;
        sqlx::query("INSERT INTO diagram_conflict_copies(registration_id,operation_id,node_id,generation,xml,base_xml,base_revision,png_hash) VALUES(?,?,?,?,?,?,?,?)")
            .bind(registration).bind(&item.operation_id).bind(node_id).bind(draft.generation as i64).bind(&copy.xml).bind(&copy.base_xml).bind(&copy.base_revision).bind(png_hash)
            .execute(&mut *tx).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(Some(copy.summary))
    }
    pub async fn read_diagram_conflict_copy(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
    ) -> Result<Option<DiagramConflictCopy>, StoreError> {
        validate_node(node_id)?;
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let Some(draft) = draft_on(&mut tx, registration, node_id).await? else {
            return Ok(None);
        };
        let Some(item) = latest_conflict(&mut tx, registration, binding, node_id).await? else {
            return Ok(None);
        };
        Ok(matching_copy(&mut tx, registration, &item, &draft)
            .await?
            .map(|copy| copy.summary))
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "Keep the confirmed diagram, generation and remote revision explicit at this boundary."
    )]
    // The native caller verifies the current remote bytes and revision before this local transaction.
    pub async fn complete_diagram_conflict_choose_remote(
        &mut self,
        binding: &RegistrationBinding,
        node_id: &str,
        expected_generation: u64,
        expected_operation_id: &str,
        remote_xml: &str,
        remote_revision: &str,
    ) -> Result<DiagramDraft, StoreError> {
        validate_node(node_id)?;
        validate_document(node_id, remote_xml.as_bytes(), remote_revision)?;
        if remote_xml.is_empty() {
            return Err(StoreError::InvalidRequest);
        }
        notryx_protocol::ContentPrecondition::parse(&format!("\"{remote_revision}\""))
            .and_then(|value| value.revision())
            .map_err(|_| StoreError::InvalidRequest)?;
        let generation = expected_generation
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(StoreError::InvalidGeneration)?;
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let draft = draft_on(&mut tx, registration, node_id)
            .await?
            .ok_or(StoreError::MissingDraft)?;
        if let Some((completed_node, selected, result)) =
            verified_completed(&mut tx, registration, binding, expected_operation_id).await?
        {
            if completed_node != node_id
                || selected != expected_generation
                || result.xml != remote_xml
                || result.base_revision != remote_revision
            {
                return Err(StoreError::InvalidRequest);
            }
            if draft != result {
                return Err(StoreError::StaleGeneration);
            }
            return Ok(result);
        }
        if draft.generation != expected_generation {
            return Err(StoreError::StaleGeneration);
        }
        let item =
            conflict_operation(&mut tx, registration, binding, expected_operation_id).await?;
        if item.document_id != node_id {
            return Err(StoreError::InvalidRequest);
        }
        let copy = matching_copy(&mut tx, registration, &item, &draft)
            .await?
            .ok_or(StoreError::InvalidRequest)?;
        let (ack, confirmed, next): (i64, i64, i64) = sqlx::query_as("SELECT ack_through,confirmed_through,next_seq FROM outbox_streams WHERE registration_id=?")
            .bind(registration).fetch_one(&mut *tx).await.map_err(storage_error)?;
        let following: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_outbox WHERE registration_id=? AND document_id=? AND operation_seq>? AND status IN ('queued','sent','conflict'))")
            .bind(registration).bind(node_id).bind(item.operation_seq as i64).fetch_one(&mut *tx).await.map_err(storage_error)?;
        if confirmed < item.operation_seq as i64 || confirmed > ack || ack >= next || following != 0
        {
            return Err(StoreError::OutboxBlocked);
        }
        sqlx::query("UPDATE diagram_drafts SET generation=?,xml=?,base_xml=?,base_revision=? WHERE registration_id=? AND node_id=?")
            .bind(generation).bind(remote_xml).bind(remote_xml).bind(remote_revision).bind(registration).bind(node_id)
            .execute(&mut *tx).await.map_err(storage_error)?;
        let changed = sqlx::query("UPDATE diagram_conflict_copies SET selected_generation=?,remote_xml=?,remote_revision=?,completed_generation=? WHERE registration_id=? AND operation_id=? AND generation=? AND completed_generation IS NULL")
            .bind(expected_generation as i64).bind(remote_xml).bind(remote_revision).bind(generation).bind(registration).bind(expected_operation_id).bind(copy.summary.generation as i64)
            .execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidRequest);
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(DiagramDraft {
            generation: generation as u64,
            xml: remote_xml.into(),
            base_xml: remote_xml.into(),
            base_revision: remote_revision.into(),
        })
    }
}
