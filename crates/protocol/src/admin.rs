use notryx_domain::{WorkspaceEpoch, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{WorkspaceRole, http::decimal_seconds};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "command", rename_all = "snake_case")]
pub enum AdminRequest {
    // Internally tagged unit variants ignore unknown fields even with deny_unknown_fields.
    Status {},
    CreatePrincipal {
        role: WorkspaceRole,
    },
    SetRole {
        principal_id: String,
        role: WorkspaceRole,
    },
    TransferOwner {
        principal_id: String,
        previous_role: WorkspaceRole,
    },
    IssuePairing {
        principal_id: String,
    },
    RevokeDevice {
        device_id: String,
    },
}

// Pairing results contain credentials and must not be included in debug output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum AdminResponse {
    Ok { result: AdminResult },
    Error { code: String },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "kind", rename_all = "snake_case")]
pub enum AdminResult {
    Status {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_id: WorkspaceId,
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_epoch: WorkspaceEpoch,
        owner_principal_id: String,
    },
    Principal {
        principal_id: String,
    },
    Pairing {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        workspace_id: WorkspaceId,
        principal_id: String,
        pairing_secret: String,
        #[serde(deserialize_with = "decimal_seconds")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        expires_at: String,
    },
    Done {},
}
