use crate::{ClientStore, RegistrationBinding, StoreError, storage_error, validate_document};
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

fn validate_node(node_id: &str) -> Result<(), StoreError> {
    let node = node_id
        .parse::<NodeId>()
        .map_err(|_| StoreError::InvalidDocument)?;
    if node.to_string() != node_id {
        return Err(StoreError::InvalidDocument);
    }
    Ok(())
}

fn decode(row: sqlx::sqlite::SqliteRow, node_id: &str) -> Result<DiagramDraft, StoreError> {
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
