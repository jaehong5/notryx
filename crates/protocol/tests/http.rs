use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, ApiError, NodeKind, NodeMetadata, PairRequest,
    PairResponse, ServerInfo, WorkspaceRole,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";
const NODE: &str = "019a1000-0000-7000-8000-000000000001";

fn pair_request() -> Value {
    json!({
        "protocol": "mdw-sync/1.2",
        "workspace_id": WORKSPACE,
        "pairing_secret": "synthetic-pairing-secret"
    })
}

fn pair_response() -> Value {
    json!({
        "protocol": "mdw-sync/1.2",
        "workspace_id": WORKSPACE,
        "workspace_epoch": WORKSPACE,
        "principal_id": "synthetic-principal",
        "device_id": "synthetic-device",
        "device_stream_id": "synthetic-stream",
        "device_token": "synthetic-device-token",
        "role": "reader",
        "idle_expires_at": "1780000000",
        "absolute_expires_at": "1811536000"
    })
}

fn server_info() -> Value {
    json!({
        "protocol": "mdw-sync/1.2",
        "workspace_id": WORKSPACE,
        "workspace_epoch": WORKSPACE,
        "root_id": NODE,
        "head_seq": "9007199254740993",
        "journal_floor_seq": "0",
        "ready": true,
        "max_document_bytes": 5242880,
        "max_attachment_bytes": 26214400
    })
}

fn node_metadata() -> Value {
    json!({
        "node_id": NODE,
        "parent_id": null,
        "name": "",
        "kind": "folder",
        "deleted": false,
        "meta_rev": "opaque-meta-1",
        "meta_etag": "\"opaque-meta-1\""
    })
}

fn api_error() -> Value {
    json!({
        "code": "unauthorized",
        "message": "기기 인증이 필요합니다.",
        "request_id": NODE,
    })
}

fn assert_roundtrip<T: DeserializeOwned + Serialize>(value: Value) {
    let parsed = serde_json::from_value::<T>(value.clone()).ok().unwrap();
    let serialized = serde_json::to_value(parsed).ok().unwrap();
    // Boolean comparison keeps credential-bearing fixtures out of failure output.
    assert!(serialized == value);
}

fn assert_unknown_field_rejected<T: DeserializeOwned>(mut value: Value) {
    value["unexpected"] = json!(true);
    assert!(serde_json::from_value::<T>(value).is_err());
}

#[test]
fn http_dtos_roundtrip_the_declared_wire_fields() {
    assert_roundtrip::<PairRequest>(pair_request());
    assert_roundtrip::<PairResponse>(pair_response());
    assert_roundtrip::<ServerInfo>(server_info());
    assert_roundtrip::<NodeMetadata>(node_metadata());
    assert_roundtrip::<ApiError>(api_error());
}

#[test]
fn roles_and_node_kinds_use_closed_lowercase_wire_values() {
    for role in ["reader", "editor", "owner"] {
        assert_roundtrip::<WorkspaceRole>(json!(role));
    }
    for kind in ["file", "folder"] {
        assert_roundtrip::<NodeKind>(json!(kind));
    }
    for invalid in ["Reader", "admin", "", "OWNER"] {
        assert!(serde_json::from_value::<WorkspaceRole>(json!(invalid)).is_err());
    }
    for invalid in ["File", "directory", "symlink", ""] {
        assert!(serde_json::from_value::<NodeKind>(json!(invalid)).is_err());
    }
}

#[test]
fn all_http_dtos_reject_undeclared_fields() {
    assert_unknown_field_rejected::<PairRequest>(pair_request());
    assert_unknown_field_rejected::<PairResponse>(pair_response());
    assert_unknown_field_rejected::<ServerInfo>(server_info());
    assert_unknown_field_rejected::<NodeMetadata>(node_metadata());
    assert_unknown_field_rejected::<ApiError>(api_error());
}

#[test]
fn identifiers_keep_domain_uuid_version_and_variant_validation() {
    let mut request = pair_request();
    request["workspace_id"] = json!(NODE);
    assert!(serde_json::from_value::<PairRequest>(request).is_err());

    for field in ["workspace_id", "workspace_epoch"] {
        let mut response = pair_response();
        response[field] = json!(NODE);
        assert!(serde_json::from_value::<PairResponse>(response).is_err());
        let mut response = pair_response();
        response[field] = json!("550e8400-e29b-41d4-0716-446655440000");
        assert!(serde_json::from_value::<PairResponse>(response).is_err());
    }
    let mut info = server_info();
    info["root_id"] = json!(WORKSPACE);
    assert!(serde_json::from_value::<ServerInfo>(info).is_err());
    for field in ["node_id", "parent_id"] {
        let mut metadata = node_metadata();
        metadata[field] = json!(WORKSPACE);
        assert!(serde_json::from_value::<NodeMetadata>(metadata).is_err());
    }
}

#[test]
fn expiry_is_an_unsigned_decimal_seconds_string() {
    for field in ["idle_expires_at", "absolute_expires_at"] {
        for valid in ["0", "9007199254740993", "18446744073709551615"] {
            let mut response = pair_response();
            response[field] = json!(valid);
            assert!(serde_json::from_value::<PairResponse>(response).is_ok());
        }
        for invalid in [
            json!(0),
            json!(-1),
            json!(null),
            json!(""),
            json!("-1"),
            json!("+1"),
            json!("1.1"),
            json!("1e2"),
            json!(" 1"),
            json!("1\n"),
            json!("１２"),
            json!("18446744073709551616"),
        ] {
            let mut response = pair_response();
            response[field] = invalid;
            assert!(serde_json::from_value::<PairResponse>(response).is_err());
        }
    }
}

