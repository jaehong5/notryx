use notryx_protocol::{
    WorkspaceFrozenContentLocalInspection, WorkspaceFrozenContentRemoteEvidence,
};
use serde_json::{Value, json};

fn local() -> Value {
    let binding = json!({"server_origin":"https://example.invalid","workspace_id":"workspace","workspace_epoch":"epoch","device_id":"device","device_stream_id":"stream"});
    json!({
        "current_binding":binding, "old_binding":binding, "document_id":"document", "operation_id":"operation",
        "operation_seq":"2", "operation_status":"sent", "abandon_state":"none", "dependency":null,
        "expected_revision":"base-revision", "operation_generation":"2", "local_generation":"3", "outcome_unknown":true,
        "base_revision":"base-revision", "operation_bytes_base64":"c2VudA==", "operation_hash":format!("sha256:{}", "a".repeat(64)),
        "local_bytes_base64":"bG9jYWw=", "local_hash":format!("sha256:{}", "b".repeat(64)),
        "base_bytes_base64":"YmFzZQ==", "base_hash":format!("sha256:{}", "c".repeat(64)), "remote_evidence":{"kind":"unknown"}, "remote_index_error":false
    })
}

#[test]
fn local_frozen_inspection_requires_dependency_and_never_synthesizes_remote_content() {
    let value = local();
    let parsed: WorkspaceFrozenContentLocalInspection =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("dependency");
    assert!(serde_json::from_value::<WorkspaceFrozenContentLocalInspection>(missing).is_err());
    let mut missing_error = value.clone();
    missing_error
        .as_object_mut()
        .unwrap()
        .remove("remote_index_error");
    assert!(
        serde_json::from_value::<WorkspaceFrozenContentLocalInspection>(missing_error).is_err()
    );
    let mut extra = value;
    extra["remote_bytes_base64"] = json!("");
    assert!(serde_json::from_value::<WorkspaceFrozenContentLocalInspection>(extra).is_err());
}

#[test]
fn local_remote_evidence_keeps_deleted_too_large_and_unknown_distinct() {
    for value in [
        json!({"kind":"deleted","change_seq":"9"}),
        json!({"kind":"too_large","change_seq":"9","size":"26214401"}),
        json!({"kind":"unknown"}),
    ] {
        let parsed: WorkspaceFrozenContentRemoteEvidence =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
    for value in [
        json!({"kind":"deleted","change_seq":"0"}),
        json!({"kind":"too_large","change_seq":"9","size":"01"}),
        json!({"kind":"too_large","change_seq":"9","size":"26214400"}),
        json!({"kind":"unknown","bytes":""}),
        json!({"kind":"other"}),
    ] {
        assert!(
            serde_json::from_value::<WorkspaceFrozenContentRemoteEvidence>(value.clone()).is_err(),
            "invalid evidence accepted: {value}"
        );
    }
}
