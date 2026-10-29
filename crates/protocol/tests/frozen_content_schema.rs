use notryx_protocol::WorkspaceFrozenContentInspection;
use serde_json::{Value, json};

fn schema() -> Value {
    serde_json::to_value(schemars::schema_for!(WorkspaceFrozenContentInspection)).unwrap()
}

#[test]
fn frozen_content_schema_requires_nullable_expected_revision() {
    let schema = schema();
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("expected_revision"))
    );
    assert_eq!(
        schema["properties"]["expected_revision"]["type"],
        json!(["string", "null"])
    );
}

#[test]
fn frozen_content_schema_limits_operation_status_to_inspectable_states() {
    let schema = schema();
    let field = &schema["properties"]["operation_status"];
    let status = match field.get("$ref").and_then(Value::as_str) {
        Some(reference) => schema
            .pointer(reference.strip_prefix('#').unwrap())
            .unwrap(),
        None => field,
    };
    assert_eq!(status["type"], "string");
    let mut actual = status["enum"].as_array().unwrap().clone();
    actual.sort_by(|a, b| a.as_str().unwrap().cmp(b.as_str().unwrap()));
    assert_eq!(
        actual,
        vec![json!("abandoning"), json!("queued"), json!("sent")]
    );
}
