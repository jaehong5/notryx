use notryx_domain::{ChangeSeq, MetaRevision, NodeId, WorkspaceEpoch, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkspaceRole {
    Reader,
    Editor,
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Folder,
}

// Credential-bearing DTOs deliberately omit Debug to avoid accidental secret logging.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    pub protocol: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_id: WorkspaceId,
    pub pairing_secret: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PairResponse {
    pub protocol: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_id: WorkspaceId,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    pub principal_id: String,
    pub device_id: String,
    pub device_stream_id: String,
    pub device_token: String,
    pub role: WorkspaceRole,
    #[serde(deserialize_with = "decimal_u64")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub idle_expires_at: String,
    #[serde(deserialize_with = "decimal_u64")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub absolute_expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ServerInfo {
    pub protocol: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_id: WorkspaceId,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub root_id: NodeId,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub head_seq: ChangeSeq,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub journal_floor_seq: ChangeSeq,
    pub ready: bool,
    #[ts(type = "number")]
    pub max_document_bytes: u64,
    #[ts(type = "number")]
    pub max_attachment_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct NodeMetadata {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    // An explicit null identifies the root; a missing parent is malformed metadata.
    #[serde(deserialize_with = "nullable_parent")]
    #[schemars(schema_with = "nullable_parent_schema")]
    #[ts(type = "string | null")]
    pub parent_id: Option<NodeId>,
    pub name: String,
    pub kind: NodeKind,
    pub deleted: bool,
    #[schemars(with = "String", length(min = 1))]
    #[ts(type = "string")]
    pub meta_rev: MetaRevision,
    pub meta_etag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(deserialize_with = "uuid_v7_request_id")]
    #[schemars(extend("format" = "uuid"))]
    pub request_id: String,
}

pub(crate) fn uuid_v7_request_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    value.parse::<NodeId>().map_err(serde::de::Error::custom)?;
    Ok(value)
}

pub(crate) fn decimal_u64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(serde::de::Error::custom("invalid decimal value"));
    }
    value
        .parse::<u64>()
        .map_err(|_| serde::de::Error::custom("invalid decimal value"))?;
    Ok(value)
}

fn nullable_parent<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NodeId>, D::Error> {
    Option::<NodeId>::deserialize(deserializer)
}

fn nullable_parent_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    Option::<String>::json_schema(generator)
}
