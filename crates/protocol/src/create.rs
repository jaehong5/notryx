use notryx_domain::NodeId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{NodeKind, NodeState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct CreateNodeRequest {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub node_id: NodeId,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub parent_id: NodeId,
    #[serde(deserialize_with = "node_name")]
    pub name: String,
    pub kind: NodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null", optional)]
    pub upload_id: Option<uuid::Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateFields {
    node_id: NodeId,
    parent_id: NodeId,
    #[serde(deserialize_with = "node_name")]
    name: String,
    kind: NodeKind,
    upload_id: Option<uuid::Uuid>,
}

impl<'de> Deserialize<'de> for CreateNodeRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = CreateFields::deserialize(deserializer)?;
        if value
            .upload_id
            .is_some_and(|id| value.kind == NodeKind::Folder || !crate::upload::valid_id(id))
        {
            return Err(serde::de::Error::custom("invalid create upload"));
        }
        Ok(Self {
            node_id: value.node_id,
            parent_id: value.parent_id,
            name: value.name,
            kind: value.kind,
            upload_id: value.upload_id,
        })
    }
}

pub(crate) fn node_name<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    notryx_domain::mutation::validate_name(&value).map_err(serde::de::Error::custom)?;
    Ok(value)
}

pub type CreatedNode = NodeState;

pub(crate) fn created_node<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<CreatedNode, D::Error> {
    let value = NodeState::deserialize(deserializer)?;
    if value.parent_id.is_none() || value.deleted {
        return Err(serde::de::Error::custom("invalid created node state"));
    }
    Ok(value)
}
