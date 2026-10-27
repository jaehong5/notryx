use notryx_protocol::{AbandonRequest, AckRequest, AckResponse, OperationReceipt};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const EPOCH: &str = "550e8400-e29b-41d4-a716-446655440000";
const STREAM: &str = "2a3d41b3-e91f-4f4d-961a-772f17ac94ba";
const OPERATION: &str = "019a1000-0000-7000-8000-000000000042";

fn ack() -> Value {
    json!({"workspace_epoch": EPOCH, "device_stream_id": STREAM, "ack_through": "42"})
}

fn abandon() -> Value {
    json!({"workspace_epoch": EPOCH, "device_stream_id": STREAM,
        "operation_id": OPERATION, "operation_seq": "43"})
}

#[test]
fn stream_requests_preserve_bindings_and_decimal_u64_boundaries() {
    for number in ["0", "42", "9007199254740993", "18446744073709551615"] {
        let mut expected = ack();
        expected["ack_through"] = json!(number);
        let parsed: AckRequest = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
    for number in ["1", "43", "9007199254740993", "18446744073709551615"] {
        let mut expected = abandon();
        expected["operation_seq"] = json!(number);
        let parsed: AbandonRequest = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
    let expected = json!({"workspace_epoch": EPOCH, "device_stream_id": STREAM,
        "last_seq": "18446744073709551615", "retired_through": "9007199254740993"});
    let parsed: AckResponse = serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
}

#[test]
fn stream_requests_reject_wrong_uuid_versions_variants_and_missing_fields() {
    for invalid in [
        "",
        "invalid",
        OPERATION,
        "550e8400-e29b-41d4-0716-446655440000",
    ] {
        for field in ["workspace_epoch", "device_stream_id"] {
            let mut request = ack();
            request[field] = json!(invalid);
            assert!(
                serde_json::from_value::<AckRequest>(request).is_err(),
                "ack {field}"
            );
            let mut request = abandon();
            request[field] = json!(invalid);
            assert!(
                serde_json::from_value::<AbandonRequest>(request).is_err(),
                "abandon {field}"
            );
        }
    }
    for field in ["workspace_epoch", "device_stream_id", "ack_through"] {
        let mut request = ack();
        request.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<AckRequest>(request).is_err());
    }
    for field in [
        "workspace_epoch",
        "device_stream_id",
        "operation_id",
        "operation_seq",
    ] {
        let mut request = abandon();
        request.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<AbandonRequest>(request).is_err());
    }
    let mut request = abandon();
    request["operation_id"] = json!(EPOCH);
    assert!(serde_json::from_value::<AbandonRequest>(request).is_err());
}

#[test]
fn stream_requests_reject_unknown_fields_and_invalid_sequences() {
    for invalid in [
        json!(0),
        json!(null),
        json!(""),
        json!("-1"),
        json!("+1"),
        json!("1.5"),
        json!("1e2"),
        json!(" 1"),
        json!("18446744073709551616"),
    ] {
        let mut request = ack();
        request["ack_through"] = invalid.clone();
        assert!(serde_json::from_value::<AckRequest>(request).is_err());
        let mut request = abandon();
        request["operation_seq"] = invalid;
        assert!(serde_json::from_value::<AbandonRequest>(request).is_err());
    }
    for zero in ["0", "000"] {
        let mut request = abandon();
        request["operation_seq"] = json!(zero);
        assert!(serde_json::from_value::<AbandonRequest>(request).is_err());
    }
    let mut request = ack();
    request["unexpected"] = json!(true);
    assert!(serde_json::from_value::<AckRequest>(request).is_err());
    let mut request = abandon();
    request["unexpected"] = json!(true);
    assert!(serde_json::from_value::<AbandonRequest>(request).is_err());
}

#[test]
fn abandoned_receipt_is_a_closed_terminal_outcome_with_identity() {
    let expected = json!({"status": "abandoned", "workspace_epoch": EPOCH,
        "operation_id": OPERATION, "operation_seq": "43"});
    let parsed: OperationReceipt = serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    let mut invalid = expected;
    invalid["bytes"] = json!("not part of cancellation");
    assert!(serde_json::from_value::<OperationReceipt>(invalid).is_err());
}

#[test]
fn stream_contract_schemas_and_types_keep_numbers_as_strings() {
    for schema in [
        serde_json::to_value(schemars::schema_for!(AckRequest)).unwrap(),
        serde_json::to_value(schemars::schema_for!(AbandonRequest)).unwrap(),
        serde_json::to_value(schemars::schema_for!(AckResponse)).unwrap(),
    ] {
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["device_stream_id"]["type"], "string");
    }
    let config = Config::default();
    assert!(AckRequest::decl(&config).contains("ack_through: string"));
    assert!(AckResponse::decl(&config).contains("retired_through: string"));
    assert!(AbandonRequest::decl(&config).contains("operation_seq: string"));
}
