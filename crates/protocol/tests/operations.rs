use notryx_protocol::{ContentPrecondition, OperationError, OperationReceipt};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const OPERATION: &str = "019a1000-0000-7000-8000-000000000042";
const NODE: &str = "019a1000-0000-7000-8000-000000000001";
const EPOCH: &str = "550e8400-e29b-41d4-a716-446655440000";

fn receipt(status: &str) -> Value {
    let mut value = json!({
        "status": status,
        "operation_id": OPERATION,
        "operation_seq": "42",
        "workspace_epoch": EPOCH,
    });
    if status == "committed" {
        value["node_id"] = json!(NODE);
        value["content_rev"] = json!("opaque:c.7");
        value["content_etag"] = json!("\"opaque:c.7\"");
        value["meta_rev"] = json!("opaque:m.2");
        value["meta_etag"] = json!("\"opaque:m.2\"");
        value["node_etag"] = json!("\"opaque:n.9\"");
        value["content_hash"] =
            json!("sha256:5332a760c3e1779553d90dbbc45974255e6c8eb06b9fc962c826860204c77646");
        value["commit_seq"] = json!("9007199254740993");
    } else if status == "precondition_failed" {
        value["current_content_rev"] = json!("opaque:c.8");
        value["current_content_etag"] = json!("\"opaque:c.8\"");
    }
    value
}

#[test]
fn operation_receipts_preserve_each_recorded_outcome_and_large_sequences() {
    for status in [
        "committed",
        "precondition_failed",
        "not_found",
        "not_applied",
        "abandoned",
        "pending",
    ] {
        let expected = receipt(status);
        let parsed: OperationReceipt = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
    }
    for valid in ["1", "9007199254740993", "18446744073709551615"] {
        let mut value = receipt("pending");
        value["operation_seq"] = json!(valid);
        let parsed: OperationReceipt = serde_json::from_value(value).unwrap();
        assert_eq!(
            serde_json::to_value(parsed).unwrap()["operation_seq"],
            valid
        );
    }
}

#[test]
fn operation_receipts_reject_unknown_fields_status_and_incomplete_outcomes() {
    for status in [
        "committed",
        "precondition_failed",
        "not_found",
        "not_applied",
        "abandoned",
        "pending",
    ] {
        let mut value = receipt(status);
        value["unexpected"] = json!(true);
        assert!(
            serde_json::from_value::<OperationReceipt>(value).is_err(),
            "{status}"
        );
        let mut value = receipt(status);
        value.as_object_mut().unwrap().remove("operation_id");
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    }
    for invalid in ["saved", "Committed", "", "rejected"] {
        assert!(serde_json::from_value::<OperationReceipt>(receipt(invalid)).is_err());
    }
    for field in [
        "node_id",
        "content_rev",
        "content_etag",
        "meta_rev",
        "meta_etag",
        "node_etag",
        "content_hash",
        "commit_seq",
    ] {
        let mut value = receipt("committed");
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<OperationReceipt>(value).is_err(),
            "{field}"
        );
    }
}

#[test]
fn operation_receipts_validate_identity_and_positive_decimal_operation_sequences() {
    for status in [
        "committed",
        "precondition_failed",
        "not_found",
        "not_applied",
        "abandoned",
        "pending",
    ] {
        for invalid in [
            json!(0),
            json!(null),
            json!(""),
            json!("0"),
            json!("000"),
            json!("-1"),
            json!("+1"),
            json!("1.5"),
            json!("1e2"),
            json!(" 1"),
            json!("18446744073709551616"),
        ] {
            let mut value = receipt(status);
            value["operation_seq"] = invalid;
            assert!(
                serde_json::from_value::<OperationReceipt>(value).is_err(),
                "{status}"
            );
        }
        for (field, invalid) in [
            ("operation_id", EPOCH),
            ("operation_id", "019a1000-0000-7000-0000-000000000042"),
            ("workspace_epoch", OPERATION),
        ] {
            let mut value = receipt(status);
            value[field] = json!(invalid);
            assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
        }
    }
    let mut value = receipt("committed");
    value["node_id"] = json!(EPOCH);
    assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    for field in ["content_rev", "meta_rev"] {
        let mut value = receipt("committed");
        value[field] = json!("");
        assert!(serde_json::from_value::<OperationReceipt>(value).is_err());
    }
}

#[test]
fn operation_errors_keep_the_common_error_contract_and_the_recorded_receipt() {
    let valid = json!({
        "code": "content_revision_mismatch",
        "message": "현재 본문을 확인해 주세요.",
        "request_id": NODE,
        "operation": receipt("precondition_failed"),
    });
    let parsed: OperationError = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), valid);
    for missing in ["code", "message", "request_id", "operation"] {
        let mut value = valid.clone();
        value.as_object_mut().unwrap().remove(missing);
        assert!(serde_json::from_value::<OperationError>(value).is_err());
    }
    for invalid in [EPOCH, "not-a-uuid", "019a1000-0000-7000-0000-000000000001"] {
        let mut value = valid.clone();
        value["request_id"] = json!(invalid);
        assert!(serde_json::from_value::<OperationError>(value).is_err());
    }
    let mut value = valid;
    value["unexpected"] = json!(true);
    assert!(serde_json::from_value::<OperationError>(value).is_err());
}

#[test]
fn typed_content_preconditions_preserve_the_opaque_revision_and_bound_its_bytes() {
    for literal in [
        "opaque:content:A-7",
        "c.7d38b92f",
        "한글:revision",
        "back\\slash",
    ] {
        let condition = ContentPrecondition::parse(&format!("\"{literal}\"")).unwrap();
        let revision = condition.revision().unwrap();
        assert_eq!(revision.as_str(), literal);
        assert!(condition.matches(&revision).unwrap());
    }
    let maximum = "a".repeat(256);
    assert_eq!(
        ContentPrecondition::parse(&format!("\"{maximum}\""))
            .unwrap()
            .revision()
            .unwrap()
            .as_str(),
        maximum
    );
    for value in [String::new(), "a".repeat(257), "한".repeat(86)] {
        assert!(
            ContentPrecondition::parse(&format!("\"{value}\""))
                .unwrap()
                .revision()
                .is_err()
        );
    }
}

#[test]
fn generated_operation_contracts_expose_closed_outcomes_and_decimal_strings() {
    let schema = serde_json::to_value(schemars::schema_for!(OperationReceipt)).unwrap();
    let alternatives = schema["oneOf"].as_array().unwrap();
    assert_eq!(alternatives.len(), 23);
    for alternative in alternatives {
        assert_eq!(alternative["additionalProperties"], false);
        assert_eq!(alternative["properties"]["operation_seq"]["type"], "string");
        assert_eq!(
            alternative["properties"]["operation_seq"]["pattern"],
            "^0*[1-9][0-9]*$"
        );
    }
    let declaration = OperationReceipt::decl(&Config::default());
    assert!(declaration.contains("operation_seq: string"));
    assert!(declaration.contains("commit_seq: string"));
    assert!(declaration.contains("recovery_conflict"));
    assert!(OperationError::decl(&Config::default()).contains("operation: OperationReceipt"));
}
