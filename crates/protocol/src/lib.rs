use std::fmt;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_domain::{ChangeSeq, ContentRevision, MetaRevision, WorkspaceEpoch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

pub mod admin;
pub mod create;
pub mod http;
pub mod node;
pub mod operation;
pub mod structure;
pub mod upload;
pub use admin::{AdminRecoverySummary, AdminRequest, AdminResponse, AdminResult};
pub use create::{CreateNodeRequest, CreatedNode};
pub use http::{
    ApiError, NodeKind, NodeMetadata, PairRequest, PairResponse, ServerInfo, WorkspaceRole,
};
pub use node::{InventoryPage, NodeState};
pub use operation::{AbandonRequest, AckRequest, AckResponse, OperationError, OperationReceipt};
pub use structure::{MoveNodeRequest, RestoreNodeRequest, TrashNode, TrashPage};
pub use upload::UploadResponse;

pub const CONTRACT: &str = "mdw-sync/1.2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidContract {
    Protocol,
    Etag,
    Digest,
}

impl fmt::Display for InvalidContract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Protocol => "unsupported protocol contract",
            Self::Etag => "invalid strong ETag",
            Self::Digest => "invalid content digest",
        })
    }
}

impl std::error::Error for InvalidContract {}

pub fn require_contract(value: &str) -> Result<(), InvalidContract> {
    if value == CONTRACT {
        Ok(())
    } else {
        Err(InvalidContract::Protocol)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrongEtag(String);

impl StrongEtag {
    pub fn parse(value: &str) -> Result<Self, InvalidContract> {
        let inner = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(InvalidContract::Etag)?;
        if !inner
            .bytes()
            .all(|byte| byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80)
        {
            return Err(InvalidContract::Etag);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_content_revision(revision: &ContentRevision) -> Result<Self, InvalidContract> {
        Self::parse(&format!("\"{}\"", revision.as_str()))
    }

    pub fn from_meta_revision(revision: &MetaRevision) -> Result<Self, InvalidContract> {
        Self::parse(&format!("\"{}\"", revision.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentPrecondition(StrongEtag);

impl ContentPrecondition {
    pub fn parse(value: &str) -> Result<Self, InvalidContract> {
        StrongEtag::parse(value).map(Self)
    }

    pub fn etag(&self) -> &StrongEtag {
        &self.0
    }

    pub fn revision(&self) -> Result<ContentRevision, InvalidContract> {
        let value = self
            .0
            .as_str()
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(InvalidContract::Etag)?;
        if value.len() > 256 {
            return Err(InvalidContract::Etag);
        }
        ContentRevision::new(value).map_err(|_| InvalidContract::Etag)
    }

    pub fn matches(&self, current: &ContentRevision) -> Result<bool, InvalidContract> {
        Ok(self.0 == StrongEtag::from_content_revision(current)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPrecondition(StrongEtag);

impl MetadataPrecondition {
    pub fn parse(value: &str) -> Result<Self, InvalidContract> {
        StrongEtag::parse(value).map(Self)
    }

    pub fn etag(&self) -> &StrongEtag {
        &self.0
    }

    pub fn revision(&self) -> Result<MetaRevision, InvalidContract> {
        let value = self
            .0
            .as_str()
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(InvalidContract::Etag)?;
        if value.len() > 256 {
            return Err(InvalidContract::Etag);
        }
        MetaRevision::new(value).map_err(|_| InvalidContract::Etag)
    }

    pub fn matches(&self, current: &MetaRevision) -> Result<bool, InvalidContract> {
        Ok(self.0 == StrongEtag::from_meta_revision(current)?)
    }
}

pub fn verify_content_digest(header: &str, bytes: &[u8]) -> Result<(), InvalidContract> {
    let encoded = header
        .strip_prefix("sha-256=:")
        .and_then(|value| value.strip_suffix(':'))
        .ok_or(InvalidContract::Digest)?;
    let expected = STANDARD
        .decode(encoded)
        .map_err(|_| InvalidContract::Digest)?;
    if expected.as_slice() != &Sha256::digest(bytes)[..] {
        return Err(InvalidContract::Digest);
    }
    Ok(())
}

fn decimal_generation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    ChangeSeq::try_from(value.clone()).map_err(serde::de::Error::custom)?;
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HeadHint {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub head_seq: ChangeSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0OpenRequest {
    pub document_id: String,
    pub renderer_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0CloseRequest {
    pub document_id: String,
    pub edit_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0OpenedDocument {
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0SaveRequest {
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0SavedCheckpoint {
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePairRequest {
    pub renderer_session_id: String,
    pub origin: String,
    pub workspace_id: String,
    pub pairing_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceInventoryRequest {
    pub renderer_session_id: String,
    pub after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOpenRequest {
    pub renderer_session_id: String,
    pub node: NodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOpenedDocument {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub bytes_base64: String,
    pub readonly: bool,
    pub local_draft: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRegistration {
    pub server_origin: String,
    pub workspace_id: String,
    pub workspace_epoch: String,
    pub device_id: String,
    pub device_stream_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDraft {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub current_registration: bool,
    pub has_session: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspacePairStatus {
    None,
    InProgress,
    PendingAdoption,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLocalRequest {
    pub renderer_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDraftRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePreviewImageRequest {
    pub renderer_session_id: String,
    pub document_id: String,
    pub edit_session_id: String,
    pub node: NodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePreviewImage {
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceQueueRequest {
    pub renderer_session_id: String,
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub expected_generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAbandonRequest {
    pub renderer_session_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceQueuedOperation {
    pub operation_id: String,
    pub operation_seq: String,
    pub document_id: String,
    pub generation: String,
    pub committed: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExternalLinkRequest {
    pub renderer_session_id: String,
    pub document_id: String,
    pub edit_session_id: String,
    pub url: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRecoveryRequest {
    pub renderer_session_id: String,
    pub document_id: String,
    pub edit_session_id: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ClientStartupMode {
    Ready,
    Recovery,
    StoreLocked,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ClientStartupStatus {
    pub mode: ClientStartupMode,
    pub renderer_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MigrationDraft {
    pub storage_id: String,
    pub document_id: String,
    pub binding: Option<WorkspaceRegistration>,
    pub registration_status: Option<String>,
    pub generation: Option<String>,
    pub has_checkpoint: bool,
    pub has_base: bool,
    pub metadata_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MigrationOperation {
    pub operation_id: String,
    pub document_id: String,
    pub binding: Option<WorkspaceRegistration>,
    pub sequence: Option<String>,
    pub generation: Option<String>,
    pub status: String,
    pub terminal_kind: Option<String>,
    pub registration_status: Option<String>,
    pub metadata_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MigrationInventory {
    pub drafts: Vec<MigrationDraft>,
    pub operations: Vec<MigrationOperation>,
    pub next_draft_after_id: Option<String>,
    pub next_operation_after_id: Option<String>,
    pub draft_error: Option<String>,
    pub operation_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MigrationInventoryRequest {
    pub renderer_session_id: String,
    pub draft_after_id: Option<String>,
    pub operation_after_id: Option<String>,
    pub include_drafts: bool,
    pub include_operations: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum MigrationSourceKind {
    Store,
    Draft,
    Base,
    Outbox,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MigrationSourceRequest {
    pub renderer_session_id: String,
    pub kind: MigrationSourceKind,
    pub id: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MigrationSource {
    pub bytes_base64: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceSendStatus {
    Idle,
    Pending,
    Committed,
    Conflict,
    Failed,
    Abandoned,
    NeedsReview,
    AckPending,
    AuthRequired,
    GatewayAuthRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSendResult {
    pub binding: WorkspaceRegistration,
    pub status: WorkspaceSendStatus,
    pub operation_id: Option<String>,
    pub document_id: Option<String>,
    pub generation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutboxRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub after_seq: String,
    pub limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceOutboxStatus {
    Queued,
    Sent,
    Committed,
    Conflict,
    Abandoning,
    Abandoned,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceAbandonState {
    None,
    Requested,
    Sent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceOutboxFailure {
    NodeDeleted,
    RecoveryConflict,
    NotFound,
    NotApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutboxEntry {
    pub operation_id: String,
    pub operation_seq: String,
    pub document_id: String,
    pub generation: String,
    pub status: WorkspaceOutboxStatus,
    pub frozen: bool,
    pub abandon_state: WorkspaceAbandonState,
    pub failure: Option<WorkspaceOutboxFailure>,
    pub payload_retired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDocumentOutboxState {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub latest: Option<WorkspaceOutboxEntry>,
    pub has_conflict: bool,
    pub has_failure: bool,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub confirmed_through: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutboxPage {
    pub entries: Vec<WorkspaceOutboxEntry>,
    pub next_after_seq: Option<String>,
}
