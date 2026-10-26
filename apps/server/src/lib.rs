//! Internal server storage core. No network or authentication adapter is exposed.

mod state;

use notryx_domain::{
    ChangeSeq, ContentRevision, MetaRevision, NodeId, OperationId, WorkspaceEpoch, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Storage,
    InvalidFormat,
    InvalidRequest,
    DigestMismatch,
    WorkspaceMismatch,
    UnknownStream,
    OperationGap,
    IdempotencyMismatch,
    RecoveryRequired,
    ExternalChange,
    NotFound,
    Locked,
    ReceiptLimit,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub node_id: NodeId,
    pub parent_id: NodeId,
    pub name: String,
    pub content_rev: ContentRevision,
    pub meta_rev: MetaRevision,
    pub content_hash: String,
    pub size: u64,
    pub last_change_seq: ChangeSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Saved(Document),
    PreconditionFailed { current: ContentRevision },
    NotFound,
    NotApplied,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub operation_id: OperationId,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveContent {
    pub epoch: WorkspaceEpoch,
    pub stream_id: Uuid,
    pub operation_seq: u64,
    pub operation_id: OperationId,
    pub node_id: NodeId,
    pub expected_content_rev: ContentRevision,
    pub content_hash: String,
    pub bytes: Vec<u8>,
}

use notryx_domain::mutation::{
    RecoveryDecision, check_content_revision, decide_content_recovery, validate_name,
};
use notryx_server_storage::{FileStore, StorageError};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use state::{Intent, State};

#[cfg(test)]
type FailureHook = Box<dyn FnMut(Phase) -> Result<(), Error>>;

pub struct Workspace {
    state: State,
    files: FileStore,
    ready: bool,
    #[cfg(test)]
    hook: Option<FailureHook>,
}

impl From<StorageError> for Error {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Locked => Self::Locked,
            StorageError::NotFound => Self::NotFound,
            StorageError::Changed => Self::ExternalChange,
            StorageError::InvalidName | StorageError::TooLarge => Self::InvalidRequest,
            StorageError::InvalidMarker
            | StorageError::ChecksumMismatch
            | StorageError::InvalidHash => Self::InvalidFormat,
            _ => Self::Storage,
        }
    }
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn node_id() -> NodeId {
    Uuid::now_v7()
        .to_string()
        .parse()
        .expect("generated UUIDv7")
}
fn revision() -> ContentRevision {
    ContentRevision::new(Uuid::new_v4().to_string()).expect("nonempty UUID")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    BeforeAdmission,
    AfterAdmission,
    BeforeFinish,
    AfterFinish,
    BeforeRecoveryFinish,
    AfterRecoveryFinish,
    #[cfg(test)]
    Storage(notryx_server_storage::StorageStage),
}

impl Workspace {
    pub async fn initialize(notes: &Path, state_dir: &Path) -> Result<Self, Error> {
        let workspace_id = Uuid::new_v4()
            .to_string()
            .parse()
            .expect("generated UUIDv4");
        let files = FileStore::init(notes, state_dir, workspace_id)?;
        let state = State::initialize(&files.state_db_path(), workspace_id).await?;
        files.sync_state_dir()?;
        Ok(Self {
            state,
            files,
            ready: true,
            #[cfg(test)]
            hook: None,
        })
    }

    pub async fn open(notes: &Path, state_dir: &Path) -> Result<Self, Error> {
        let mut workspace = Self::load(notes, state_dir).await?;
        workspace.recover().await?;
        Ok(workspace)
    }

    async fn load(notes: &Path, state_dir: &Path) -> Result<Self, Error> {
        let files = FileStore::open(notes, state_dir)?;
        files.validate_state_database()?;
        let mut state = State::open(&files.state_db_path(), files.workspace_id()).await?;
        for document in state.documents().await? {
            validate_name(&document.name).map_err(|_| Error::InvalidFormat)?;
            if document.parent_id != state.root_id
                || files.read_object(&document.content_hash)?.len() as u64 != document.size
            {
                return Err(Error::InvalidFormat);
            }
        }
        Ok(Self {
            state,
            files,
            ready: false,
            #[cfg(test)]
            hook: None,
        })
    }

    pub fn epoch(&self) -> WorkspaceEpoch {
        self.state.epoch
    }
    pub fn workspace_id(&self) -> WorkspaceId {
        self.state.workspace_id
    }
    pub fn ready(&self) -> bool {
        self.ready
    }

