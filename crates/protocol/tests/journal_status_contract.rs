use notryx_protocol::{AdminRequest, AdminResponse};
use serde_json::{Value, json};

fn status() -> Value {
    json!({"status":"ok","result":{
        "kind":"journal_status",
        "workspace_epoch":"550e8400-e29b-41d4-a716-446655440000",
        "head_seq":"2",
        "journal_floor_seq":"1",
        "logical_bytes":"9007199254740993",
        "budget_bytes":"536870912",
        "budget_exceeded":true,
        "accounting":"sqlite_blob_lengths_v1"
    }})
}

#[test]
fn journal_status_contract_roundtrips_exact_values_and_rejects_unknown_request_fields() {
    let request = json!({"command":"journal_status"});
    let decoded: AdminRequest = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), request);
    assert!(
        serde_json::from_value::<AdminRequest>(json!({"command":"journal_status","extra":true}))
            .is_err()
    );
    let response = status();
    let decoded: AdminResponse = serde_json::from_value(response.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), response);
}

#[test]
fn journal_status_contract_rejects_invalid_fields() {
    for (field, invalid) in [
        ("workspace_epoch", json!("invalid")),
        ("head_seq", json!("18446744073709551616")),
        ("journal_floor_seq", json!("-1")),
        ("logical_bytes", json!("18446744073709551616")),
        ("budget_bytes", json!("-1")),
        ("budget_exceeded", json!("true")),
        ("accounting", json!("physical_disk")),
    ] {
        let mut response = status();
        response["result"][field] = invalid;
        assert!(
            serde_json::from_value::<AdminResponse>(response).is_err(),
            "invalid {field} must be rejected"
        );
    }
    let mut extra = status();
    extra["result"]["unused"] = json!(true);
    assert!(serde_json::from_value::<AdminResponse>(extra).is_err());
}
