use notryx_protocol::{InventoryPage, NodeMetadata, NodeState, OperationReceipt, TrashPage};
use serde_json::{Value, json};

fn assert_required_nullable_string(schema: &Value, field: &str) {
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!(field)),
        "missing required field {field}"
    );
    assert_eq!(
        schema["properties"][field]["type"],
        json!(["string", "null"]),
        "{field} must accept an explicit null"
    );
}

#[test]
fn node_schemas_accept_explicit_null_without_making_fields_optional() {
    let node = serde_json::to_value(schemars::schema_for!(NodeState)).unwrap();
    for field in [
        "parent_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        assert_required_nullable_string(&node, field);
    }
    let metadata = serde_json::to_value(schemars::schema_for!(NodeMetadata)).unwrap();
    assert_required_nullable_string(&metadata, "parent_id");
}

#[test]
fn inventory_cursor_schema_accepts_null_and_requires_the_key() {
    let inventory = serde_json::to_value(schemars::schema_for!(InventoryPage)).unwrap();
    assert_required_nullable_string(&inventory, "next_after_id");
}

#[test]
fn trash_cursor_schema_accepts_null_and_requires_the_key() {
    let trash = serde_json::to_value(schemars::schema_for!(TrashPage)).unwrap();
    assert_required_nullable_string(&trash, "next_after_id");
}

#[test]
fn receipt_node_schema_accepts_the_same_required_nulls() {
    let receipt = serde_json::to_value(schemars::schema_for!(OperationReceipt)).unwrap();
    let node = &receipt["$defs"]["NodeState"];
    for field in [
        "parent_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        assert_required_nullable_string(node, field);
    }
}
