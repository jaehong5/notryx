use crate::WorkspaceRole;
use notryx_domain::{WorkspaceEpoch, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DeviceIdentity {
    pub protocol: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_id: WorkspaceId,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    pub device_id: String,
    pub device_stream_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DeviceCurrentRole {
    pub protocol: String,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_id: WorkspaceId,
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    pub device_id: String,
    pub device_stream_id: String,
    pub role: WorkspaceRole,
}
