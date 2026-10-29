use notryx_protocol::{
    WorkspaceContentConflictMergeOperation, WorkspaceContentConflictMergeStatusRequest,
    WorkspaceContentConflictMergeStatusResponse,
};
use serde_json::{Value, json};

fn binding() -> Value {
    json!({
        "server_origin": "https://workspace.example.invalid",
        "workspace_id": "019a1000-0000-7000-8000-000000000001",
        "workspace_epoch": "019a1000-0000-7000-8000-000000000002",
        "device_id": "019a1000-0000-7000-8000-000000000003",
        "device_stream_id": "019a1000-0000-7000-8000-000000000004"
    })
}

fn request() -> Value {
    json!({
        "renderer_session_id": "synthetic-owner",
        "binding": binding(),
        "document_id": "019a1000-0000-7000-8000-000000000005",
        "operation_id": "019a1000-0000-7000-8000-000000000006",
        "expected_intent_version": "0"
    })
}

fn response(operation: Value) -> Value {
    json!({
        "binding": binding(),
        "document_id": "019a1000-0000-7000-8000-000000000005",
        "operation_id": "019a1000-0000-7000-8000-000000000006",
        "intent_version": "0",
        "merge_operation": operation
    })
}

#[test]
fn merge_status_request_requires_each_field_and_rejects_unknown_fields() {
    let valid = request();
    assert!(
        serde_json::from_value::<WorkspaceContentConflictMergeStatusRequest>(valid.clone()).is_ok()
    );
    for field in [
        "renderer_session_id",
        "binding",
        "document_id",
        "operation_id",
        "expected_intent_version",
    ] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeStatusRequest>(missing).is_err(),
            "missing {field}"
        );
    }
    let mut extra = valid;
    extra["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictMergeStatusRequest>(extra).is_err());
}

#[test]
fn merge_status_intent_version_is_a_canonical_nonnegative_decimal() {
    for valid in ["0", "1", "42", "9223372036854775807"] {
        let mut value = request();
        value["expected_intent_version"] = json!(valid);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeStatusRequest>(value).is_ok(),
            "{valid}"
        );
        let mut value = response(Value::Null);
        value["intent_version"] = json!(valid);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeStatusResponse>(value).is_ok(),
            "{valid}"
        );
    }
    for invalid in [
        json!(""),
        json!("01"),
        json!("-1"),
        json!("+1"),
        json!(" 1"),
        json!("1.0"),
        json!("9223372036854775808"),
        json!("18446744073709551616"),
        json!(1),
        Value::Null,
    ] {
        let mut value = request();
        value["expected_intent_version"] = invalid.clone();
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeStatusRequest>(value).is_err(),
            "{invalid}"
        );
        let mut value = response(Value::Null);
        value["intent_version"] = invalid.clone();
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeStatusResponse>(value).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn merge_status_operation_requires_id_and_positive_canonical_decimal_sequence() {
    let valid =
        json!({"operation_id": "019a1000-0000-7000-8000-000000000007", "operation_seq": "1"});
    assert!(
        serde_json::from_value::<WorkspaceContentConflictMergeOperation>(valid.clone()).is_ok()
    );
    for field in ["operation_id", "operation_seq"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeOperation>(missing).is_err(),
            "missing {field}"
        );
    }
    let mut extra = valid.clone();
    extra["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictMergeOperation>(extra).is_err());
    for invalid in [
        json!(""),
        json!("0"),
        json!("01"),
        json!("-1"),
        json!("+1"),
        json!(" 1"),
        json!("1.0"),
        json!("9223372036854775808"),
        json!("18446744073709551616"),
        json!(1),
        Value::Null,
    ] {
        let mut operation = valid.clone();
        operation["operation_seq"] = invalid.clone();
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeOperation>(operation).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn merge_status_response_round_trips_explicit_null_or_exact_operation_pair() {
    for value in [
        Value::Null,
        json!({"operation_id": "019a1000-0000-7000-8000-000000000007", "operation_seq": "123"}),
    ] {
        let expected = response(value);
        let decoded: WorkspaceContentConflictMergeStatusResponse =
            serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
    }
    let valid = response(Value::Null);
    for field in [
        "binding",
        "document_id",
        "operation_id",
        "intent_version",
        "merge_operation",
    ] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeStatusResponse>(missing).is_err(),
            "missing {field}"
        );
    }
    let mut extra = valid;
    extra["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictMergeStatusResponse>(extra).is_err());
}
