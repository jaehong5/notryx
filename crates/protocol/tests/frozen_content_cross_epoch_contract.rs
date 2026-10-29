include!("frozen_content_contract.rs");

use notryx_protocol::{
    WorkspaceFrozenContentCrossEpochApplyRequest, WorkspaceFrozenContentCrossEpochInspection,
};

fn cross_epoch_inspection() -> Value {
    json!({
        "inspected": response(),
        "current_target": {
            "node_id": "019a1000-0000-7000-8000-000000000002",
            "parent_id": "019a1000-0000-7000-8000-000000000001",
            "name": "note.md",
            "path": "/folder/note.md"
        }
    })
}

#[test]
fn frozen_content_cross_epoch_inspection_requires_snapshot_and_full_target() {
    let value = cross_epoch_inspection();
    let decoded: WorkspaceFrozenContentCrossEpochInspection =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    for field in ["inspected", "current_target"] {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceFrozenContentCrossEpochInspection>(missing).is_err(),
            "missing {field}"
        );
    }
    for field in ["node_id", "parent_id", "name", "path"] {
        let mut missing = value.clone();
        missing["current_target"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            serde_json::from_value::<WorkspaceFrozenContentCrossEpochInspection>(missing).is_err(),
            "missing target {field}"
        );
    }
    let mut extra = value;
    extra["current_target"]["remote_document"] = json!(true);
    assert!(serde_json::from_value::<WorkspaceFrozenContentCrossEpochInspection>(extra).is_err());
}

#[test]
fn frozen_content_cross_epoch_approval_requires_explicit_confirmation_field() {
    let value = json!({"renderer_session_id":"owner", "inspected":cross_epoch_inspection(), "same_node_is_not_identity":true});
    let parsed: WorkspaceFrozenContentCrossEpochApplyRequest =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    for field in [
        "renderer_session_id",
        "inspected",
        "same_node_is_not_identity",
    ] {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<WorkspaceFrozenContentCrossEpochApplyRequest>(missing)
                .is_err(),
            "missing {field}"
        );
    }
    let mut wrong = value;
    wrong["same_node_is_not_identity"] = json!("true");
    assert!(serde_json::from_value::<WorkspaceFrozenContentCrossEpochApplyRequest>(wrong).is_err());
}

#[test]
fn frozen_content_cross_epoch_schema_requires_every_confirmation_field() {
    let schema = serde_json::to_value(schemars::schema_for!(
        WorkspaceFrozenContentCrossEpochApplyRequest
    ))
    .unwrap();
    for field in [
        "renderer_session_id",
        "inspected",
        "same_node_is_not_identity",
    ] {
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field)),
            "missing request field {field}"
        );
    }
    let target = &schema["$defs"]["WorkspaceFrozenContentCrossEpochTarget"];
    for field in ["node_id", "parent_id", "name", "path"] {
        assert!(
            target["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field)),
            "missing target field {field}"
        );
    }
}
