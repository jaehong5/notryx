use notryx_domain::{ChangeSeq, ContentRevision, MetaRevision, NodeId, WorkspaceEpoch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::NodeKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct NodeState {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(schema_with = "nullable_string_schema")]
    #[ts(type = "string | null")]
    pub parent_id: Option<NodeId>,
    pub name: String,
    pub kind: NodeKind,
    pub deleted: bool,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(schema_with = "nullable_string_schema")]
    #[ts(type = "string | null")]
    pub content_rev: Option<ContentRevision>,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(schema_with = "nullable_string_schema")]
    pub content_etag: Option<String>,
    #[schemars(with = "String", length(min = 1))]
    #[ts(type = "string")]
    pub meta_rev: MetaRevision,
    pub meta_etag: String,
    pub node_etag: String,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(schema_with = "nullable_string_schema")]
    pub content_hash: Option<String>,
    #[serde(deserialize_with = "nullable_decimal")]
    #[schemars(schema_with = "nullable_string_schema")]
    pub size: Option<String>,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub children_generation: ChangeSeq,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub last_change_seq: ChangeSeq,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeStateFields {
    node_id: NodeId,
    #[serde(deserialize_with = "required_nullable")]
    parent_id: Option<NodeId>,
    name: String,
    kind: NodeKind,
    deleted: bool,
    #[serde(deserialize_with = "required_nullable")]
    content_rev: Option<ContentRevision>,
    #[serde(deserialize_with = "required_nullable")]
    content_etag: Option<String>,
    meta_rev: MetaRevision,
    meta_etag: String,
    node_etag: String,
    #[serde(deserialize_with = "required_nullable")]
    content_hash: Option<String>,
    #[serde(deserialize_with = "nullable_decimal")]
    size: Option<String>,
    children_generation: ChangeSeq,
    last_change_seq: ChangeSeq,
}

impl<'de> Deserialize<'de> for NodeState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = NodeStateFields::deserialize(deserializer)?;
        let content = [&value.content_etag, &value.content_hash, &value.size];
        let valid_content = match value.kind {
            NodeKind::File => {
                value.content_rev.is_some()
                    && content.iter().all(|field| field.is_some())
                    && value.children_generation.get() == 0
            }
            NodeKind::Folder => {
                value.content_rev.is_none() && content.iter().all(|field| field.is_none())
            }
        };
        let valid_name = match value.parent_id {
            None => value.name.is_empty() && value.kind == NodeKind::Folder && !value.deleted,
            Some(_) => notryx_domain::mutation::validate_name(&value.name).is_ok(),
        };
        if !valid_content || !valid_name {
            return Err(serde::de::Error::custom("invalid node state"));
        }
        Ok(Self {
            node_id: value.node_id,
            parent_id: value.parent_id,
            name: value.name,
            kind: value.kind,
            deleted: value.deleted,
            content_rev: value.content_rev,
            content_etag: value.content_etag,
            meta_rev: value.meta_rev,
            meta_etag: value.meta_etag,
            node_etag: value.node_etag,
            content_hash: value.content_hash,
            size: value.size,
            children_generation: value.children_generation,
            last_change_seq: value.last_change_seq,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct InventoryPage {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    pub nodes: Vec<NodeState>,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(schema_with = "nullable_string_schema")]
    #[ts(type = "string | null")]
    pub next_after_id: Option<NodeId>,
}

// Schemars' required attribute removes Option nullability; schema_with keeps the key required.
pub(crate) fn nullable_string_schema(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    Option::<String>::json_schema(generator)
}

pub(crate) fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn nullable_decimal<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(serde::de::Error::custom("invalid decimal size"));
        }
        value.parse::<u64>().map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}
