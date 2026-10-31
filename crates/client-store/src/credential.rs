use crate::{ClientStore, RegistrationBinding, StoreError, registration, storage_error};
use sqlx::{Connection, Row, sqlite::SqliteRow};
use uuid::Uuid;

pub(crate) const UPGRADE_V9: &str = "
    CREATE TABLE credential_pending (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        item_id TEXT NOT NULL UNIQUE,
        server_origin TEXT NOT NULL,
        workspace_id TEXT NOT NULL,
        workspace_epoch TEXT NOT NULL,
        device_id TEXT NOT NULL,
        device_stream_id TEXT NOT NULL,
        previous_registration_id INTEGER REFERENCES client_registrations(id)
    );
    CREATE TABLE credential_links (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        item_id TEXT UNIQUE
    );
    PRAGMA user_version = 9;
";

pub(crate) const UPGRADE_V24: &str = "
    CREATE TABLE credential_expiry (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        idle_expires_at INTEGER NOT NULL CHECK(typeof(idle_expires_at)='integer' AND idle_expires_at>0),
        absolute_expires_at INTEGER NOT NULL CHECK(typeof(absolute_expires_at)='integer' AND absolute_expires_at>=idle_expires_at)
    );
    PRAGMA user_version = 24;
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialPending {
    pub id: Uuid,
    pub binding: RegistrationBinding,
}

fn valid_item_id(id: Uuid) -> bool {
    id.get_version_num() == 4 && id.get_variant() == uuid::Variant::RFC4122
}

