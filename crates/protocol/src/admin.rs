use notryx_domain::{OperationId, WorkspaceEpoch, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{WorkspaceRole, http::decimal_u64};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "command", rename_all = "snake_case")]
pub enum AdminRequest {
    // Internally tagged unit variants ignore unknown fields even with deny_unknown_fields.
    Status {},
    RecoveryList {
        #[serde(deserialize_with = "crate::node::required_nullable")]
        #[schemars(schema_with = "crate::node::nullable_string_schema")]
        #[ts(type = "string | null")]
        after_operation_id: Option<OperationId>,
        #[serde(deserialize_with = "recovery_limit")]
        #[schemars(range(min = 1, max = 20))]
        limit: u16,
    },
    RecoveryInspect {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "recovery_hash"
        )]
        #[schemars(
            schema_with = "crate::node::nullable_string_schema",
            regex(pattern = "^[0-9a-f]{64}$")
        )]
        #[ts(type = "string | null", optional)]
        after_hash: Option<String>,
    },
    RecoveryResolve {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
    },
    RecoveryRelease {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
    },
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
    RecoveryList {
        entries: Vec<AdminRecoverySummary>,
        #[serde(deserialize_with = "crate::node::required_nullable")]
        #[schemars(schema_with = "crate::node::nullable_string_schema")]
        #[ts(type = "string | null")]
        next_after_operation_id: Option<OperationId>,
    },
    RecoveryDetail {
        summary: AdminRecoverySummary,
        paths: Vec<String>,
        objects: Vec<String>,
        objects_truncated: bool,
        #[serde(deserialize_with = "recovery_hash")]
        #[schemars(
            schema_with = "crate::node::nullable_string_schema",
            regex(pattern = "^[0-9a-f]{64}$")
        )]
        next_after_hash: Option<String>,
    },
    RecoveryResolved {
        #[schemars(with = "String")]
        #[ts(type = "string")]
        operation_id: OperationId,
        ready: bool,
    },
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
        #[serde(deserialize_with = "decimal_u64")]
        #[schemars(regex(pattern = "^[0-9]+$"))]
        expires_at: String,
    },
    Done {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AdminRecoverySummary {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub operation_id: OperationId,
    pub resolved: bool,
    #[serde(deserialize_with = "decimal_u64")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub retained_objects: String,
}

fn recovery_limit<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u16, D::Error> {
    let value = u16::deserialize(deserializer)?;
    if !(1..=20).contains(&value) {
        return Err(serde::de::Error::custom("invalid recovery page limit"));
    }
    Ok(value)
}

fn recovery_hash<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if value.as_ref().is_some_and(|hash| {
        hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(serde::de::Error::custom("invalid recovery hash cursor"));
    }
    Ok(value)
}
