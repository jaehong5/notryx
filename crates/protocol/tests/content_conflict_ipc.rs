use notryx_protocol::{WorkspaceContentConflictRequest, WorkspaceContentConflictSnapshot};
use serde_json::json;

fn binding() -> serde_json::Value {
    json!({"server_origin":"https://example.invalid","workspace_id":"550e8400-e29b-41d4-a716-446655440000",
      "workspace_epoch":"550e8400-e29b-41d4-a716-446655440001","device_id":"550e8400-e29b-41d4-a716-446655440002",
      "device_stream_id":"550e8400-e29b-41d4-a716-446655440003"})
}

#[test]
fn content_conflict_request_uses_binding_session_and_decimal_generation() {
    let valid = json!({"renderer_session_id":"renderer","binding":binding(),"document_id":"document",
        "edit_session_id":"session","generation":"1"});
    let request: WorkspaceContentConflictRequest = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), valid);
    for invalid in [
        json!(1),
        json!("-1"),
        json!("1.1"),
        json!("18446744073709551616"),
    ] {
        let mut value = valid.clone();
        value["generation"] = invalid;
        assert!(serde_json::from_value::<WorkspaceContentConflictRequest>(value).is_err());
    }
    let mut extra = valid.clone();
    extra["path"] = json!("../outside");
    assert!(serde_json::from_value::<WorkspaceContentConflictRequest>(extra).is_err());
}

#[test]
fn content_conflict_response_distinguishes_failure_revision_from_current_remote() {
    let valid = json!({"binding":binding(),"document_id":"document","operation_id":"operation",
        "operation_seq":"1","generation":"2","base_revision":"base", "base_bytes_base64":"YmFzZQ==",
        "base_hash":"sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2",
        "local_bytes_base64":"bG9jYWw=", "local_hash":"sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6",
        "conflict_revision":"peer-1", "remote_revision":"peer-2", "remote_bytes_base64":"cmVtb3Rl",
        "remote_hash":"sha256:b71199ebd070b36beab7317920c2c2f1d777df8d05e5527d8458fda57cb17a7a"});
    let snapshot: WorkspaceContentConflictSnapshot = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(snapshot.conflict_revision, "peer-1");
    assert_eq!(snapshot.remote_revision, "peer-2");
    assert_eq!(serde_json::to_value(snapshot).unwrap(), valid);
    for field in ["base_hash", "local_hash", "remote_hash"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<WorkspaceContentConflictSnapshot>(missing).is_err());
        let mut invalid = valid.clone();
        invalid[field] = json!("sha256:invalid");
        assert!(serde_json::from_value::<WorkspaceContentConflictSnapshot>(invalid).is_err());
    }
    let mut extra = valid.clone();
    extra["credentials"] = json!("forbidden");
    assert!(serde_json::from_value::<WorkspaceContentConflictSnapshot>(extra).is_err());
}
