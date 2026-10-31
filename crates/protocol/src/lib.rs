use std::fmt;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_domain::{ChangeSeq, ContentRevision, MetaRevision, NodeId, WorkspaceEpoch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

pub mod admin;
pub mod create;
pub mod http;
pub mod identity;
pub mod node;
pub mod operation;
pub mod structure;
pub mod theme;
pub mod upload;
pub use admin::{
    AdminRecoverySummary, AdminRequest, AdminResponse, AdminResult, AdminUnsupportedItem,
    JournalAccounting, UnsupportedItemReason,
};
pub use create::{CreateNodeRequest, CreatedNode};
pub use http::{
    ApiError, NodeKind, NodeMetadata, PairRequest, PairResponse, ServerInfo, WorkspaceRole,
};
pub use identity::{DeviceCurrentRole, DeviceIdentity, DeviceRenewal};
pub use node::{ChangesPage, InventoryPage, NodeState};
pub use operation::{AbandonRequest, AckRequest, AckResponse, OperationError, OperationReceipt};
pub use structure::{MoveNodeRequest, RestoreNodeRequest, TrashNode, TrashPage};
pub use theme::{
    ThemeBase, ThemeFont, ThemePackage, ThemePreferences, ThemeSelection, ThemeStatus, ThemeTokens,
};
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

pub fn content_hash(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

fn decimal_generation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    ChangeSeq::try_from(value.clone()).map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn optional_decimal_generation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(number) = &value {
        ChangeSeq::try_from(number.clone()).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn merge_status_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    let valid = !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
        && value.parse::<i64>().is_ok();
    if !valid {
        return Err(serde::de::Error::custom("invalid merge intent version"));
    }
    Ok(value)
}

fn merge_status_sequence<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = merge_status_version(deserializer)?;
    if value == "0" {
        return Err(serde::de::Error::custom("invalid merge operation sequence"));
    }
    Ok(value)
}

fn required_merge_operation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<WorkspaceContentConflictMergeOperation>, D::Error> {
    Option::<WorkspaceContentConflictMergeOperation>::deserialize(deserializer)
}

fn required_first_content_conflict<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<WorkspaceContentConflictRef>, D::Error> {
    Option::<WorkspaceContentConflictRef>::deserialize(deserializer)
}

fn required_merge_followup<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<WorkspaceContentConflictMergeFollowup>, D::Error> {
    Option::<WorkspaceContentConflictMergeFollowup>::deserialize(deserializer)
}

fn required_frozen_content_revision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

fn frozen_content_operation_status<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<WorkspaceOutboxStatus, D::Error> {
    let status = WorkspaceOutboxStatus::deserialize(deserializer)?;
    if matches!(
        status,
        WorkspaceOutboxStatus::Queued
            | WorkspaceOutboxStatus::Sent
            | WorkspaceOutboxStatus::Abandoning
    ) {
        Ok(status)
    } else {
        Err(serde::de::Error::custom("invalid frozen content status"))
    }
}

fn frozen_content_operation_status_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["queued", "sent", "abandoning"]
    })
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
pub struct WorkspaceRemoteRefreshRequest {
    pub renderer_session_id: String,
    pub document_id: String,
    pub edit_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRemoteRefreshCandidate {
    pub token: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRemoteRefreshCommitRequest {
    pub renderer_session_id: String,
    pub document_id: String,
    pub edit_session_id: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDirtyRemoteRequest {
    pub renderer_session_id: String,
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopySnapshotRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopySnapshot {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub local_hash: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub tombstone_seq: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyConfirmRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub local_hash: String,
    pub parent_id: String,
    pub name: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub tombstone_seq: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceDeletedDraftCopyConfirmStatus {
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyConfirmResult {
    pub status: WorkspaceDeletedDraftCopyConfirmStatus,
    pub node_id: String,
    pub create_operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyReconfirmRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub expected_create_operation_id: String,
    pub parent_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyStatusRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyBlockingFollowup {
    pub blocker_operation_id: String,
    pub followup_operation_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub followup_operation_seq: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub followup_generation: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub followup_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyStatus {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub local_hash: String,
    #[serde(deserialize_with = "optional_decimal_generation")]
    pub tombstone_seq: Option<String>,
    pub terminal_operation_id: Option<String>,
    pub parent_id: String,
    pub name: String,
    pub node_id: String,
    pub create_operation_id: String,
    pub create_status: WorkspaceContentConflictCopyCreateStatus,
    pub create_failure: Option<WorkspaceContentConflictCopyCreateFailure>,
    pub content_status: WorkspaceContentConflictCopyContentStatus,
    pub content_operation_id: Option<String>,
    pub blocking_followup: Option<WorkspaceDeletedDraftCopyBlockingFollowup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyFollowupAbandonRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub node_id: String,
    pub create_operation_id: String,
    pub blocker_operation_id: String,
    pub followup_operation_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub followup_operation_seq: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub followup_generation: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub followup_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyFollowupAbandonResult {
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyProgressRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub node_id: String,
    pub create_operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyProgressResult {
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyReopenRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub previous_edit_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletedDraftCopyReopenResponse {
    pub state: Option<WorkspaceDeletedDraftCopyStatus>,
    pub document: WorkspaceOpenedDocument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictSnapshot {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub operation_seq: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub base_revision: String,
    pub base_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub base_hash: String,
    pub local_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub local_hash: String,
    pub conflict_revision: String,
    pub remote_revision: String,
    pub remote_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub remote_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentInspectRequest {
    pub renderer_session_id: String,
    pub current_binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub operation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceFrozenContentApplyChoice {
    LatestDraft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentApplyRequest {
    pub renderer_session_id: String,
    pub inspected: WorkspaceFrozenContentInspection,
    pub choice: WorkspaceFrozenContentApplyChoice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentApplyResponse {
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentCrossEpochTarget {
    pub node_id: String,
    pub parent_id: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentCrossEpochInspection {
    pub inspected: WorkspaceFrozenContentInspection,
    pub current_target: WorkspaceFrozenContentCrossEpochTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentCrossEpochApplyRequest {
    pub renderer_session_id: String,
    pub inspected: WorkspaceFrozenContentCrossEpochInspection,
    pub same_node_is_not_identity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentDismissRequest {
    pub renderer_session_id: String,
    pub inspected: WorkspaceFrozenContentInspection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceFrozenContentRemoteEvidence {
    Deleted {
        #[serde(deserialize_with = "merge_status_sequence")]
        #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
        change_seq: String,
    },
    TooLarge {
        #[serde(deserialize_with = "merge_status_sequence")]
        #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
        change_seq: String,
        #[serde(deserialize_with = "merge_status_sequence")]
        #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
        size: String,
    },
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FrozenContentRemoteEvidenceFields {
    Deleted {
        #[serde(deserialize_with = "merge_status_sequence")]
        change_seq: String,
    },
    TooLarge {
        #[serde(deserialize_with = "merge_status_sequence")]
        change_seq: String,
        #[serde(deserialize_with = "merge_status_sequence")]
        size: String,
    },
    Unknown {},
}

impl<'de> Deserialize<'de> for WorkspaceFrozenContentRemoteEvidence {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match FrozenContentRemoteEvidenceFields::deserialize(deserializer)? {
            FrozenContentRemoteEvidenceFields::Deleted { change_seq } => {
                Ok(Self::Deleted { change_seq })
            }
            FrozenContentRemoteEvidenceFields::TooLarge { change_seq, size } => {
                if size.parse::<u64>().map_err(serde::de::Error::custom)? <= 25 * 1024 * 1024 {
                    return Err(serde::de::Error::custom("invalid oversized document"));
                }
                Ok(Self::TooLarge { change_seq, size })
            }
            FrozenContentRemoteEvidenceFields::Unknown {} => Ok(Self::Unknown),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentLocalInspectRequest {
    pub renderer_session_id: String,
    pub current_binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentLocalInspection {
    pub current_binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
    #[serde(deserialize_with = "frozen_content_operation_status")]
    #[schemars(schema_with = "frozen_content_operation_status_schema")]
    pub operation_status: WorkspaceOutboxStatus,
    pub abandon_state: WorkspaceAbandonState,
    #[serde(deserialize_with = "required_frozen_content_revision")]
    #[schemars(required, schema_with = "node::nullable_string_schema")]
    pub dependency: Option<String>,
    #[serde(deserialize_with = "required_frozen_content_revision")]
    #[schemars(required, schema_with = "node::nullable_string_schema")]
    pub expected_revision: Option<String>,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_generation: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub local_generation: String,
    pub outcome_unknown: bool,
    pub base_revision: String,
    pub operation_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub operation_hash: String,
    pub local_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub local_hash: String,
    pub base_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub base_hash: String,
    pub remote_evidence: WorkspaceFrozenContentRemoteEvidence,
    pub remote_index_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentLocalDismissRequest {
    pub renderer_session_id: String,
    pub inspected: WorkspaceFrozenContentLocalInspection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
#[schemars(transform = frozen_content_dismissed_state_schema)]
pub struct WorkspaceFrozenContentDismissed {
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
    #[serde(deserialize_with = "frozen_content_operation_status")]
    #[schemars(schema_with = "frozen_content_operation_status_schema")]
    pub operation_status: WorkspaceOutboxStatus,
    pub abandon_state: WorkspaceAbandonState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenContentDismissedFields {
    operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    operation_seq: String,
    #[serde(deserialize_with = "frozen_content_operation_status")]
    operation_status: WorkspaceOutboxStatus,
    abandon_state: WorkspaceAbandonState,
}

impl<'de> Deserialize<'de> for WorkspaceFrozenContentDismissed {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = FrozenContentDismissedFields::deserialize(deserializer)?;
        if !matches!(
            (value.operation_status, value.abandon_state),
            (
                WorkspaceOutboxStatus::Queued | WorkspaceOutboxStatus::Sent,
                WorkspaceAbandonState::None
            ) | (
                WorkspaceOutboxStatus::Abandoning,
                WorkspaceAbandonState::Requested | WorkspaceAbandonState::Sent
            )
        ) {
            return Err(serde::de::Error::custom(
                "invalid frozen content dismissal state",
            ));
        }
        Ok(Self {
            operation_id: value.operation_id,
            operation_seq: value.operation_seq,
            operation_status: value.operation_status,
            abandon_state: value.abandon_state,
        })
    }
}

fn frozen_content_dismissed_state_schema(schema: &mut schemars::Schema) {
    schema.insert("oneOf".to_owned(), serde_json::json!([
        {"properties": {"operation_status": {"enum": ["queued", "sent"]}, "abandon_state": {"const": "none"}}},
        {"properties": {"operation_status": {"const": "abandoning"}, "abandon_state": {"enum": ["requested", "sent"]}}}
    ]));
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentDecisionStatusRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub document_id: String,
    pub old_operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentDecision {
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
    pub status: WorkspaceOutboxStatus,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub approved_generation: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub approved_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
#[schemars(transform = reject_both_frozen_content_decisions_schema)]
pub struct WorkspaceFrozenContentDecisionStatusResponse {
    pub binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub document_id: String,
    pub old_operation_id: String,
    #[serde(deserialize_with = "required_frozen_content_decision")]
    #[schemars(required, schema_with = "nullable_frozen_content_decision_schema")]
    pub decision: Option<WorkspaceFrozenContentDecision>,
    #[serde(deserialize_with = "required_frozen_content_dismissed")]
    #[schemars(required, schema_with = "nullable_frozen_content_dismissed_schema")]
    pub dismissed: Option<WorkspaceFrozenContentDismissed>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenContentDecisionStatusResponseFields {
    binding: WorkspaceRegistration,
    old_binding: WorkspaceRegistration,
    document_id: String,
    old_operation_id: String,
    #[serde(deserialize_with = "required_frozen_content_decision")]
    decision: Option<WorkspaceFrozenContentDecision>,
    #[serde(deserialize_with = "required_frozen_content_dismissed")]
    dismissed: Option<WorkspaceFrozenContentDismissed>,
}

impl<'de> Deserialize<'de> for WorkspaceFrozenContentDecisionStatusResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = FrozenContentDecisionStatusResponseFields::deserialize(deserializer)?;
        if value.decision.is_some() && value.dismissed.is_some() {
            return Err(serde::de::Error::custom(
                "conflicting frozen content decisions",
            ));
        }
        Ok(Self {
            binding: value.binding,
            old_binding: value.old_binding,
            document_id: value.document_id,
            old_operation_id: value.old_operation_id,
            decision: value.decision,
            dismissed: value.dismissed,
        })
    }
}

fn reject_both_frozen_content_decisions_schema(schema: &mut schemars::Schema) {
    schema.insert(
        "not".to_owned(),
        serde_json::json!({
            "properties": {
                "decision": {"type": "object"},
                "dismissed": {"type": "object"}
            },
            "required": ["decision", "dismissed"]
        }),
    );
}

fn nullable_frozen_content_decision_schema(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    Option::<WorkspaceFrozenContentDecision>::json_schema(generator)
}

fn required_frozen_content_decision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<WorkspaceFrozenContentDecision>, D::Error> {
    Option::<WorkspaceFrozenContentDecision>::deserialize(deserializer)
}

fn nullable_frozen_content_dismissed_schema(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    Option::<WorkspaceFrozenContentDismissed>::json_schema(generator)
}

fn required_frozen_content_dismissed<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<WorkspaceFrozenContentDismissed>, D::Error> {
    Option::<WorkspaceFrozenContentDismissed>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenContentInspection {
    pub current_binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
    #[serde(deserialize_with = "frozen_content_operation_status")]
    #[schemars(schema_with = "frozen_content_operation_status_schema")]
    pub operation_status: WorkspaceOutboxStatus,
    pub abandon_state: WorkspaceAbandonState,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_generation: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub local_generation: String,
    #[serde(deserialize_with = "required_frozen_content_revision")]
    #[schemars(required, schema_with = "node::nullable_string_schema")]
    pub expected_revision: Option<String>,
    pub outcome_unknown: bool,
    pub base_revision: String,
    pub base_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub base_hash: String,
    pub operation_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub operation_hash: String,
    pub local_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub local_hash: String,
    pub remote_revision: String,
    pub remote_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub remote_hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenContentInspectionFields {
    current_binding: WorkspaceRegistration,
    old_binding: WorkspaceRegistration,
    document_id: String,
    operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    operation_seq: String,
    #[serde(deserialize_with = "frozen_content_operation_status")]
    operation_status: WorkspaceOutboxStatus,
    abandon_state: WorkspaceAbandonState,
    #[serde(deserialize_with = "merge_status_sequence")]
    operation_generation: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    local_generation: String,
    #[serde(deserialize_with = "required_frozen_content_revision")]
    expected_revision: Option<String>,
    outcome_unknown: bool,
    base_revision: String,
    base_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    base_hash: String,
    operation_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    operation_hash: String,
    local_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    local_hash: String,
    remote_revision: String,
    remote_bytes_base64: String,
    #[serde(deserialize_with = "upload::content_hash")]
    remote_hash: String,
}

impl<'de> Deserialize<'de> for WorkspaceFrozenContentInspection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = FrozenContentInspectionFields::deserialize(deserializer)?;
        if !matches!(
            (
                value.operation_status,
                value.abandon_state,
                value.outcome_unknown
            ),
            (
                WorkspaceOutboxStatus::Queued,
                WorkspaceAbandonState::None,
                false
            ) | (
                WorkspaceOutboxStatus::Sent,
                WorkspaceAbandonState::None,
                true
            ) | (
                WorkspaceOutboxStatus::Abandoning,
                WorkspaceAbandonState::Requested | WorkspaceAbandonState::Sent,
                false
            )
        ) {
            return Err(serde::de::Error::custom(
                "invalid frozen content operation state",
            ));
        }
        Ok(Self {
            current_binding: value.current_binding,
            old_binding: value.old_binding,
            document_id: value.document_id,
            operation_id: value.operation_id,
            operation_seq: value.operation_seq,
            operation_status: value.operation_status,
            abandon_state: value.abandon_state,
            operation_generation: value.operation_generation,
            local_generation: value.local_generation,
            expected_revision: value.expected_revision,
            outcome_unknown: value.outcome_unknown,
            base_revision: value.base_revision,
            base_bytes_base64: value.base_bytes_base64,
            base_hash: value.base_hash,
            operation_bytes_base64: value.operation_bytes_base64,
            operation_hash: value.operation_hash,
            local_bytes_base64: value.local_bytes_base64,
            local_hash: value.local_hash,
            remote_revision: value.remote_revision,
            remote_bytes_base64: value.remote_bytes_base64,
            remote_hash: value.remote_hash,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConflictResolutionRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceConflictChoice {
    KeepLocalCopy,
    ChooseRemote,
    SaveMerged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum WorkspaceConflictResolutionState {
    NotFound,
    Pending {
        #[serde(deserialize_with = "decimal_generation")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        version: String,
        #[serde(deserialize_with = "decimal_generation")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        generation: String,
        choice: WorkspaceConflictChoice,
        needs_review: bool,
        frozen: bool,
    },
    Completed {
        #[serde(deserialize_with = "decimal_generation")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        generation: String,
        frozen: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConflictResolutionResponse {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    pub state: WorkspaceConflictResolutionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictChooseRemoteRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub edit_session_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    #[serde(deserialize_with = "optional_decimal_generation")]
    pub expected_intent_version: Option<String>,
    pub base_revision: String,
    pub conflict_revision: String,
    pub remote_revision: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub base_hash: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub local_hash: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub remote_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum WorkspaceContentConflictChooseRemoteResponse {
    Pending {
        #[serde(deserialize_with = "decimal_generation")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        version: String,
    },
    Completed {
        #[serde(deserialize_with = "decimal_generation")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        generation: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictCopyRequest {
    pub selection: WorkspaceContentConflictChooseRemoteRequest,
    pub parent_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum WorkspaceContentConflictCopyResponse {
    Pending {
        #[serde(deserialize_with = "decimal_generation")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        version: String,
        node_id: String,
        create_operation_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum WorkspaceContentConflictCopyProgressResponse {
    ApprovalRequired {
        followup_operation_id: String,
    },
    Pending {
        requested_operation_id: Option<String>,
    },
    Saved {},
    NeedsReview {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictCopyProgressRequest {
    pub progress: WorkspaceContentConflictProgressRequest,
    pub approved_followup_operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictCopyRetryRequest {
    pub progress: WorkspaceContentConflictProgressRequest,
    pub expected_create_operation_id: String,
    pub parent_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceContentConflictCopyCreateStatus {
    NotQueued,
    Pending,
    Created,
    NeedsReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceContentConflictCopyCreateFailure {
    DestinationExists,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceContentConflictCopyContentStatus {
    NotQueued,
    Pending,
    Saved,
    NeedsReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictCopyStatus {
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub version: String,
    pub parent_id: String,
    pub name: String,
    pub node_id: String,
    pub create_operation_id: String,
    pub followup_operation_id: Option<String>,
    pub create_status: WorkspaceContentConflictCopyCreateStatus,
    pub create_failure: Option<WorkspaceContentConflictCopyCreateFailure>,
    pub content_status: WorkspaceContentConflictCopyContentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictCopyStatusResponse {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    pub copy: Option<WorkspaceContentConflictCopyStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictProgressRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub edit_session_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub expected_intent_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictMergeStatusRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_version")]
    #[schemars(regex(pattern = "^(0|[1-9][0-9]*)$"))]
    pub expected_intent_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictMergeOperation {
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictMergeStatusResponse {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_version")]
    #[schemars(regex(pattern = "^(0|[1-9][0-9]*)$"))]
    pub intent_version: String,
    #[serde(deserialize_with = "required_merge_operation")]
    pub merge_operation: Option<WorkspaceContentConflictMergeOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictMergeFollowup {
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub generation: String,
    #[serde(deserialize_with = "upload::content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub payload_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictMergeFollowupStatusResponse {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_version")]
    #[schemars(regex(pattern = "^(0|[1-9][0-9]*)$"))]
    pub intent_version: String,
    #[serde(deserialize_with = "required_merge_followup")]
    pub followup: Option<WorkspaceContentConflictMergeFollowup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictMergeFollowupApprovalRequest {
    pub progress: WorkspaceContentConflictProgressRequest,
    pub followup: WorkspaceContentConflictMergeFollowup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum WorkspaceContentConflictProgressResponse {
    Pending {
        #[serde(deserialize_with = "decimal_generation")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        version: String,
        requested_operation_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictReopenRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub operation_id: String,
    pub previous_edit_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictReopenResponse {
    pub state: WorkspaceConflictResolutionState,
    pub document: WorkspaceOpenedDocument,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceDirtyRemoteState {
    Same,
    Changed,
    Deleted,
    Pending,
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
    pub has_session: bool,
    pub cache_warning: Option<WorkspaceCacheWarning>,
    pub open_notice: Option<WorkspaceOpenNotice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceOpenNotice {
    PriorDraftPreserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceCacheWarning {
    Storage,
    Damaged,
    Unexpected,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceSearchScope {
    Workspace,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceSearchSource {
    Draft,
    Cache,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSearchRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub request_id: String,
    pub query: String,
    pub scope: WorkspaceSearchScope,
    pub after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSearchCancelRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSearchResult {
    pub node_id: String,
    pub node: Option<NodeState>,
    pub scope: WorkspaceSearchScope,
    pub source: WorkspaceSearchSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSearchPage {
    pub request_id: String,
    pub binding: WorkspaceRegistration,
    pub results: Vec<WorkspaceSearchResult>,
    pub available_documents: u32,
    pub total_documents: u32,
    pub recovery_available_documents: u32,
    pub recovery_total_documents: u32,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestoreQueueRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub request: RestoreNodeRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestoreAbandonRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestoreListRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub after_seq: String,
    pub limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceRestoreStatus {
    Queued,
    Sent,
    Abandoning,
    Abandoned,
    Committed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestoreEntry {
    pub operation_id: String,
    pub operation_seq: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub request: RestoreNodeRequest,
    pub status: WorkspaceRestoreStatus,
    pub receipt: Option<OperationReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestorePage {
    pub binding: WorkspaceRegistration,
    pub entries: Vec<WorkspaceRestoreEntry>,
    pub next_after_seq: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreateQueueRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub parent_id: NodeId,
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreateParentsRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreateParentsPage {
    pub binding: WorkspaceRegistration,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    pub nodes: Vec<NodeState>,
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub next_after_id: Option<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenCreateListRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub after_id: Option<String>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenCreateEntry {
    pub old_binding: WorkspaceRegistration,
    pub operation_id: String,
    pub operation_seq: String,
    pub request: CreateNodeRequest,
    pub status: WorkspaceCreateStatus,
    pub receipt: Option<OperationReceipt>,
    pub current_same_id: Option<NodeState>,
    pub current_same_name: Vec<NodeState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenCreatePage {
    pub binding: WorkspaceRegistration,
    pub entries: Vec<WorkspaceFrozenCreateEntry>,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceFrozenCreateChoice {
    Apply {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        parent_id: NodeId,
        name: String,
    },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenCreateDecisionRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub operation_id: String,
    pub choice: WorkspaceFrozenCreateChoice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreateAbandonRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreateListRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub after_seq: String,
    pub limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceCreateStatus {
    Queued,
    Sent,
    Abandoning,
    Abandoned,
    Committed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreateEntry {
    pub operation_id: String,
    pub operation_seq: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub request: CreateNodeRequest,
    pub status: WorkspaceCreateStatus,
    pub receipt: Option<OperationReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreatePage {
    pub binding: WorkspaceRegistration,
    pub entries: Vec<WorkspaceCreateEntry>,
    pub next_after_seq: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeleteQueueRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub expected_node_etag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeleteListRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub after_seq: String,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeleteAbandonRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub operation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceDeleteStatus {
    Queued,
    Sent,
    Abandoning,
    Abandoned,
    Committed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeleteEntry {
    pub operation_id: String,
    pub operation_seq: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub expected_node_etag: String,
    pub status: WorkspaceDeleteStatus,
    pub receipt: Option<OperationReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeletePage {
    pub binding: WorkspaceRegistration,
    pub entries: Vec<WorkspaceDeleteEntry>,
    pub next_after_seq: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMoveQueueRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub expected_meta_etag: String,
    pub request: MoveNodeRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMoveListRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub after_seq: String,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMoveAbandonRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub operation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceMoveStatus {
    Queued,
    Sent,
    Abandoning,
    Abandoned,
    Committed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMoveEntry {
    pub operation_id: String,
    pub operation_seq: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub expected_meta_etag: String,
    pub request: MoveNodeRequest,
    pub status: WorkspaceMoveStatus,
    pub receipt: Option<OperationReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMovePage {
    pub binding: WorkspaceRegistration,
    pub entries: Vec<WorkspaceMoveEntry>,
    pub next_after_seq: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenMoveListRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub after_id: Option<String>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenMoveEntry {
    pub old_binding: WorkspaceRegistration,
    pub operation_id: String,
    pub operation_seq: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub expected_meta_etag: String,
    pub request: MoveNodeRequest,
    pub status: WorkspaceMoveStatus,
    pub receipt: Option<OperationReceipt>,
    pub current_node: Option<NodeState>,
    pub current_destination: Option<NodeState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenMovePage {
    pub binding: WorkspaceRegistration,
    pub entries: Vec<WorkspaceFrozenMoveEntry>,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceFrozenMoveChoice {
    Apply {
        expected_meta_etag: String,
        request: MoveNodeRequest,
    },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenMoveDecisionRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub operation_id: String,
    pub choice: WorkspaceFrozenMoveChoice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenDeleteListRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub after_id: Option<String>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenDeleteEntry {
    pub old_binding: WorkspaceRegistration,
    pub operation_id: String,
    pub operation_seq: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    pub expected_node_etag: String,
    pub status: WorkspaceDeleteStatus,
    pub receipt: Option<OperationReceipt>,
    pub current_node: Option<NodeState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenDeletePage {
    pub binding: WorkspaceRegistration,
    pub entries: Vec<WorkspaceFrozenDeleteEntry>,
    pub next_after_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceFrozenDeleteChoice {
    Apply { expected_node_etag: String },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFrozenDeleteDecisionRequest {
    pub renderer_session_id: String,
    pub binding: WorkspaceRegistration,
    pub old_binding: WorkspaceRegistration,
    pub operation_id: String,
    pub choice: WorkspaceFrozenDeleteChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceSyncStatus {
    Connecting,
    Ready,
    Offline,
    AuthRequired,
    GatewayAuthRequired,
    NeedsReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceCredentialStatus {
    None,
    Secure,
    SessionOnly,
    RegistrationRequired,
    IdentityUnsupported,
    Unavailable,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCredentialState {
    pub status: WorkspaceCredentialStatus,
    pub renewal_status: WorkspaceCredentialRenewalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceCredentialRenewalStatus {
    None,
    Expiring,
    Expired,
    AuthRequired,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSyncState {
    pub binding: WorkspaceRegistration,
    pub status: WorkspaceSyncStatus,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum MigrationOperationKind {
    Content,
    Restore,
    Create,
    Delete,
    Move,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MigrationOperation {
    pub operation_id: String,
    pub document_id: String,
    pub binding: Option<WorkspaceRegistration>,
    pub sequence: Option<String>,
    pub generation: Option<String>,
    pub kind: Option<MigrationOperationKind>,
    pub expected_node_etag: Option<String>,
    pub expected_meta_etag: Option<String>,
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
    PermissionDenied,
    NeedsReview,
    SyncPending,
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
    pub content_block: Option<WorkspaceContentConflictBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContentConflictBlock {
    pub conflict_operation_id: String,
    pub blocked_operation_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub blocked_seq: String,
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
pub struct WorkspaceContentConflictRef {
    pub operation_id: String,
    #[serde(deserialize_with = "merge_status_sequence")]
    #[schemars(regex(pattern = "^[1-9][0-9]*$"))]
    pub operation_seq: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDocumentOutboxState {
    pub binding: WorkspaceRegistration,
    pub document_id: String,
    pub latest: Option<WorkspaceOutboxEntry>,
    #[serde(deserialize_with = "required_first_content_conflict")]
    pub first_content_conflict: Option<WorkspaceContentConflictRef>,
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