fn parse_item_id(value: &str) -> Result<Uuid, StoreError> {
    let id = Uuid::parse_str(value).map_err(|_| StoreError::UnsupportedFormat)?;
    if !valid_item_id(id) || id.to_string() != value {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(id)
}

fn pending_from_row(row: &SqliteRow) -> Result<CredentialPending, StoreError> {
    let value: String = row.try_get("item_id").map_err(storage_error)?;
    Ok(CredentialPending {
        id: parse_item_id(&value)?,
        binding: registration::binding_from_row(row)?,
    })
}

impl ClientStore {
    pub async fn credential_expiry(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<Option<(u64, u64)>, StoreError> {
        let id = self.registration_id(binding, true).await?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT idle_expires_at,absolute_expires_at FROM credential_expiry WHERE registration_id=?",
        )
        .bind(id)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        row.map(|(idle, absolute)| {
            if idle <= 0 || absolute < idle {
                return Err(StoreError::UnsupportedFormat);
            }
            Ok((idle as u64, absolute as u64))
        })
        .transpose()
    }

    pub async fn set_credential_expiry(
        &mut self,
        binding: &RegistrationBinding,
        idle_expires_at: u64,
        absolute_expires_at: u64,
    ) -> Result<(), StoreError> {
        if idle_expires_at == 0 || idle_expires_at > absolute_expires_at {
            return Err(StoreError::InvalidRegistration);
        }
        let idle = i64::try_from(idle_expires_at).map_err(|_| StoreError::InvalidRegistration)?;
        let absolute =
            i64::try_from(absolute_expires_at).map_err(|_| StoreError::InvalidRegistration)?;
        let id = self.registration_id(binding, true).await?;
        sqlx::query(
            "INSERT INTO credential_expiry(registration_id,idle_expires_at,absolute_expires_at) VALUES(?,?,?)
             ON CONFLICT(registration_id) DO UPDATE SET idle_expires_at=excluded.idle_expires_at,absolute_expires_at=excluded.absolute_expires_at",
        )
        .bind(id)
        .bind(idle)
        .bind(absolute)
        .execute(&mut self.connection)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn prepare_credential_pending(
        &mut self,
        binding: &RegistrationBinding,
        id: Uuid,
    ) -> Result<CredentialPending, StoreError> {
        if !registration::valid_binding(binding) || !valid_item_id(id) {
            return Err(StoreError::InvalidRegistration);
        }
        if !self.sessions.is_empty() {
            return Err(StoreError::OpenEditSessions);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let occupied: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credential_pending)")
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
        if occupied != 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        let duplicate: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?)")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .bind(&binding.device_id).bind(&binding.device_stream_id)
            .fetch_one(&mut *tx).await.map_err(storage_error)?;
        if duplicate != 0 {
            return Err(StoreError::StaleRegistration);
        }
        let item_used: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credential_links WHERE item_id=?)")
                .bind(id.to_string())
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if item_used != 0 {
            return Err(StoreError::InvalidRegistration);
        }
        let previous: Option<i64> =
            sqlx::query_scalar("SELECT id FROM client_registrations WHERE status='active'")
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
        sqlx::query("INSERT INTO credential_pending(singleton,item_id,server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,previous_registration_id) VALUES(1,?,?,?,?,?,?,?)")
            .bind(id.to_string()).bind(&binding.server_origin).bind(&binding.workspace_id)
            .bind(&binding.workspace_epoch).bind(&binding.device_id).bind(&binding.device_stream_id)
            .bind(previous).execute(&mut *tx).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(CredentialPending {
            id,
            binding: binding.clone(),
        })
    }

    pub async fn pending_credential(&mut self) -> Result<Option<CredentialPending>, StoreError> {
        let row = sqlx::query("SELECT item_id,server_origin,workspace_id,workspace_epoch,device_id,device_stream_id FROM credential_pending WHERE singleton=1")
            .fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        row.as_ref().map(pending_from_row).transpose()
    }

    pub async fn discard_credential_pending(
        &mut self,
        pending: &CredentialPending,
    ) -> Result<(), StoreError> {
        if !valid_item_id(pending.id) || !registration::valid_binding(&pending.binding) {
            return Err(StoreError::InvalidRegistration);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT item_id,server_origin,workspace_id,workspace_epoch,device_id,device_stream_id FROM credential_pending WHERE singleton=1")
            .fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::StaleRegistration)?;
        if pending_from_row(&row)? != *pending {
            return Err(StoreError::StaleRegistration);
        }
        sqlx::query("DELETE FROM credential_pending WHERE singleton=1")
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    pub async fn commit_credential_pending(
        &mut self,
        pending: &CredentialPending,
        stored: bool,
    ) -> Result<(), StoreError> {
        if !valid_item_id(pending.id) || !registration::valid_binding(&pending.binding) {
            return Err(StoreError::InvalidRegistration);
        }
        if !self.sessions.is_empty() {
            return Err(StoreError::OpenEditSessions);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT item_id,server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,previous_registration_id FROM credential_pending WHERE singleton=1")
            .fetch_optional(&mut *tx).await.map_err(storage_error)?.ok_or(StoreError::StaleRegistration)?;
        if pending_from_row(&row)? != *pending {
            return Err(StoreError::StaleRegistration);
        }
        let previous: Option<i64> = row
            .try_get("previous_registration_id")
            .map_err(storage_error)?;
        let active: Option<i64> =
            sqlx::query_scalar("SELECT id FROM client_registrations WHERE status='active'")
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
        if previous != active {
            return Err(StoreError::StaleRegistration);
        }
        let binding = &pending.binding;
        let duplicate: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?)")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .bind(&binding.device_id).bind(&binding.device_stream_id)
            .fetch_one(&mut *tx).await.map_err(storage_error)?;
        if duplicate != 0 {
            return Err(StoreError::StaleRegistration);
        }
        if let Some(previous) = previous {
            let updated = sqlx::query(
                "UPDATE client_registrations SET status='frozen' WHERE id=? AND status='active'",
            )
            .bind(previous)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            if updated.rows_affected() != 1 {
                return Err(StoreError::StaleRegistration);
            }
        }
        let inserted = sqlx::query("INSERT INTO client_registrations(server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,status) VALUES(?,?,?,?,?,'active')")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .bind(&binding.device_id).bind(&binding.device_stream_id)
            .execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("INSERT INTO credential_links(registration_id,item_id) VALUES(?,?)")
            .bind(inserted.last_insert_rowid())
            .bind(stored.then(|| pending.id.to_string()))
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        sqlx::query("DELETE FROM credential_pending WHERE singleton=1")
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    pub async fn active_credential(
        &mut self,
    ) -> Result<Option<(RegistrationBinding, Option<Uuid>)>, StoreError> {
        let row = sqlx::query("SELECT r.server_origin,r.workspace_id,r.workspace_epoch,r.device_id,r.device_stream_id,c.item_id FROM client_registrations r LEFT JOIN credential_links c ON c.registration_id=r.id WHERE r.status='active'")
            .fetch_optional(&mut self.connection).await.map_err(storage_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let binding = registration::binding_from_row(&row)?;
        let item: Option<String> = row.try_get("item_id").map_err(storage_error)?;
        let item = item.as_deref().map(parse_item_id).transpose()?;
        Ok(Some((binding, item)))
    }
}