    pub async fn read(&mut self, node_id: NodeId) -> Result<(Document, Vec<u8>), Error> {
        let document = self.state.document(node_id).await?.ok_or(Error::NotFound)?;
        let bytes = self.files.read_object(&document.content_hash)?;
        if bytes.len() as u64 != document.size {
            return Err(Error::InvalidFormat);
        }
        Ok((document, bytes))
    }

    // This local bootstrap operation must never be exposed as an unauthenticated endpoint.
    pub async fn adopt_root_document(&mut self, name: &str) -> Result<Document, Error> {
        if !self.ready {
            return Err(Error::RecoveryRequired);
        }
        validate_name(name).map_err(|_| Error::InvalidRequest)?;
        let observed = self.files.observe(name)?;
        self.files.sync_observed(name, &observed.stamp)?;
        let content_hash = self.files.put_object(&observed.bytes)?;
        let document = Document {
            node_id: node_id(),
            parent_id: self.state.root_id,
            name: name.to_owned(),
            content_rev: revision(),
            meta_rev: MetaRevision::new(Uuid::new_v4().to_string()).expect("nonempty UUID"),
            content_hash,
            size: observed.bytes.len() as u64,
            last_change_seq: self.state.next_change_seq().await?,
        };
        self.state.adopt(&document).await?;
        Ok(document)
    }

    // Pairing and authorization must precede this local-only stream registration in a future server adapter.
    pub async fn register_stream(&mut self) -> Result<Uuid, Error> {
        if !self.ready {
            return Err(Error::RecoveryRequired);
        }
        self.state.register_stream().await
    }

    pub async fn save(&mut self, request: SaveContent) -> Result<Receipt, Error> {
        if request.epoch != self.state.epoch {
            return Err(Error::WorkspaceMismatch);
        }
        if request.operation_seq == 0
            || request.bytes.len() > notryx_server_storage::MAX_FILE_BYTES
            || request.expected_content_rev.as_str().len() > 256
        {
            return Err(Error::InvalidRequest);
        }
        if request.content_hash != hash(&request.bytes) {
            return Err(Error::DigestMismatch);
        }
        let fingerprint = hash(
            &serde_json::to_vec(&(
                "replace_content/v1",
                request.epoch,
                request.stream_id,
                request.operation_seq,
                request.operation_id,
                request.node_id,
                &request.expected_content_rev,
                &request.content_hash,
            ))
            .map_err(state::db_error)?,
        );
        if let Some(receipt) = self.state.previous(&request, &fingerprint).await? {
            return Ok(receipt);
        }
        if !self.ready {
            return Err(Error::RecoveryRequired);
        }
        let Some(before) = self.state.document(request.node_id).await? else {
            return self
                .terminal(&request, &fingerprint, Outcome::NotFound)
                .await;
        };
        let observed = self.files.observe(&before.name)?;
        if observed.stamp.hash != before.content_hash {
            return Err(Error::ExternalChange);
        }
        if !check_content_revision(&before.content_rev, &request.expected_content_rev) {
            return self
                .terminal(
                    &request,
                    &fingerprint,
                    Outcome::PreconditionFailed {
                        current: before.content_rev,
                    },
                )
                .await;
        }
        self.put_object(&observed.bytes)?;
        self.put_object(&request.bytes)?;
        let mut proposed = before.clone();
        if before.content_hash != request.content_hash {
            proposed.content_rev = revision();
        }
        proposed.content_hash = request.content_hash.clone();
        proposed.size = request.bytes.len() as u64;
        proposed.last_change_seq = self.state.next_change_seq().await?;
        let intent = Intent {
            operation_id: request.operation_id,
            before,
            proposed,
        };
        self.point(Phase::BeforeAdmission)?;
        // Admission commit errors can be ambiguous. Only recovery may reopen writes afterwards.
        self.ready = false;
        self.state
            .admit(&request, &fingerprint, &Outcome::Pending, Some(&intent))
            .await?;
        self.point(Phase::AfterAdmission)?;
        self.replace(
            &intent.before.name,
            &observed.stamp,
            &intent.proposed.content_hash,
        )?;
        self.point(Phase::BeforeFinish)?;
        let receipt = self.state.finish(&intent, true).await?;
        self.point(Phase::AfterFinish)?;
        self.ready = true;
        Ok(receipt)
    }

