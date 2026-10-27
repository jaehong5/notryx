use notryx_domain::{MetaRevision, NodeId, WorkspaceEpoch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MoveNodeRequest {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub expected_parent_id: NodeId,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub parent_id: NodeId,
    #[serde(deserialize_with = "crate::create::node_name")]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RestoreNodeRequest {
    #[serde(deserialize_with = "expected_meta_revision")]
    #[schemars(with = "String", length(min = 1, max = 256))]
    #[ts(type = "string")]
    pub expected_meta_rev: MetaRevision,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub parent_id: NodeId,
    #[serde(deserialize_with = "crate::create::node_name")]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TrashNode {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub parent_id: NodeId,
    #[serde(deserialize_with = "crate::create::node_name")]
    pub name: String,
    pub kind: crate::NodeKind,
    #[schemars(with = "String", length(min = 1))]
    #[ts(type = "string")]
    pub meta_rev: MetaRevision,
    pub restorable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TrashPage {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    pub nodes: Vec<TrashNode>,
    #[serde(deserialize_with = "nullable_cursor")]
    #[schemars(schema_with = "crate::node::nullable_string_schema")]
    #[ts(type = "string | null")]
    pub next_after_id: Option<NodeId>,
}

fn expected_meta_revision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<MetaRevision, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.len() > 256 {
        return Err(serde::de::Error::custom(
            "metadata revision exceeds byte limit",
        ));
    }
    MetaRevision::new(value).map_err(serde::de::Error::custom)
}

fn nullable_cursor<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NodeId>, D::Error> {
    Option::<NodeId>::deserialize(deserializer)
}

pub(crate) fn deleted_node<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<crate::NodeState, D::Error> {
    let value = crate::NodeState::deserialize(deserializer)?;
    if value.parent_id.is_none() || !value.deleted {
        return Err(serde::de::Error::custom("invalid deleted node state"));
    }
    Ok(value)
}
