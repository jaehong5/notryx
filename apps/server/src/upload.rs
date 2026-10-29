use crate::{
    Error, UploadLimits, Workspace,
    auth::{AuthError, AuthIdentity},
    state::upload::UploadRecord,
};
use notryx_protocol::WorkspaceRole;
use notryx_server_storage::{MAX_FILE_BYTES, SealedUpload, StoredUpload, UploadWriter};
use uuid::Uuid;

pub(crate) const RECEIVE_SECONDS: i64 = 120;
const UPLOAD_SECONDS: i64 = 24 * 60 * 60;

pub struct UploadGrant {
    pub upload_id: Uuid,
    pub writer: UploadWriter,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uploaded {
    pub upload_id: Uuid,
    pub content_hash: String,
    pub size: u64,
    pub expires_at: i64,
}

pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
pub(crate) fn now() -> Result<i64, Error> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::Storage)?
        .as_secs()
        .try_into()
        .map_err(|_| Error::Storage)
}

impl Workspace {
    pub fn set_upload_limits(&mut self, limits: UploadLimits) -> Result<(), Error> {
        limits.validate().map_err(|_| Error::InvalidRequest)?;
        self.upload_limits = limits;
        Ok(())
    }

    async fn upload_identity(&mut self, token: &str, time: i64) -> Result<AuthIdentity, Error> {
        let identity = self.authenticate(token, time).await.map_err(Error::Auth)?;
        if identity.role == WorkspaceRole::Reader {
            return Err(Error::Auth(AuthError::Forbidden));
        }
        Ok(identity)
    }

    pub async fn begin_upload(
        &mut self,
        token: &str,
        expected_hash: String,
        declared_size: Option<u64>,
        time: i64,
    ) -> Result<UploadGrant, Error> {
        let identity = self.upload_identity(token, time).await?;
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        self.verify_mutation_root()?;
        if !valid_hash(&expected_hash) || time < 0 {
            return Err(Error::InvalidRequest);
        }
        if declared_size.is_some_and(|size| size > MAX_FILE_BYTES as u64) {
            return Err(Error::UploadTooLarge);
        }
        self.cleanup_uploads(time).await?;
        if self
            .receiving_uploads
            .values()
            .filter(|alive| alive.strong_count() > 0)
            .count()
            >= self.upload_limits.max_receiving as usize
        {
            return Err(Error::UploadQuota);
        }
        let id = Uuid::new_v4();
        let record = UploadRecord {
            id,
            device_id: identity.device_id,
            status: "receiving".into(),
            expected_hash,
            declared_size,
            reserved_bytes: declared_size.unwrap_or(MAX_FILE_BYTES as u64),
            started_at: time,
            expires_at: time
                .checked_add(RECEIVE_SECONDS)
                .ok_or(Error::InvalidRequest)?,
            content_hash: None,
            size: None,
            pending_operation_id: None,
        };
        self.state
            .reserve_upload(&record, &self.upload_limits)
            .await?;
        match self.files.begin_upload(id, record.reserved_bytes) {
            Ok(writer) => {
                self.receiving_uploads.insert(id, writer.liveness());
                Ok(UploadGrant {
                    upload_id: id,
                    writer,
                })
            }
            Err(error) => {
                let _ = self.state.delete_upload(id, true).await;
                Err(error.into())
            }
        }
    }

    pub async fn finish_upload(
        &mut self,
        token: &str,
        id: Uuid,
        sealed: SealedUpload,
        time: i64,
    ) -> Result<Uploaded, Error> {
        let result = self.finish_upload_inner(token, id, sealed, time).await;
        if result.is_err() {
            // An ambiguous completion may already be ready. Only receiving reservations may be removed.
            let _ = self.abort_upload(id).await;
        }
        result
    }

