use notryx_protocol::{OperationReceipt, RestoreNodeRequest, TrashNode, TrashPage};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const NODE: &str = "019a1000-0000-7000-8000-000000000001";
const PARENT: &str = "019a1000-0000-7000-8000-000000000002";
const OPERATION: &str = "019a1000-0000-7000-8000-000000000003";
const EPOCH: &str = "550e8400-e29b-41d4-a716-446655440000";

fn restore() -> Value {
    json!({"expected_meta_rev": "opaque:m1", "parent_id": PARENT, "name": "복원 문서.md"})
}

fn trash(kind: &str, restorable: bool) -> Value {
    json!({"node_id": NODE, "parent_id": PARENT, "name": "이전 이름.md", "kind": kind,
        "meta_rev": "deleted:m2", "restorable": restorable})
}

fn receipt(status: &str) -> Value {
    json!({"status": status, "operation_id": OPERATION,
        "operation_seq": "18446744073709551615", "workspace_epoch": EPOCH})
}

fn changed(status: &str, kind: &str) -> Value {
    let file = kind == "file";
    let mut value = receipt(status);
    value["commit_seq"] = json!("9007199254740993");
    value["node"] = json!({"node_id": NODE, "parent_id": PARENT, "name": "문서.md",
        "kind": kind, "deleted": status == "deleted", "meta_rev": "m3",
        "meta_etag": "\"m3\"", "node_etag": "\"n3\"",
        "content_rev": if file {Some("c3")} else {None},
        "content_etag": if file {Some("\"c3\"")} else {None},
        "content_hash": if file {Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")} else {None},
        "size": if file {Some("0")} else {None},
        "children_generation": if file {"0"} else {"7"},
        "last_change_seq": "9007199254740993"});
    value
}

#[test]
fn restore_requests_preserve_destination_and_opaque_revision_bytes() {
    for revision in [
        "opaque:meta\\revision".into(),
        "a".repeat(256),
        "한".repeat(85),
    ] {
        let mut expected = restore();
        expected["expected_meta_rev"] = json!(revision);
        let parsed: RestoreNodeRequest = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
}

#[test]
fn restore_requests_reject_oversized_revisions_invalid_names_and_extra_fields() {
    for invalid in [String::new(), "a".repeat(257), "한".repeat(86)] {
        let mut value = restore();
        value["expected_meta_rev"] = json!(invalid);
        assert!(serde_json::from_value::<RestoreNodeRequest>(value).is_err());
    }
    for name in ["", "../outside", "a\\b", "name.", ".mdw-lock", "e\u{301}"] {
        let mut value = restore();
        value["name"] = json!(name);
        assert!(serde_json::from_value::<RestoreNodeRequest>(value).is_err());
    }
    for invalid in [
        Value::Null,
        json!(EPOCH),
        json!("019a1000-0000-7000-0000-000000000002"),
    ] {
        let mut value = restore();
        value["parent_id"] = invalid;
        assert!(serde_json::from_value::<RestoreNodeRequest>(value).is_err());
    }
    for missing in ["expected_meta_rev", "parent_id", "name"] {
        let mut value = restore();
        value.as_object_mut().unwrap().remove(missing);
        assert!(serde_json::from_value::<RestoreNodeRequest>(value).is_err());
    }
    let mut value = restore();
    value["recursive"] = json!(true);
    assert!(serde_json::from_value::<RestoreNodeRequest>(value).is_err());
}

#[test]
fn trash_pages_preserve_previous_identity_availability_and_optional_cursor() {
    for next in [Value::Null, json!(NODE)] {
        let expected = json!({"workspace_epoch": EPOCH,
            "nodes": [trash("file", true), trash("file", false), trash("folder", true)],
            "next_after_id": next});
        let parsed: TrashPage = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
    let empty = json!({"workspace_epoch": EPOCH, "nodes": [], "next_after_id": null});
    let parsed: TrashPage = serde_json::from_value(empty.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), empty);
}

#[test]
fn trash_nodes_require_valid_nonroot_identity_and_declared_fields() {
    for (field, invalid) in [
        ("node_id", json!(EPOCH)),
        ("parent_id", Value::Null),
        ("parent_id", json!(EPOCH)),
        ("name", json!("")),
        ("name", json!("../outside")),
        ("meta_rev", json!("")),
        ("kind", json!("symlink")),
        ("restorable", json!("true")),
        ("full_path", json!("derived/path")),
    ] {
        let mut value = trash("file", true);
        value[field] = invalid;
        assert!(
            serde_json::from_value::<TrashNode>(value).is_err(),
            "{field}"
        );
    }
    for missing in [
        "node_id",
        "parent_id",
        "name",
        "kind",
        "meta_rev",
        "restorable",
    ] {
        let mut value = trash("folder", true);
        value.as_object_mut().unwrap().remove(missing);
        assert!(serde_json::from_value::<TrashNode>(value).is_err());
    }
}

#[test]
fn trash_pages_require_explicit_null_cursor_and_closed_envelope() {
    for missing in ["workspace_epoch", "nodes", "next_after_id"] {
        let mut value = json!({"workspace_epoch": EPOCH, "nodes": [], "next_after_id": null});
        value.as_object_mut().unwrap().remove(missing);
        assert!(
            serde_json::from_value::<TrashPage>(value).is_err(),
            "{missing}"
        );
    }
    for (field, invalid) in [
        ("workspace_epoch", json!(NODE)),
        ("next_after_id", json!(EPOCH)),
        ("snapshot_id", json!("unsupported")),
    ] {
        let mut value = json!({"workspace_epoch": EPOCH, "nodes": [], "next_after_id": null});
        value[field] = invalid;
        assert!(serde_json::from_value::<TrashPage>(value).is_err());
    }
}

#[test]
fn lifecycle_receipts_preserve_deleted_and_restored_file_and_folder_state() {
    for status in ["deleted", "restored"] {
        for kind in ["file", "folder"] {
            let expected = changed(status, kind);
            let parsed: OperationReceipt = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
        }
    }
}

#[test]
fn lifecycle_receipts_reject_wrong_liveness_root_and_incomplete_own_state() {
    for status in ["deleted", "restored"] {
        let mut value = changed(status, "folder");
        value["node"]["deleted"] = json!(status != "deleted");
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        let mut value = changed(status, "folder");
        value["node"]["parent_id"] = Value::Null;
        value["node"]["name"] = json!("");
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        for field in [
            "parent_id",
            "content_rev",
            "content_etag",
            "content_hash",
            "size",
        ] {
            let mut value = changed(status, "folder");
            value["node"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
        for (field, invalid) in [
            ("size", json!(0)),
            ("children_generation", json!("1")),
            ("last_change_seq", json!("18446744073709551616")),
            ("content_rev", Value::Null),
        ] {
            let mut value = changed(status, "file");
            value["node"][field] = invalid;
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
        let mut value = changed(status, "file");
        value.as_object_mut().unwrap().remove("commit_seq");
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    }
}

#[test]
fn lifecycle_failures_preserve_recorded_results_with_positive_sequences() {
    for status in [
        "node_precondition_failed",
        "node_deleted",
        "not_deleted",
        "folder_not_empty",
        "history_expired",
    ] {
        let mut expected = receipt(status);
        if status == "node_precondition_failed" {
            expected["current_node_etag"] = json!("\"opaque:node:tag\"");
        }
        let parsed: OperationReceipt = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
        for invalid in [
            json!(0),
            json!("0"),
            json!("-1"),
            json!("18446744073709551616"),
        ] {
            let mut value = expected.clone();
            value["operation_seq"] = invalid;
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
        for missing in ["operation_id", "operation_seq", "workspace_epoch"] {
            let mut value = expected.clone();
            value.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
        for (field, invalid) in [
            ("operation_id", json!(EPOCH)),
            ("workspace_epoch", json!(NODE)),
            ("unexpected", json!(true)),
        ] {
            let mut value = expected.clone();
            value[field] = invalid;
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
    }
}

#[test]
fn lifecycle_generated_types_keep_required_fields_and_nullable_cursor() {
    let schema = serde_json::to_value(schemars::schema_for!(TrashPage)).unwrap();
    assert_eq!(schema["additionalProperties"], false);
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("next_after_id"))
    );
    let declaration = TrashPage::decl(&Config::default());
    assert!(declaration.contains("next_after_id: string | null"));
    assert!(declaration.contains("nodes: Array<TrashNode>"));
    let declaration = RestoreNodeRequest::decl(&Config::default());
    assert!(declaration.contains("expected_meta_rev: string"));
    let schema = serde_json::to_value(schemars::schema_for!(RestoreNodeRequest)).unwrap();
    assert_eq!(schema["additionalProperties"], false);
}
