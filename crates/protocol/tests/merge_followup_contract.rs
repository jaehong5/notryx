use notryx_protocol::{
    WorkspaceContentConflictMergeFollowup, WorkspaceContentConflictMergeFollowupApprovalRequest,
    WorkspaceContentConflictMergeFollowupStatusResponse,
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

fn followup() -> Value {
    json!({
        "operation_id": "019a1000-0000-7000-8000-000000000007",
        "operation_seq": "42",
        "generation": "3",
        "payload_hash": format!("sha256:{}", "a".repeat(64))
    })
}

fn response(followup: Value) -> Value {
    json!({
        "binding": binding(),
        "document_id": "019a1000-0000-7000-8000-000000000005",
        "operation_id": "019a1000-0000-7000-8000-000000000006",
        "intent_version": "0",
        "followup": followup
    })
}

fn approval() -> Value {
    json!({
        "progress": {
            "renderer_session_id": "synthetic-owner",
            "binding": binding(),
            "document_id": "019a1000-0000-7000-8000-000000000005",
            "edit_session_id": "synthetic-session",
            "operation_id": "019a1000-0000-7000-8000-000000000006",
            "expected_intent_version": "0"
        },
        "followup": followup()
    })
}

#[test]
fn followup_requires_all_fields_and_rejects_unknown_fields() {
    let valid = followup();
    assert!(serde_json::from_value::<WorkspaceContentConflictMergeFollowup>(valid.clone()).is_ok());
    for field in [
        "operation_id",
        "operation_seq",
        "generation",
        "payload_hash",
    ] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeFollowup>(missing).is_err(),
            "missing {field}"
        );
    }
    let mut extra = valid;
    extra["extra"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceContentConflictMergeFollowup>(extra).is_err());
}

#[test]
fn followup_sequence_and_generation_require_positive_canonical_i64_decimals() {
    for field in ["operation_seq", "generation"] {
        for valid in ["1", "42", "9223372036854775807"] {
            let mut value = followup();
            value[field] = json!(valid);
            assert!(
                serde_json::from_value::<WorkspaceContentConflictMergeFollowup>(value).is_ok(),
                "{field}: {valid}"
            );
        }
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
            let mut value = followup();
            value[field] = invalid.clone();
            assert!(
                serde_json::from_value::<WorkspaceContentConflictMergeFollowup>(value).is_err(),
                "{field}: {invalid}"
            );
        }
    }
}

#[test]
fn followup_payload_hash_requires_lowercase_sha256_shape() {
    let valid = followup();
    for invalid in [
        json!(""),
        json!(format!("sha256:{}", "A".repeat(64))),
        json!(format!("sha256:{}", "a".repeat(63))),
        json!(format!("sha256:{}", "g".repeat(64))),
        json!(format!("sha1:{}", "a".repeat(64))),
        json!(1),
        Value::Null,
    ] {
        let mut value = valid.clone();
        value["payload_hash"] = invalid.clone();
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeFollowup>(value).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn followup_status_requires_explicit_null_or_complete_followup() {
    for item in [Value::Null, followup()] {
        let expected = response(item);
        let decoded: WorkspaceContentConflictMergeFollowupStatusResponse =
            serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
    }
    let valid = response(Value::Null);
    for field in [
        "binding",
        "document_id",
        "operation_id",
        "intent_version",
        "followup",
    ] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeFollowupStatusResponse>(missing)
                .is_err(),
            "missing {field}"
        );
    }
    let mut extra = valid;
    extra["extra"] = json!(true);
    assert!(
        serde_json::from_value::<WorkspaceContentConflictMergeFollowupStatusResponse>(extra)
            .is_err()
    );
}

#[test]
fn followup_status_intent_version_requires_canonical_nonnegative_i64_decimal() {
    for valid in ["0", "1", "42", "9223372036854775807"] {
        let mut value = response(Value::Null);
        value["intent_version"] = json!(valid);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeFollowupStatusResponse>(value)
                .is_ok(),
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
        let mut value = response(Value::Null);
        value["intent_version"] = invalid.clone();
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeFollowupStatusResponse>(value)
                .is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn followup_approval_requires_progress_and_complete_followup() {
    let valid = approval();
    let decoded: WorkspaceContentConflictMergeFollowupApprovalRequest =
        serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), valid);
    for field in ["progress", "followup"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeFollowupApprovalRequest>(missing)
                .is_err(),
            "missing {field}"
        );
    }
    for field in [
        "renderer_session_id",
        "binding",
        "document_id",
        "edit_session_id",
        "operation_id",
        "expected_intent_version",
    ] {
        let mut missing = valid.clone();
        missing["progress"].as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceContentConflictMergeFollowupApprovalRequest>(missing)
                .is_err(),
            "missing progress.{field}"
        );
    }
    let mut extra = valid.clone();
    extra["extra"] = json!(true);
    assert!(
        serde_json::from_value::<WorkspaceContentConflictMergeFollowupApprovalRequest>(extra)
            .is_err()
    );
    let mut extra_progress = valid.clone();
    extra_progress["progress"]["extra"] = json!(true);
    assert!(
        serde_json::from_value::<WorkspaceContentConflictMergeFollowupApprovalRequest>(
            extra_progress
        )
        .is_err()
    );
    let mut null_followup = valid;
    null_followup["followup"] = Value::Null;
    assert!(
        serde_json::from_value::<WorkspaceContentConflictMergeFollowupApprovalRequest>(
            null_followup
        )
        .is_err()
    );
}