    async fn finish_upload_inner(
        &mut self,
        token: &str,
        id: Uuid,
        sealed: SealedUpload,
        time: i64,
    ) -> Result<Uploaded, Error> {
        let identity = self.upload_identity(token, time).await?;
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        self.verify_mutation_root()?;
        let record = self
            .state
            .upload(id)
            .await?
            .ok_or(Error::UploadUnavailable)?;
        if identity.device_id != record.device_id {
            return Err(Error::Auth(AuthError::Forbidden));
        }
        if record.status != "receiving"
            || time < record.started_at
            || time >= record.expires_at
            || id != sealed.id()
        {
            return Err(Error::UploadUnavailable);
        }
        if sealed.hash() != record.expected_hash {
            return Err(Error::DigestMismatch);
        }
        if record
            .declared_size
            .is_some_and(|size| size != sealed.size())
        {
            return Err(Error::InvalidRequest);
        }
        if sealed.size() > record.reserved_bytes {
            return Err(Error::UploadTooLarge);
        }
        self.point(crate::Phase::BeforeUploadPublish)?;
        let checked = time.max(now()?);
        self.upload_identity(token, checked).await?;
        if checked >= record.expires_at {
            return Err(Error::UploadUnavailable);
        }
        let stored = self.files.commit_upload(sealed)?;
        self.point(crate::Phase::AfterUploadPublish)?;
        let completed = time.max(now()?);
        self.upload_identity(token, completed).await?;
        if completed >= record.expires_at {
            return Err(Error::UploadUnavailable);
        }
        let expires_at = completed
            .checked_add(UPLOAD_SECONDS)
            .ok_or(Error::InvalidRequest)?;
        self.state
            .complete_upload(id, &stored.hash, stored.size, expires_at)
            .await?;
        self.receiving_uploads.remove(&id);
        self.point(crate::Phase::AfterUploadFinish)?;
        Ok(Uploaded {
            upload_id: id,
            content_hash: stored.hash,
            size: stored.size,
            expires_at,
        })
    }

    pub async fn abort_upload(&mut self, id: Uuid) -> Result<(), Error> {
        if self.journal_integrity_failed {
            return Err(Error::RecoveryRequired);
        }
        self.state.delete_upload(id, true).await?;
        self.cleanup_uploads(now()?).await
    }

    pub(crate) async fn cleanup_uploads(&mut self, time: i64) -> Result<(), Error> {
        if self.journal_integrity_failed {
            return Err(Error::RecoveryRequired);
        }
        self.receiving_uploads
            .retain(|_, alive| alive.strong_count() > 0);
        let records = self.state.uploads().await?;
        for record in records {
            let inactive = !self.receiving_uploads.contains_key(&record.id);
            if record.pending_operation_id.is_none()
                && (record.status == "consumed"
                    || (record.status == "receiving" && inactive)
                    || (record.status == "ready" && time >= record.expires_at))
            {
                self.state.delete_upload(record.id, false).await?;
            }
        }
        let records = self.state.uploads().await?;
        for entry in self.files.list_upload_staging()? {
            if self.receiving_uploads.contains_key(&entry.id) {
                continue;
            }
            let keep = entry.complete
                && records
                    .iter()
                    .any(|record| record.id == entry.id && record.status == "ready");
            if !keep {
                self.files.remove_staged_entry(&entry)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn validate_upload_files(&mut self) -> Result<(), Error> {
        let entries = self.files.list_upload_staging()?;
        for record in self.state.uploads().await? {
            if record.status == "ready"
                && (record.pending_operation_id.is_some() || record.expires_at > now()?)
            {
                let entry = entries
                    .iter()
                    .find(|entry| entry.id == record.id && entry.complete)
                    .ok_or(Error::InvalidFormat)?;
                self.files.validate_upload(&record.stored()?)?;
                if Some(entry.stamp.size) != record.size {
                    return Err(Error::InvalidFormat);
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn attachment_upload(
        &mut self,
        id: Uuid,
        token: Option<&str>,
        stream_id: Uuid,
    ) -> Result<StoredUpload, Error> {
        let time = now()?;
        let identity = self
            .upload_identity(token.ok_or(Error::Auth(AuthError::Unauthorized))?, time)
            .await?;
        if identity.stream_id != stream_id {
            return Err(Error::Auth(AuthError::Forbidden));
        }
        let record = self
            .state
            .upload(id)
            .await?
            .ok_or(Error::UploadUnavailable)?;
        if record.device_id != identity.device_id {
            return Err(Error::Auth(AuthError::Forbidden));
        }
        if record.status != "ready"
            || record.pending_operation_id.is_some()
            || time >= record.expires_at
        {
            return Err(Error::UploadUnavailable);
        }
        record.stored()
    }
}
