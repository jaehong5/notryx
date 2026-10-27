use notryx_protocol::{InventoryPage, NodeState, OperationReceipt};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FILE: &str = "019a1000-0000-7000-8000-000000000002";
const EPOCH: &str = "550e8400-e29b-41d4-a716-446655440000";

fn state(kind: &str, root: bool) -> Value {
    let file = kind == "file";
    json!({"node_id": if root { ROOT } else { FILE },
        "parent_id": if root { None } else { Some(ROOT) },
        "name": if root { "" } else { "한글.md" }, "kind": kind, "deleted": false,
        "content_rev": if file {Some("c1")} else {None},
        "content_etag": if file {Some("\"c1\"")} else {None},
        "meta_rev": "m1", "meta_etag": "\"m1\"", "node_etag": "\"n1\"",
        "content_hash": if file {Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")} else {None},
        "size": if file {Some("9007199254740993")} else {None},
        "children_generation": "0", "last_change_seq": "18446744073709551615"})
}

#[test]
fn node_state_round_trips_root_file_folder_and_non_root_tombstone() {
    for value in [
        state("folder", true),
        state("file", false),
        state("folder", false),
    ] {
        let parsed: NodeState = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
    let mut tombstone = state("file", false);
    tombstone["deleted"] = json!(true);
    let parsed: NodeState = serde_json::from_value(tombstone.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), tombstone);
}

#[test]
fn node_state_rejects_inconsistent_root_and_content_shapes() {
    for (kind, root, field, invalid) in [
        ("file", false, "parent_id", Value::Null),
        ("file", false, "name", json!("")),
        ("file", false, "name", json!("../outside")),
        ("file", false, "children_generation", json!("1")),
        ("file", false, "content_rev", Value::Null),
        ("file", false, "content_etag", Value::Null),
        ("file", false, "content_hash", Value::Null),
        ("file", false, "size", Value::Null),
        ("folder", false, "content_rev", json!("unexpected")),
        ("folder", false, "size", json!("0")),
        ("folder", true, "name", json!("named-root")),
        ("folder", true, "deleted", json!(true)),
    ] {
        let mut value = state(kind, root);
        value[field] = invalid;
        assert!(
            serde_json::from_value::<NodeState>(value).is_err(),
            "{kind} {root} {field}"
        );
    }
}

#[test]
fn node_state_requires_explicit_nulls_valid_ids_and_decimal_u64s() {
    for field in [
        "parent_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        let mut value = state("folder", false);
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<NodeState>(value).is_err(),
            "{field}"
        );
    }
    for field in ["size", "children_generation", "last_change_seq"] {
        for invalid in [json!(1), json!("-1"), json!("18446744073709551616")] {
            let mut value = state("file", false);
            value[field] = invalid;
            assert!(
                serde_json::from_value::<NodeState>(value).is_err(),
                "{field}"
            );
        }
    }
    for field in ["node_id", "parent_id"] {
        let mut value = state("file", false);
        value[field] = json!(EPOCH);
        assert!(
            serde_json::from_value::<NodeState>(value).is_err(),
            "{field}"
        );
    }
    let mut value = state("file", false);
    value["full_path"] = json!("derived/path");
    assert!(serde_json::from_value::<NodeState>(value).is_err());
}

#[test]
fn inventory_pages_preserve_epoch_nodes_and_explicit_optional_cursor() {
    for (nodes, next) in [
        (vec![], Value::Null),
        (
            vec![state("folder", true), state("file", false)],
            Value::Null,
        ),
        (
            vec![state("folder", true), state("file", false)],
            json!(FILE),
        ),
    ] {
        let value = json!({"workspace_epoch": EPOCH, "nodes": nodes, "next_after_id": next});
        let parsed: InventoryPage = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
    for field in ["workspace_epoch", "nodes", "next_after_id"] {
        let mut value = json!({"workspace_epoch": EPOCH, "nodes": [], "next_after_id": null});
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<InventoryPage>(value).is_err(),
            "{field}"
        );
    }
    for (field, invalid) in [
        ("workspace_epoch", json!(ROOT)),
        ("next_after_id", json!(EPOCH)),
        ("snapshot_id", json!("unsupported")),
    ] {
        let mut value = json!({"workspace_epoch": EPOCH, "nodes": [], "next_after_id": null});
        value[field] = invalid;
        assert!(
            serde_json::from_value::<InventoryPage>(value).is_err(),
            "{field}"
        );
    }
}

#[test]
fn created_receipt_still_rejects_root_and_deleted_node_state() {
    for mut node in [state("folder", true), state("folder", false)] {
        if node["parent_id"].is_string() {
            node["deleted"] = json!(true);
        }
        let value = json!({"status": "created", "operation_id": FILE, "operation_seq": "1",
            "workspace_epoch": EPOCH, "commit_seq": "1", "node": node});
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    }
}

#[test]
fn node_and_inventory_schema_keep_nullable_fields_required() {
    let node = serde_json::to_value(schemars::schema_for!(NodeState)).unwrap();
    let required = node["required"].as_array().unwrap();
    for field in [
        "parent_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        assert!(required.contains(&json!(field)), "{field}");
    }
    let page = serde_json::to_value(schemars::schema_for!(InventoryPage)).unwrap();
    assert!(
        page["required"]
            .as_array()
            .unwrap()
            .contains(&json!("next_after_id"))
    );
    let declaration = NodeState::decl(&Config::default());
    assert!(declaration.contains("size: string | null"));
    assert!(declaration.contains("children_generation: string"));
    assert!(declaration.contains("last_change_seq: string"));
    assert!(InventoryPage::decl(&Config::default()).contains("next_after_id: string | null"));
}
