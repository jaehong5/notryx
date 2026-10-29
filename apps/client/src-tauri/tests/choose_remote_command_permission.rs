use notryx_protocol::{
    WorkspaceContentConflictChooseRemoteRequest, WorkspaceContentConflictChooseRemoteResponse,
};
use serde_json::{Value, json};

fn request() -> Value {
    let hash = format!("sha256:{}", "a".repeat(64));
    json!({
        "renderer_session_id":"owner",
        "binding":{"server_origin":"https://workspace.example",
            "workspace_id":"019a1000-0000-7000-8000-000000000001",
            "workspace_epoch":"019a1000-0000-7000-8000-000000000002",
            "device_id":"019a1000-0000-7000-8000-000000000003",
            "device_stream_id":"019a1000-0000-7000-8000-000000000004"},
        "document_id":"019a1000-0000-7000-8000-000000000005",
        "edit_session_id":"session",
        "operation_id":"019a1000-0000-7000-8000-000000000006",
        "generation":"1", "expected_intent_version":null,
        "base_revision":"base", "conflict_revision":"failed", "remote_revision":"remote",
        "base_hash":hash, "local_hash":hash, "remote_hash":hash
    })
}

#[test]
fn choose_remote_request_validates_null_or_decimal_version_generation_and_hashes() {
    let initial = request();
    assert!(
        serde_json::from_value::<WorkspaceContentConflictChooseRemoteRequest>(initial.clone())
            .is_ok()
    );
    for field in ["generation", "expected_intent_version"] {
        for valid in [json!("0"), json!("1"), json!("18446744073709551615")] {
            let mut item = initial.clone();
            item[field] = valid;
            assert!(
                serde_json::from_value::<WorkspaceContentConflictChooseRemoteRequest>(item).is_ok(),
                "field={field}"
            );
        }
        for invalid in [
            json!(""),
            json!("-1"),
            json!("+1"),
            json!("1.5"),
            json!("18446744073709551616"),
            json!(1),
            json!(false),
        ] {
            let mut item = initial.clone();
            item[field] = invalid;
            assert!(
                serde_json::from_value::<WorkspaceContentConflictChooseRemoteRequest>(item)
                    .is_err(),
                "field={field}"
            );
        }
    }
    let mut null_generation = initial.clone();
    null_generation["generation"] = Value::Null;
    assert!(
        serde_json::from_value::<WorkspaceContentConflictChooseRemoteRequest>(null_generation)
            .is_err()
    );
    for field in ["base_hash", "local_hash", "remote_hash"] {
        for invalid in [
            json!("sha256:bad"),
            json!(format!("sha256:{}", "A".repeat(64))),
            json!(format!("sha256:{}", "a".repeat(63))),
            json!(null),
        ] {
            let mut item = initial.clone();
            item[field] = invalid;
            assert!(
                serde_json::from_value::<WorkspaceContentConflictChooseRemoteRequest>(item)
                    .is_err(),
                "field={field}"
            );
        }
    }
    let mut extra = initial.clone();
    extra["unexpected"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictChooseRemoteRequest>(extra).is_err());
    let mut extra_binding = initial;
    extra_binding["binding"]["unexpected"] = json!(true);
    assert!(
        serde_json::from_value::<WorkspaceContentConflictChooseRemoteRequest>(extra_binding)
            .is_err()
    );
}

#[test]
fn choose_remote_response_validates_variant_and_decimal_bounds() {
    for item in [
        json!({"status":"pending", "version":"0"}),
        json!({"status":"completed", "generation":"1"}),
    ] {
        assert!(
            serde_json::from_value::<WorkspaceContentConflictChooseRemoteResponse>(item).is_ok()
        );
    }
    for (status, field) in [("pending", "version"), ("completed", "generation")] {
        for invalid in [
            json!(""),
            json!("-1"),
            json!("18446744073709551616"),
            json!(1),
            json!(null),
        ] {
            let mut item = json!({"status":status});
            item[field] = invalid;
            assert!(
                serde_json::from_value::<WorkspaceContentConflictChooseRemoteResponse>(item)
                    .is_err(),
                "status={status}"
            );
        }
        let mut extra = json!({"status":status});
        extra[field] = json!("1");
        extra["raw_bytes"] = json!("forbidden");
        assert!(
            serde_json::from_value::<WorkspaceContentConflictChooseRemoteResponse>(extra).is_err()
        );
    }
    assert!(
        serde_json::from_value::<WorkspaceContentConflictChooseRemoteResponse>(
            json!({"status":"unknown"})
        )
        .is_err()
    );
}

#[test]
fn choose_remote_is_registered_with_explicit_main_window_permission() {
    let command = "workspace_choose_remote_content_conflict";
    let manifest = include_str!("../build.rs");
    let handler = include_str!("../src/lib.rs");
    let capability: Value =
        serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
    assert!(manifest.contains(&format!("\"{command}\"")));
    assert!(handler.contains(&format!("conflict_snapshot_native::{command}")));
    assert_eq!(capability["windows"], json!(["main"]));
    assert!(
        capability["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "allow-workspace-choose-remote-content-conflict")
    );
    let permission = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("permissions/autogenerated/workspace_choose_remote_content_conflict.toml"),
    )
    .unwrap();
    assert!(permission.contains("identifier = \"allow-workspace-choose-remote-content-conflict\""));
    assert!(permission.contains("commands.allow = [\"workspace_choose_remote_content_conflict\"]"));
}
