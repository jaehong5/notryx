use notryx_protocol::{MetadataPrecondition, MoveNodeRequest, OperationReceipt};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const NODE: &str = "019a1000-0000-7000-8000-000000000001";
const PARENT: &str = "019a1000-0000-7000-8000-000000000002";
const DESTINATION: &str = "019a1000-0000-7000-8000-000000000003";
const OPERATION: &str = "019a1000-0000-7000-8000-000000000004";
const EPOCH: &str = "550e8400-e29b-41d4-a716-446655440000";

fn request() -> Value {
    json!({"expected_parent_id": PARENT, "parent_id": DESTINATION, "name": "새 이름.md"})
}

fn receipt(status: &str) -> Value {
    json!({"status": status, "operation_id": OPERATION,
        "operation_seq": "18446744073709551615", "workspace_epoch": EPOCH})
}

fn moved(kind: &str) -> Value {
    let file = kind == "file";
    let mut value = receipt("moved");
    value["commit_seq"] = json!("9007199254740993");
    value["node"] = json!({"node_id": NODE, "parent_id": DESTINATION,
        "name": "새 이름.md", "kind": kind, "deleted": false,
        "meta_rev": "m2", "meta_etag": "\"m2\"", "node_etag": "\"n2\"",
        "content_rev": if file {Some("c1")} else {None},
        "content_etag": if file {Some("\"c1\"")} else {None},
        "content_hash": if file {Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")} else {None},
        "size": if file {Some("0")} else {None},
        "children_generation": if file {"0"} else {"5"},
        "last_change_seq": "9007199254740993"});
    value
}

#[test]
fn move_requests_preserve_both_parents_and_unicode_names() {
    let expected = request();
    let parsed: MoveNodeRequest = serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    let declaration = MoveNodeRequest::decl(&Config::default());
    assert!(declaration.contains("expected_parent_id: string"));
    assert!(declaration.contains("parent_id: string"));
}

#[test]
fn move_requests_reject_invalid_names_ids_and_undeclared_fields() {
    for invalid in [
        "", ".", "..", "a/b", "a\\b", "name.", "name ", ".GIT", ".mdw-tmp", "CON", "line\n",
        "e\u{301}",
    ] {
        let mut value = request();
        value["name"] = json!(invalid);
        assert!(
            serde_json::from_value::<MoveNodeRequest>(value).is_err(),
            "{invalid:?}"
        );
    }
    let mut value = request();
    value["name"] = json!("x".repeat(256));
    assert!(serde_json::from_value::<MoveNodeRequest>(value).is_err());
    for field in ["expected_parent_id", "parent_id"] {
        for invalid in [EPOCH, "019a1000-0000-7000-0000-000000000001"] {
            let mut value = request();
            value[field] = json!(invalid);
            assert!(serde_json::from_value::<MoveNodeRequest>(value).is_err());
        }
    }
    for field in ["expected_parent_id", "parent_id", "name"] {
        let mut value = request();
        value.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<MoveNodeRequest>(value).is_err());
    }
    let mut value = request();
    value["subtree_generation"] = json!("5");
    assert!(serde_json::from_value::<MoveNodeRequest>(value).is_err());
}

#[test]
fn metadata_preconditions_preserve_opaque_revision_and_bound_bytes() {
    for literal in [
        "opaque:metadata:A-7",
        "m.7d38b92f",
        "한글:revision",
        "back\\slash",
    ] {
        let condition = MetadataPrecondition::parse(&format!("\"{literal}\"")).unwrap();
        let revision = condition.revision().unwrap();
        assert_eq!(revision.as_str(), literal);
        assert!(condition.matches(&revision).unwrap());
    }
    let maximum = "a".repeat(256);
    assert_eq!(
        MetadataPrecondition::parse(&format!("\"{maximum}\""))
            .unwrap()
            .revision()
            .unwrap()
            .as_str(),
        maximum
    );
    for literal in [String::new(), "a".repeat(257), "한".repeat(86)] {
        assert!(
            MetadataPrecondition::parse(&format!("\"{literal}\""))
                .unwrap()
                .revision()
                .is_err()
        );
    }
    for invalid in ["*", "W/\"m1\"", "\"m1\",\"m2\"", "\"line\nbreak\""] {
        assert!(MetadataPrecondition::parse(invalid).is_err());
    }
}

#[test]
fn moved_receipts_preserve_file_and_folder_own_state() {
    for kind in ["file", "folder"] {
        let expected = moved(kind);
        let parsed: OperationReceipt = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
}

#[test]
fn move_failures_are_closed_terminal_receipts_with_positive_sequences() {
    for status in [
        "metadata_precondition_failed",
        "expected_parent_mismatch",
        "cycle_detected",
        "root_immutable",
    ] {
        let mut expected = receipt(status);
        if status == "metadata_precondition_failed" {
            expected["current_meta_rev"] = json!("m3");
            expected["current_meta_etag"] = json!("\"m3\"");
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
        let mut value = expected.clone();
        value["unexpected"] = json!(true);
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        for field in ["operation_id", "operation_seq", "workspace_epoch"] {
            let mut value = expected.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
        for (field, invalid) in [("operation_id", EPOCH), ("workspace_epoch", OPERATION)] {
            let mut value = expected.clone();
            value[field] = json!(invalid);
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
    }
}

#[test]
fn moved_receipts_require_live_nonroot_nodes_and_complete_state() {
    for field in [
        "parent_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        let mut value = moved("folder");
        value["node"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    }
    for field in ["size", "children_generation", "last_change_seq"] {
        for invalid in [json!(1), json!("-1"), json!("18446744073709551616")] {
            let mut value = moved("file");
            value["node"][field] = invalid;
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
    }
    for (kind, field, invalid) in [
        ("file", "size", Value::Null),
        ("folder", "content_rev", json!("c1")),
        ("folder", "deleted", json!(true)),
    ] {
        let mut value = moved(kind);
        value["node"][field] = invalid;
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    }
    let mut value = moved("folder");
    value["node"]["parent_id"] = Value::Null;
    value["node"]["name"] = json!("");
    assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    let mut value = moved("file");
    value["node"]["unexpected"] = json!(true);
    assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    let mut value = moved("file");
    value.as_object_mut().unwrap().remove("commit_seq");
    assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
}
