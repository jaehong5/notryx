use notryx_protocol::{
    WorkspaceContentConflictChooseRemoteResponse, WorkspaceContentConflictProgressRequest,
    WorkspaceContentConflictReopenRequest, WorkspaceContentConflictReopenResponse,
};
use serde_json::{Value, json};

fn binding() -> Value {
    json!({
        "server_origin":"https://workspace.example",
        "workspace_id":"019a1000-0000-7000-8000-000000000001",
        "workspace_epoch":"019a1000-0000-7000-8000-000000000002",
        "device_id":"019a1000-0000-7000-8000-000000000003",
        "device_stream_id":"019a1000-0000-7000-8000-000000000004"
    })
}

fn progress() -> Value {
    json!({
        "renderer_session_id":"owner", "binding":binding(),
        "document_id":"019a1000-0000-7000-8000-000000000005",
        "edit_session_id":"edit-session", "operation_id":"019a1000-0000-7000-8000-000000000006",
        "expected_intent_version":"0"
    })
}

fn reopen() -> Value {
    json!({
        "renderer_session_id":"owner", "binding":binding(),
        "document_id":"019a1000-0000-7000-8000-000000000005",
        "operation_id":"019a1000-0000-7000-8000-000000000006",
        "previous_edit_session_id":"previous-session"
    })
}

#[test]
fn progress_request_roundtrips_identity_and_validates_decimal_version() {
    let valid = progress();
    let parsed: WorkspaceContentConflictProgressRequest =
        serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), valid);
    for version in [json!("0"), json!("1"), json!("18446744073709551615")] {
        let mut value = valid.clone();
        value["expected_intent_version"] = version;
        assert!(serde_json::from_value::<WorkspaceContentConflictProgressRequest>(value).is_ok());
    }
    for version in [
        json!(null),
        json!(0),
        json!(true),
        json!(""),
        json!("-1"),
        json!("+1"),
        json!("1.5"),
        json!("18446744073709551616"),
    ] {
        let mut value = valid.clone();
        value["expected_intent_version"] = version;
        assert!(serde_json::from_value::<WorkspaceContentConflictProgressRequest>(value).is_err());
    }
    for field in [
        "renderer_session_id",
        "binding",
        "document_id",
        "edit_session_id",
        "operation_id",
        "expected_intent_version",
    ] {
        let mut value = valid.clone();
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictProgressRequest>(value).is_err(),
            "missing {field}"
        );
    }
    let mut value = valid.clone();
    value["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictProgressRequest>(value).is_err());
    let mut value = valid;
    value["binding"]["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictProgressRequest>(value).is_err());
}

#[test]
fn reopen_request_roundtrips_binding_document_operation_and_previous_session() {
    let valid = reopen();
    let parsed: WorkspaceContentConflictReopenRequest =
        serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), valid);
    for field in [
        "renderer_session_id",
        "binding",
        "document_id",
        "operation_id",
        "previous_edit_session_id",
    ] {
        let mut value = valid.clone();
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictReopenRequest>(value).is_err(),
            "missing {field}"
        );
    }
    let mut value = valid.clone();
    value["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictReopenRequest>(value).is_err());
    let mut value = valid;
    value["binding"]["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictReopenRequest>(value).is_err());
}

#[test]
fn reopen_response_preserves_resolution_and_document_identity() {
    for state in [
        json!({"status":"not_found"}),
        json!({"status":"pending","version":"0","generation":"2","choice":"choose_remote","needs_review":false,"frozen":false}),
        json!({"status":"completed","generation":"3","frozen":false}),
    ] {
        let value = json!({"state":state,"document":{
            "binding":binding(),"document_id":"019a1000-0000-7000-8000-000000000005",
            "edit_session_id":"fresh-session","generation":"3","bytes_base64":"YQ==",
            "readonly":false,"local_draft":true,"has_session":true,"cache_warning":null,"open_notice":null
        }});
        let parsed: WorkspaceContentConflictReopenResponse =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
        let mut extra = value.clone();
        extra["extra"] = json!(true);
        assert!(serde_json::from_value::<WorkspaceContentConflictReopenResponse>(extra).is_err());
        let mut invalid = value;
        invalid["document"]["generation"] = json!("-1");
        assert!(serde_json::from_value::<WorkspaceContentConflictReopenResponse>(invalid).is_err());
    }
    for value in [
        json!({"status":"pending","version":"0"}),
        json!({"status":"completed","generation":"3"}),
    ] {
        assert!(
            serde_json::from_value::<WorkspaceContentConflictChooseRemoteResponse>(value).is_ok()
        );
    }
}

#[test]
fn three_commands_are_registered_and_limited_to_main_capability() {
    let manifest = include_str!("../build.rs");
    let handler = include_str!("../src/lib.rs");
    let capability: Value =
        serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
    assert_eq!(capability["windows"], json!(["main"]));
    for (command, permission) in [
        (
            "workspace_continue_remote_content_conflict",
            "allow-workspace-continue-remote-content-conflict",
        ),
        (
            "workspace_complete_pending_remote_content_conflict",
            "allow-workspace-complete-pending-remote-content-conflict",
        ),
        (
            "workspace_reopen_content_conflict_checkpoint",
            "allow-workspace-reopen-content-conflict-checkpoint",
        ),
    ] {
        assert!(
            manifest.contains(&format!("\"{command}\"")),
            "manifest: {command}"
        );
        assert!(
            handler.contains(&format!("conflict_snapshot_native::{command}")),
            "handler: {command}"
        );
        assert!(
            capability["permissions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry == permission),
            "capability: {command}"
        );
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("permissions/autogenerated")
            .join(format!("{command}.toml"));
        let generated = std::fs::read_to_string(file).unwrap();
        assert!(generated.contains(&format!("identifier = \"{permission}\"")));
        assert!(generated.contains(&format!("commands.allow = [\"{command}\"]")));
    }
}

#[test]
fn progress_and_reopen_types_exist_in_generated_contracts() {
    let declarations = include_str!("../../../../packages/protocol-types/index.ts");
    let schemas: Value = serde_json::from_str(include_str!(
        "../../../../packages/protocol-types/schema.json"
    ))
    .unwrap();
    for ty in [
        "WorkspaceContentConflictProgressRequest",
        "WorkspaceContentConflictReopenRequest",
        "WorkspaceContentConflictReopenResponse",
    ] {
        assert!(
            declarations.contains(&format!("export type {ty} =")),
            "declaration: {ty}"
        );
        assert!(schemas["$defs"][ty].is_object(), "schema: {ty}");
    }
}
