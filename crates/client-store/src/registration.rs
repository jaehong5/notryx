use super::{
    ClientStore, OpenedDocument, PersistedDraft, SavedCheckpoint, StoreError, storage_error,
    validate_document,
};
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

fn valid_binding(binding: &RegistrationBinding) -> bool {
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

    pub async fn open_registered_document(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
        base: &[u8],
        base_revision: &str,
    ) -> Result<OpenedDocument, StoreError> {
        validate_document(document_id, base, base_revision)?;
        let registration_id = self.registration_id(binding, true).await?;
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
        } else {
            self.recovery_storage_id(binding, document_id).await?;
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
