include!("frozen_content_contract.rs");

#[test]
fn frozen_content_abandon_state_is_required_and_separate_from_content_outcome() {
    for (status, abandon_state, outcome_unknown) in [
        ("queued", "none", false),
        ("sent", "none", true),
        ("abandoning", "requested", false),
        ("abandoning", "sent", false),
    ] {
        let mut value = response();
        value["operation_status"] = json!(status);
        value["abandon_state"] = json!(abandon_state);
        value["outcome_unknown"] = json!(outcome_unknown);
        assert_eq!(decode_response(value.clone()).unwrap(), value);
    }
    let mut missing = response();
    missing.as_object_mut().unwrap().remove("abandon_state");
    assert!(decode_response(missing).is_err());
    for invalid in [json!(null), json!("other"), json!(true)] {
        let mut value = response();
        value["abandon_state"] = invalid;
        assert!(decode_response(value).is_err());
    }
}

#[test]
fn frozen_content_rejects_contradictory_abandon_and_content_outcome_states() {
    for (status, abandon_state, outcome_unknown) in [
        ("queued", "requested", false),
        ("queued", "sent", false),
        ("sent", "requested", true),
        ("sent", "sent", true),
        ("abandoning", "none", false),
        ("queued", "none", true),
        ("sent", "none", false),
        ("abandoning", "requested", true),
        ("abandoning", "sent", true),
    ] {
        let mut value = response();
        value["operation_status"] = json!(status);
        value["abandon_state"] = json!(abandon_state);
        value["outcome_unknown"] = json!(outcome_unknown);
        assert!(
            decode_response(value).is_err(),
            "{status} {abandon_state} {outcome_unknown}"
        );
    }
}
