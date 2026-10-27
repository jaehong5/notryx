use notryx_protocol::{AdminRequest, AdminResponse, OperationReceipt};
use serde_json::json;

const OPERATION: &str = "019a1000-0000-7000-8000-000000000001";
const EPOCH: &str = "550e8400-e29b-41d4-a716-446655440000";

#[test]
fn recovery_conflict_is_a_terminal_receipt_without_a_claimed_commit() {
    let value = json!({
        "status": "recovery_conflict",
        "operation_id": OPERATION,
        "operation_seq": "9007199254740993",
        "workspace_epoch": EPOCH,
    });
    let receipt = serde_json::from_value::<OperationReceipt>(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(receipt).unwrap(), value);

    for field in ["node", "commit_seq", "unexpected"] {
        let mut invalid = value.clone();
        invalid[field] = json!("not a successful commit");
        assert!(serde_json::from_value::<OperationReceipt>(invalid).is_err());
    }
    for sequence in [json!(1), json!("0"), json!("-1")] {
        let mut invalid = value.clone();
        invalid["operation_seq"] = sequence;
        assert!(serde_json::from_value::<OperationReceipt>(invalid).is_err());
    }
}

#[test]
fn local_recovery_commands_validate_operation_ids_and_reject_extra_fields() {
    for command in ["recovery_inspect", "recovery_resolve", "recovery_release"] {
        let value = json!({ "command": command, "operation_id": OPERATION });
        let request = serde_json::from_value::<AdminRequest>(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), value);
        let mut invalid = value.clone();
        invalid["unexpected"] = json!(true);
        assert!(serde_json::from_value::<AdminRequest>(invalid).is_err());
        for id in [
            EPOCH,
            "not-an-operation",
            "019a1000-0000-7000-0000-000000000001",
        ] {
            let mut invalid = value.clone();
            invalid["operation_id"] = json!(id);
            assert!(serde_json::from_value::<AdminRequest>(invalid).is_err());
        }
    }
}

#[test]
fn local_recovery_list_is_bounded_and_requires_an_explicit_nullable_cursor() {
    for cursor in [json!(null), json!(OPERATION)] {
        let value =
            json!({ "command": "recovery_list", "after_operation_id": cursor, "limit": 20 });
        let request = serde_json::from_value::<AdminRequest>(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), value);
        for limit in [json!(0), json!(21), json!("20")] {
            let mut invalid = value.clone();
            invalid["limit"] = limit;
            assert!(serde_json::from_value::<AdminRequest>(invalid).is_err());
        }
        let mut invalid = value;
        invalid
            .as_object_mut()
            .unwrap()
            .remove("after_operation_id");
        assert!(serde_json::from_value::<AdminRequest>(invalid).is_err());
    }
}

#[test]
fn recovery_diagnostics_report_retained_objects_without_source_bytes() {
    let summary = json!({
        "operation_id": OPERATION,
        "resolved": false,
        "retained_objects": "3",
    });
    for result in [
        json!({ "kind": "recovery_list", "entries": [summary.clone()], "next_after_operation_id": null }),
        json!({
            "kind": "recovery_detail",
            "summary": summary,
            "paths": ["Example.md"],
            "objects": ["sha256:0000000000000000000000000000000000000000000000000000000000000000"],
            "objects_truncated": true,
            "next_after_hash": "0000000000000000000000000000000000000000000000000000000000000000",
        }),
        json!({ "kind": "recovery_resolved", "operation_id": OPERATION, "ready": true }),
    ] {
        let value = json!({ "status": "ok", "result": result });
        let response = serde_json::from_value::<AdminResponse>(value.clone())
            .ok()
            .unwrap();
        assert!(serde_json::to_value(response).unwrap() == value);
    }
}

#[test]
fn recovery_object_pages_accept_a_bounded_hash_cursor_and_require_the_next_cursor() {
    let hash = "a".repeat(64);
    let value =
        json!({ "command": "recovery_inspect", "operation_id": OPERATION, "after_hash": hash });
    let request = serde_json::from_value::<AdminRequest>(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), value);
    for cursor in [
        json!("a".repeat(63)),
        json!("A".repeat(64)),
        json!("g".repeat(64)),
        json!(12),
    ] {
        let mut invalid = value.clone();
        invalid["after_hash"] = cursor;
        assert!(serde_json::from_value::<AdminRequest>(invalid).is_err());
    }
    let result = json!({
        "kind": "recovery_detail",
        "summary": {"operation_id": OPERATION, "resolved": true, "retained_objects": "70"},
        "paths": ["Example.md"], "objects": [], "objects_truncated": false,
        "next_after_hash": null,
    });
    let value = json!({ "status": "ok", "result": result });
    let response = serde_json::from_value::<AdminResponse>(value.clone())
        .ok()
        .unwrap();
    assert!(serde_json::to_value(response).unwrap() == value);
    let mut invalid = value;
    invalid["result"]
        .as_object_mut()
        .unwrap()
        .remove("next_after_hash");
    assert!(serde_json::from_value::<AdminResponse>(invalid).is_err());
}