    async fn terminal(
        &mut self,
        request: &SaveContent,
        fingerprint: &str,
        outcome: Outcome,
    ) -> Result<Receipt, Error> {
        self.ready = false;
        self.state
            .admit(request, fingerprint, &outcome, None)
            .await?;
        self.ready = true;
        Ok(Receipt {
            operation_id: request.operation_id,
            outcome,
        })
    }

    async fn recover(&mut self) -> Result<(), Error> {
        self.ready = false;
        let pending = self.state.pending().await?;
        // One coordinator can leave at most one unresolved admission.
        if pending.len() > 1 {
            return Err(Error::InvalidFormat);
        }
        for intent in pending {
            if self.state.document(intent.before.node_id).await? != Some(intent.before.clone())
                || intent.before.node_id != intent.proposed.node_id
                || intent.before.parent_id != intent.proposed.parent_id
                || intent.before.name != intent.proposed.name
                || intent.before.meta_rev != intent.proposed.meta_rev
                || self.state.next_change_seq().await? != intent.proposed.last_change_seq
            {
                return Err(Error::InvalidFormat);
            }
            for document in [&intent.before, &intent.proposed] {
                let bytes = self.files.read_object(&document.content_hash)?;
                if bytes.len() as u64 != document.size {
                    return Err(Error::InvalidFormat);
                }
                self.put_object(&bytes)?;
            }
            // Once quarantined, only a separate explicit admin resolution can settle the receipt.
            if self.state.is_quarantined(&intent).await? {
                return Ok(());
            }
            let observed = match self.files.observe(&intent.before.name) {
                Ok(observed) => Some(observed),
                Err(StorageError::NotFound) => None,
                Err(error) => return Err(error.into()),
            };
            let decision = decide_content_recovery(
                &intent.before.content_hash,
                &intent.proposed.content_hash,
                observed.as_ref().map(|o| o.stamp.hash.as_str()),
            );
            match decision {
                RecoveryDecision::Conflict => {
                    let current_hash = if let Some(observed) = observed {
                        self.sync_observed(&intent.before.name, &observed.stamp)?;
                        Some(self.put_object(&observed.bytes)?)
                    } else {
                        None
                    };
                    self.state
                        .quarantine(&intent, current_hash.as_deref())
                        .await?;
                    return Ok(());
                }
                RecoveryDecision::Applied | RecoveryDecision::NotApplied => {
                    let observed = observed.ok_or(Error::InvalidFormat)?;
                    self.sync_observed(&intent.before.name, &observed.stamp)?;
                    self.point(Phase::BeforeRecoveryFinish)?;
                    self.state
                        .finish(&intent, decision == RecoveryDecision::Applied)
                        .await?;
                    self.point(Phase::AfterRecoveryFinish)?;
                }
            }
        }
        self.ready = true;
        Ok(())
    }

    fn point(&mut self, _phase: Phase) -> Result<(), Error> {
        #[cfg(test)]
        if let Some(hook) = &mut self.hook {
            return hook(_phase);
        }
        Ok(())
    }

    fn put_object(&mut self, bytes: &[u8]) -> Result<String, Error> {
        #[cfg(test)]
        return self
            .files
            .put_object_with_hook(bytes, |stage| {
                if let Some(hook) = &mut self.hook {
                    hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                }
                Ok(())
            })
            .map_err(Into::into);
        #[cfg(not(test))]
        self.files.put_object(bytes).map_err(Into::into)
    }
    fn replace(
        &mut self,
        name: &str,
        stamp: &notryx_server_storage::FileStamp,
        hash: &str,
    ) -> Result<(), Error> {
        #[cfg(test)]
        self.files.replace_with_hook(name, stamp, hash, |stage| {
            if let Some(hook) = &mut self.hook {
                hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
            }
            Ok(())
        })?;
        #[cfg(not(test))]
        self.files.replace(name, stamp, hash)?;
        Ok(())
    }
    fn sync_observed(
        &mut self,
        name: &str,
        stamp: &notryx_server_storage::FileStamp,
    ) -> Result<(), Error> {
        #[cfg(test)]
        self.files.sync_observed_with_hook(name, stamp, |stage| {
            if let Some(hook) = &mut self.hook {
                hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
            }
            Ok(())
        })?;
        #[cfg(not(test))]
        self.files.sync_observed(name, stamp)?;
        Ok(())
    }
    pub async fn close(self) -> Result<(), Error> {
        self.state.connection.close().await.map_err(state::db_error)
    }
}

#[cfg(test)]
mod tests;
