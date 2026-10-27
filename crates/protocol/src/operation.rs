use notryx_domain::{
    ChangeSeq, ContentRevision, MetaRevision, NodeId, OperationId, WorkspaceEpoch,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::http::uuid_v7_request_id;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum OperationReceipt {
    RecoveryConflict {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    Deleted {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        #[serde(deserialize_with = "crate::structure::deleted_node")]
        node: crate::NodeState,
        #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
        #[ts(type = "string")]
        commit_seq: ChangeSeq,
    },
    Restored {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        #[serde(deserialize_with = "crate::create::created_node")]
        node: crate::NodeState,
        #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
        #[ts(type = "string")]
        commit_seq: ChangeSeq,
    },
    NodePreconditionFailed {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        current_node_etag: String,
    },
    NodeDeleted {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    NotDeleted {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    FolderNotEmpty {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    HistoryExpired {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    Moved {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        #[serde(deserialize_with = "crate::create::created_node")]
        node: crate::NodeState,
        #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
        #[ts(type = "string")]
        commit_seq: ChangeSeq,
    },
    MetadataPreconditionFailed {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        #[schemars(with = "String", length(min = 1))]
        #[ts(type = "string")]
        current_meta_rev: MetaRevision,
        current_meta_etag: String,
    },
    ExpectedParentMismatch {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    CycleDetected {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    RootImmutable {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    Created {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        #[serde(deserialize_with = "crate::create::created_node")]
        node: crate::CreatedNode,
        #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
        #[ts(type = "string")]
        commit_seq: ChangeSeq,
    },
    Committed {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        node_id: NodeId,
        #[schemars(with = "String", length(min = 1))]
        #[ts(type = "string")]
        content_rev: ContentRevision,
        content_etag: String,
        #[schemars(with = "String", length(min = 1))]
        #[ts(type = "string")]
        meta_rev: MetaRevision,
        meta_etag: String,
        node_etag: String,
        content_hash: String,
        #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
        #[ts(type = "string")]
        commit_seq: ChangeSeq,
    },
    PreconditionFailed {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        #[schemars(with = "String", length(min = 1))]
        #[ts(type = "string")]
        current_content_rev: ContentRevision,
        current_content_etag: String,
    },
    DestinationExists {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    NodeIdConflict {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    ParentNotFolder {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    NotFound {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    NotApplied {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    Abandoned {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
    Pending {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(deserialize_with = "positive_operation_seq")]
        #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
        #[ts(type = "string")]
        operation_seq: ChangeSeq,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct OperationError {
    pub code: String,
    pub message: String,
    #[serde(deserialize_with = "uuid_v7_request_id")]
    #[schemars(extend("format" = "uuid"))]
    pub request_id: String,
    pub operation: OperationReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AckRequest {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    #[serde(deserialize_with = "uuid_v4_stream")]
    #[schemars(extend("format" = "uuid"))]
    pub device_stream_id: String,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub ack_through: ChangeSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AckResponse {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    #[serde(deserialize_with = "uuid_v4_stream")]
    #[schemars(extend("format" = "uuid"))]
    pub device_stream_id: String,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub last_seq: ChangeSeq,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub retired_through: ChangeSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AbandonRequest {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    #[serde(deserialize_with = "uuid_v4_stream")]
    #[schemars(extend("format" = "uuid"))]
    pub device_stream_id: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub operation_id: OperationId,
    #[serde(deserialize_with = "positive_operation_seq")]
    #[schemars(with = "String", regex(pattern = "^0*[1-9][0-9]*$"))]
    #[ts(type = "string")]
    pub operation_seq: ChangeSeq,
}

fn uuid_v4_stream<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    value
        .parse::<WorkspaceEpoch>()
        .map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn positive_operation_seq<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ChangeSeq, D::Error> {
    let value = ChangeSeq::deserialize(deserializer)?;
    if value.get() == 0 {
        return Err(serde::de::Error::custom("invalid operation sequence"));
    }
    Ok(value)
}