#[test]
fn server_info_sequences_preserve_integer_precision() {
    let info = serde_json::from_value::<ServerInfo>(server_info()).unwrap();
    assert_eq!(info.head_seq.get(), 9_007_199_254_740_993);
    for field in ["head_seq", "journal_floor_seq"] {
        for invalid in [json!(1), json!("-1"), json!("18446744073709551616")] {
            let mut value = server_info();
            value[field] = invalid;
            assert!(serde_json::from_value::<ServerInfo>(value).is_err());
        }
    }
    let mut value = server_info();
    value["max_document_bytes"] = json!("5242880");
    assert!(serde_json::from_value::<ServerInfo>(value).is_err());
}

#[test]
fn node_metadata_requires_parent_presence_and_nonempty_revision() {
    let mut metadata = node_metadata();
    metadata.as_object_mut().unwrap().remove("parent_id");
    assert!(serde_json::from_value::<NodeMetadata>(metadata).is_err());
    let mut metadata = node_metadata();
    metadata["meta_rev"] = json!("");
    assert!(serde_json::from_value::<NodeMetadata>(metadata).is_err());
}

#[test]
fn generated_contracts_keep_decimal_strings_nullable_parent_and_numeric_limit() {
    let response = serde_json::to_value(schemars::schema_for!(PairResponse)).unwrap();
    for field in ["idle_expires_at", "absolute_expires_at"] {
        assert_eq!(response["properties"][field]["type"], "string");
        assert_eq!(response["properties"][field]["pattern"], "^[0-9]+$");
    }
    let metadata = serde_json::to_value(schemars::schema_for!(NodeMetadata)).unwrap();
    assert_eq!(
        metadata["properties"]["parent_id"]["type"],
        json!(["string", "null"])
    );
    assert!(
        metadata["required"]
            .as_array()
            .unwrap()
            .contains(&json!("parent_id"))
    );
    let info = serde_json::to_value(schemars::schema_for!(ServerInfo)).unwrap();
    assert_eq!(info["properties"]["max_document_bytes"]["type"], "integer");
    assert!(ServerInfo::decl(&Config::default()).contains("max_document_bytes: number"));
    assert_eq!(
        WorkspaceRole::decl(&Config::default()),
        "type WorkspaceRole = \"reader\" | \"editor\" | \"owner\";"
    );
    assert_eq!(
        NodeKind::decl(&Config::default()),
        "type NodeKind = \"file\" | \"folder\";"
    );
}

#[test]
fn local_admin_contracts_reject_unknown_fields_and_invalid_expiry() {
    for request in [
        json!({"command": "status"}),
        json!({"command": "create_principal", "role": "reader"}),
        json!({"command": "set_role", "principal_id": "synthetic-principal", "role": "editor"}),
        json!({"command": "transfer_owner", "principal_id": "synthetic-principal", "previous_role": "reader"}),
        json!({"command": "issue_pairing", "principal_id": "synthetic-principal"}),
        json!({"command": "revoke_device", "device_id": "synthetic-device"}),
    ] {
        assert_roundtrip::<AdminRequest>(request.clone());
        assert_unknown_field_rejected::<AdminRequest>(request);
    }
    for result in [
        json!({"kind": "status", "workspace_id": WORKSPACE, "workspace_epoch": WORKSPACE, "owner_principal_id": "synthetic-principal"}),
        json!({"kind": "principal", "principal_id": "synthetic-principal"}),
        json!({"kind": "pairing", "workspace_id": WORKSPACE, "principal_id": "synthetic-principal", "pairing_secret": "synthetic-secret", "expires_at": "1780000000"}),
        json!({"kind": "done"}),
    ] {
        assert_roundtrip::<AdminResult>(result.clone());
        assert_roundtrip::<AdminResponse>(json!({"status": "ok", "result": result.clone()}));
        assert_unknown_field_rejected::<AdminResult>(result);
    }
    assert_roundtrip::<AdminResponse>(json!({"status": "error", "code": "forbidden"}));
    assert_unknown_field_rejected::<AdminResponse>(
        json!({"status": "ok", "result": {"kind": "done"}}),
    );
    assert_unknown_field_rejected::<AdminResponse>(json!({"status": "error", "code": "forbidden"}));
    for expires_at in [json!(1), json!("-1"), json!("18446744073709551616")] {
        assert!(
            serde_json::from_value::<AdminResult>(json!({
                "kind": "pairing", "workspace_id": WORKSPACE, "principal_id": "synthetic-principal",
                "pairing_secret": "synthetic-secret", "expires_at": expires_at
            }))
            .is_err()
        );
    }
}

#[test]
fn api_errors_require_a_message_and_uuid_v7_request_id() {
    assert!(serde_json::from_value::<ApiError>(json!({"code": "unauthorized"})).is_err());
    let valid = api_error();
    assert_roundtrip::<ApiError>(valid.clone());
    for field in ["message", "request_id"] {
        let mut value = valid.clone();
        value.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<ApiError>(value).is_err());
    }
    for invalid in [
        WORKSPACE,
        "not-a-uuid",
        "019a1000-0000-7000-0000-000000000001",
    ] {
        let mut value = valid.clone();
        value["request_id"] = json!(invalid);
        assert!(serde_json::from_value::<ApiError>(value).is_err());
    }
    let schema = serde_json::to_value(schemars::schema_for!(ApiError)).unwrap();
    assert_eq!(schema["properties"]["request_id"]["type"], "string");
    assert_eq!(schema["properties"]["request_id"]["format"], "uuid");
}
