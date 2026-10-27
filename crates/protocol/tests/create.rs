use notryx_protocol::{CreateNodeRequest, OperationReceipt};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const NODE: &str = "019a1000-0000-7000-8000-000000000001";
const PARENT: &str = "019a1000-0000-7000-8000-000000000002";
const OPERATION: &str = "019a1000-0000-7000-8000-000000000003";
const EPOCH: &str = "550e8400-e29b-41d4-a716-446655440000";

fn request() -> Value {
    json!({"node_id": NODE, "parent_id": PARENT, "name": "문서.md", "kind": "file"})
}

fn created(kind: &str) -> Value {
    let file = kind == "file";
    json!({"status": "created", "operation_id": OPERATION, "operation_seq": "1",
        "workspace_epoch": EPOCH, "commit_seq": "9007199254740993",
        "node": {"node_id": NODE, "parent_id": PARENT, "name": "문서.md", "kind": kind,
            "deleted": false, "meta_rev": "m1", "meta_etag": "\"m1\"", "node_etag": "\"n1\"",
            "content_rev": if file {Some("c1")} else {None},
            "content_etag": if file {Some("\"c1\"")} else {None},
            "content_hash": if file {Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")} else {None},
            "size": if file {Some("0")} else {None}, "children_generation": "0",
            "last_change_seq": "9007199254740993"}})
}

#[test]
fn create_requests_preserve_ids_kind_and_unicode_name() {
    for kind in ["file", "folder"] {
        let mut expected = request();
        expected["kind"] = json!(kind);
        let parsed: CreateNodeRequest = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
    let declaration = CreateNodeRequest::decl(&Config::default());
    assert!(declaration.contains("node_id: string"));
    assert!(declaration.contains("parent_id: string"));
}

#[test]
fn create_requests_reject_invalid_names_unknown_fields_and_wrong_ids() {
    for invalid in [
        "",
        ".",
        "..",
        "a/b",
        "a\\b",
        "name.",
        "name ",
        ".GIT",
        ".mdw-secret",
        "CON",
        "line\n",
        "e\u{301}",
    ] {
        let mut value = request();
        value["name"] = json!(invalid);
        assert!(
            serde_json::from_value::<CreateNodeRequest>(value).is_err(),
            "name {invalid:?}"
        );
    }
    let mut value = request();
    value["name"] = json!("x".repeat(256));
    assert!(serde_json::from_value::<CreateNodeRequest>(value).is_err());
    for field in ["node_id", "parent_id"] {
        let mut value = request();
        value[field] = json!(EPOCH);
        assert!(serde_json::from_value::<CreateNodeRequest>(value).is_err());
    }
    let mut value = request();
    value["upload_id"] = json!("not supported in this phase");
    assert!(serde_json::from_value::<CreateNodeRequest>(value).is_err());
    for field in ["node_id", "parent_id", "name", "kind"] {
        let mut value = request();
        value.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<CreateNodeRequest>(value).is_err());
    }
}

#[test]
fn created_receipts_preserve_complete_file_and_folder_state() {
    for kind in ["file", "folder"] {
        let expected = created(kind);
        let parsed: OperationReceipt = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
    for status in [
        "destination_exists",
        "node_id_conflict",
        "parent_not_folder",
    ] {
        let expected = json!({"status": status, "operation_id": OPERATION,
            "operation_seq": "1", "workspace_epoch": EPOCH});
        let parsed: OperationReceipt = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
}

#[test]
fn created_receipts_require_nullable_fields_and_decimal_u64_state() {
    for field in [
        "parent_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        let mut value = created("folder");
        value["node"].as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<OperationReceipt>(value).is_err(),
            "{field}"
        );
    }
    for field in ["size", "children_generation", "last_change_seq"] {
        for invalid in [json!(1), json!("-1"), json!("18446744073709551616")] {
            let mut value = created("file");
            value["node"][field] = invalid;
            assert!(
                serde_json::from_value::<OperationReceipt>(value).is_err(),
                "{field}"
            );
        }
    }
    let mut value = created("file");
    value["node"]["unexpected"] = json!(true);
    assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
}

#[test]
fn created_node_contract_keeps_nullability_and_string_numbers_explicit() {
    let schema = serde_json::to_value(schemars::schema_for!(notryx_protocol::CreatedNode)).unwrap();
    let required = schema["required"].as_array().unwrap();
    for field in [
        "parent_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        assert!(required.contains(&json!(field)), "{field}");
    }
    let declaration = notryx_protocol::CreatedNode::decl(&Config::default());
    assert!(declaration.contains("size: string | null"));
    assert!(declaration.contains("children_generation: string"));
    assert!(declaration.contains("last_change_seq: string"));
    for (kind, field, invalid) in [
        ("folder", "content_rev", json!("unexpected")),
        ("file", "size", Value::Null),
        ("file", "parent_id", Value::Null),
        ("folder", "deleted", json!(true)),
    ] {
        let mut value = created(kind);
        value["node"][field] = invalid;
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    }
}
