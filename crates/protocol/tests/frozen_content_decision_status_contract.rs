use notryx_protocol::WorkspaceFrozenContentDecisionStatusResponse;
use serde_json::{Value, json};

fn response() -> Value {
    let binding = json!({
        "server_origin": "https://example.invalid",
        "workspace_id": "workspace",
        "workspace_epoch": "epoch",
        "device_id": "device",
        "device_stream_id": "stream"
    });
    json!({
        "binding": binding,
        "old_binding": binding,
        "document_id": "document",
        "old_operation_id": "old-operation",
        "decision": null,
        "dismissed": null
    })
}

#[test]
fn frozen_content_decision_status_contract_requires_nullable_decision() {
    let value = response();
    let decoded: WorkspaceFrozenContentDecisionStatusResponse =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    let mut missing = value;
    missing.as_object_mut().unwrap().remove("decision");
    assert!(
        serde_json::from_value::<WorkspaceFrozenContentDecisionStatusResponse>(missing).is_err()
    );
    let mut missing_dismissed = response();
    missing_dismissed
        .as_object_mut()
        .unwrap()
        .remove("dismissed");
    assert!(
        serde_json::from_value::<WorkspaceFrozenContentDecisionStatusResponse>(missing_dismissed)
            .is_err()
    );
}

#[test]
fn frozen_content_decision_status_contract_accepts_checked_record_and_rejects_invalid_fields() {
    let mut value = response();
    value["decision"] = json!({
        "operation_id": "new-operation", "operation_seq": "3", "status": "queued",
        "approved_generation": "2", "approved_hash": format!("sha256:{}", "a".repeat(64))
    });
    let decoded: WorkspaceFrozenContentDecisionStatusResponse =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    for (field, invalid) in [
        ("operation_seq", json!("0")),
        ("approved_generation", json!("00")),
        ("approved_hash", json!("bad")),
        ("status", json!("unknown")),
    ] {
        let mut malformed = value.clone();
        malformed["decision"][field] = invalid;
        assert!(
            serde_json::from_value::<WorkspaceFrozenContentDecisionStatusResponse>(malformed)
                .is_err()
        );
    }
}

#[test]
fn frozen_content_decision_status_contract_schema_requires_nullable_decision() {
    let schema = serde_json::to_value(schemars::schema_for!(
        WorkspaceFrozenContentDecisionStatusResponse
    ))
    .unwrap();
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("decision"))
    );
    let field = &schema["properties"]["decision"];
    let nullable = field["anyOf"]
        .as_array()
        .is_some_and(|branches| branches.iter().any(|branch| branch["type"] == "null"));
    assert!(nullable, "decision schema must allow null: {field}");
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("dismissed"))
    );
    let field = &schema["properties"]["dismissed"];
    let nullable = field["anyOf"]
        .as_array()
        .is_some_and(|branches| branches.iter().any(|branch| branch["type"] == "null"));
    assert!(nullable, "dismissed schema must allow null: {field}");
}

#[test]
fn frozen_content_decision_status_contract_accepts_dismissed_and_rejects_invalid_fields() {
    let mut value = response();
    value["dismissed"] = json!({
        "operation_id": "old-operation", "operation_seq": "3", "operation_status": "sent", "abandon_state": "none"
    });
    let decoded: WorkspaceFrozenContentDecisionStatusResponse =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    for (field, invalid) in [
        ("operation_seq", json!("0")),
        ("operation_status", json!("committed")),
        ("abandon_state", json!("unknown")),
    ] {
        let mut malformed = value.clone();
        malformed["dismissed"][field] = invalid;
        assert!(
            serde_json::from_value::<WorkspaceFrozenContentDecisionStatusResponse>(malformed)
                .is_err()
        );
    }
}

#[test]
fn frozen_content_decision_status_contract_rejects_apply_and_dismiss_together() {
    let mut value = response();
    value["decision"] = json!({
        "operation_id": "new-operation", "operation_seq": "3", "status": "queued",
        "approved_generation": "2", "approved_hash": format!("sha256:{}", "a".repeat(64))
    });
    value["dismissed"] = json!({
        "operation_id": "old-operation", "operation_seq": "1", "operation_status": "sent", "abandon_state": "none"
    });
    assert!(serde_json::from_value::<WorkspaceFrozenContentDecisionStatusResponse>(value).is_err());
    let schema = serde_json::to_value(schemars::schema_for!(
        WorkspaceFrozenContentDecisionStatusResponse
    ))
    .unwrap();
    assert_eq!(
        schema["not"]["properties"]["decision"]["type"],
        json!("object")
    );
    assert_eq!(
        schema["not"]["properties"]["dismissed"]["type"],
        json!("object")
    );
    assert_eq!(schema["not"]["required"], json!(["decision", "dismissed"]));
}
