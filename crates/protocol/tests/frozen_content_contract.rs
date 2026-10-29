use notryx_protocol::{WorkspaceFrozenContentInspectRequest, WorkspaceFrozenContentInspection};
use serde_json::{Value, json};

fn decode_request(value: Value) -> Result<Value, serde_json::Error> {
    serde_json::from_value::<WorkspaceFrozenContentInspectRequest>(value)
        .and_then(serde_json::to_value)
}

fn decode_response(value: Value) -> Result<Value, serde_json::Error> {
    serde_json::from_value::<WorkspaceFrozenContentInspection>(value).and_then(serde_json::to_value)
}

fn binding(device: &str, stream: &str, epoch: &str) -> Value {
    json!({
        "server_origin": "https://example.invalid",
        "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        "workspace_epoch": epoch,
        "device_id": device,
        "device_stream_id": stream
    })
}

fn current_binding() -> Value {
    binding(
        "550e8400-e29b-41d4-a716-446655440001",
        "550e8400-e29b-41d4-a716-446655440002",
        "550e8400-e29b-41d4-a716-446655440003",
    )
}

fn old_binding() -> Value {
    binding(
        "550e8400-e29b-41d4-a716-446655440004",
        "550e8400-e29b-41d4-a716-446655440005",
        "550e8400-e29b-41d4-a716-446655440006",
    )
}

fn request() -> Value {
    json!({
        "renderer_session_id": "renderer",
        "current_binding": current_binding(),
        "old_binding": old_binding(),
        "operation_id": "019a1000-0000-7000-8000-000000000003"
    })
}

fn response() -> Value {
    json!({
        "current_binding": current_binding(),
        "old_binding": old_binding(),
        "document_id": "019a1000-0000-7000-8000-000000000002",
        "operation_id": "019a1000-0000-7000-8000-000000000003",
        "operation_seq": "1",
        "operation_status": "sent",
        "abandon_state": "none",
        "operation_generation": "2",
        "local_generation": "3",
        "expected_revision": "base-revision",
        "outcome_unknown": true,
        "base_revision": "base-revision",
        "base_bytes_base64": "YmFzZQ==",
        "base_hash": "sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2",
        "operation_bytes_base64": "c2VudA==",
        "operation_hash": "sha256:7afbb3347fb7252e533d58d99d72d9106fc6fdb3f30df23fa70b764c15ac42c5",
        "local_bytes_base64": "bG9jYWw=",
        "local_hash": "sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6",
        "remote_revision": "current-revision",
        "remote_bytes_base64": "cmVtb3Rl",
        "remote_hash": "sha256:b71199ebd070b36beab7317920c2c2f1d777df8d05e5527d8458fda57cb17a7a"
    })
}

#[test]
fn frozen_content_request_preserves_both_bindings_and_requires_all_fields() {
    let valid = request();
    assert_eq!(decode_request(valid.clone()).unwrap(), valid);
    for field in [
        "renderer_session_id",
        "current_binding",
        "old_binding",
        "operation_id",
    ] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(decode_request(missing).is_err(), "missing {field}");
    }
    for field in ["renderer_session_id", "operation_id"] {
        let mut wrong_type = valid.clone();
        wrong_type[field] = json!(7);
        assert!(decode_request(wrong_type).is_err(), "wrong type {field}");
    }
    let mut extra = valid.clone();
    extra["path"] = json!("../outside");
    assert!(decode_request(extra).is_err());
    for field in ["current_binding", "old_binding"] {
        let mut missing_epoch = valid.clone();
        missing_epoch[field]
            .as_object_mut()
            .unwrap()
            .remove("workspace_epoch");
        assert!(
            decode_request(missing_epoch).is_err(),
            "missing epoch {field}"
        );
    }
}

#[test]
fn frozen_content_response_keeps_old_operation_separate_from_newer_local_draft() {
    let valid = response();
    assert_eq!(decode_response(valid.clone()).unwrap(), valid);
    assert_ne!(valid["current_binding"], valid["old_binding"]);
    assert_ne!(valid["operation_generation"], valid["local_generation"]);
    assert_ne!(valid["operation_bytes_base64"], valid["local_bytes_base64"]);
    assert_ne!(valid["operation_hash"], valid["local_hash"]);
    assert_eq!(valid["outcome_unknown"], json!(true));
    assert_eq!(valid["operation_status"], json!("sent"));
    let mut queued = valid.clone();
    queued["operation_status"] = json!("queued");
    queued["expected_revision"] = Value::Null;
    queued["outcome_unknown"] = json!(false);
    assert_eq!(decode_response(queued.clone()).unwrap(), queued);
}

#[test]
fn frozen_content_response_requires_all_fields_and_rejects_unknown_fields() {
    let valid = response();
    assert_eq!(decode_response(valid.clone()).unwrap(), valid);
    for field in [
        "current_binding",
        "old_binding",
        "document_id",
        "operation_id",
        "operation_seq",
        "operation_status",
        "abandon_state",
        "operation_generation",
        "local_generation",
        "expected_revision",
        "outcome_unknown",
        "base_revision",
        "base_bytes_base64",
        "base_hash",
        "operation_bytes_base64",
        "operation_hash",
        "local_bytes_base64",
        "local_hash",
        "remote_revision",
        "remote_bytes_base64",
        "remote_hash",
    ] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(decode_response(missing).is_err(), "missing {field}");
    }
    let mut extra = valid.clone();
    extra["credential"] = json!("forbidden");
    assert!(decode_response(extra).is_err());
    let mut wrong_bool = valid.clone();
    wrong_bool["outcome_unknown"] = json!("true");
    assert!(decode_response(wrong_bool).is_err());
    let mut unknown_status = valid.clone();
    unknown_status["operation_status"] = json!("not_a_status");
    assert!(decode_response(unknown_status).is_err());
    for field in [
        "document_id",
        "operation_id",
        "base_revision",
        "base_bytes_base64",
        "base_hash",
        "operation_bytes_base64",
        "operation_hash",
        "local_bytes_base64",
        "local_hash",
        "remote_revision",
        "remote_bytes_base64",
        "remote_hash",
    ] {
        let mut wrong_type = valid.clone();
        wrong_type[field] = json!(false);
        assert!(decode_response(wrong_type).is_err(), "wrong type {field}");
    }
}

#[test]
fn frozen_content_sequence_and_generations_are_canonical_positive_i64_decimals() {
    let valid = response();
    assert_eq!(decode_response(valid.clone()).unwrap(), valid);
    for field in ["operation_seq", "operation_generation", "local_generation"] {
        for invalid in [
            json!(0),
            json!(""),
            json!("0"),
            json!("01"),
            json!("-1"),
            json!("1.0"),
            json!("9223372036854775808"),
        ] {
            let mut value = valid.clone();
            value[field] = invalid.clone();
            assert!(decode_response(value).is_err(), "{field}: {invalid}");
        }
        let mut largest = valid.clone();
        largest[field] = json!("9223372036854775807");
        assert_eq!(decode_response(largest.clone()).unwrap(), largest);
    }
}
