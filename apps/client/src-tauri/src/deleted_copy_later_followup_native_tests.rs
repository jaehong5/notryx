include!("deleted_draft_copy_followup_native_tests.rs");

async fn later_followup(rig: &DeletedCopyRig) -> OutboxOperation {
    let updated = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_checkpoint(&rig.binding, &rig.document, 3)
        .await
        .unwrap();
    assert_eq!(updated.operation_id, rig.followup.operation_id);
    assert_eq!(updated.operation_seq, rig.followup.operation_seq);
    assert_eq!(updated.generation, 3);
    assert_eq!(updated.bytes, LATER);
    updated
}

fn later_request(rig: &DeletedCopyRig, operation: &OutboxOperation) -> FollowupRequest {
    let mut value = serde_json::to_value(followup_request(rig)).unwrap();
    value["followup_generation"] = serde_json::json!(operation.generation.to_string());
    value["followup_hash"] = serde_json::json!(operation.payload_hash);
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn deleted_copy_later_followup_status_and_approval_preserve_newer_edit_and_confirmed_copy() {
    let rig = deleted_copy_rig().await;
    let updated = later_followup(&rig).await;
    let status = copy_status(&rig).await;
    assert_eq!(status["generation"], rig.intent.generation.to_string());
    assert_eq!(
        status["blocking_followup"]["blocker_operation_id"],
        rig.original.operation_id
    );
    assert_eq!(
        status["blocking_followup"]["followup_operation_id"],
        updated.operation_id
    );
    assert_eq!(
        status["blocking_followup"]["followup_operation_seq"],
        updated.operation_seq.to_string()
    );
    assert_eq!(status["blocking_followup"]["followup_generation"], "3");
    assert_eq!(
        status["blocking_followup"]["followup_hash"],
        updated.payload_hash
    );
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_outbox_operation(&rig.binding, &updated.operation_id)
            .await
            .unwrap(),
        Some(updated.clone())
    );
    let request = later_request(&rig, &updated);
    for field in ["followup_generation", "followup_hash"] {
        let mut value = serde_json::to_value(&request).unwrap();
        value[field] = if field == "followup_generation" {
            serde_json::json!("2")
        } else {
            serde_json::json!(notryx_protocol::content_hash(CONFIRMED))
        };
        assert!(
            approve_followup(&rig, serde_json::from_value(value).unwrap())
                .await
                .is_err(),
            "{field}"
        );
        assert_eq!(
            rig.runtime
                .0
                .lock()
                .await
                .store
                .read_outbox_operation(&rig.binding, &updated.operation_id)
                .await
                .unwrap(),
            Some(updated.clone())
        );
    }
    let approved = approve_followup(&rig, request).await.unwrap();
    assert_eq!(approved["operation_id"], updated.operation_id);
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_outbox_operation(&rig.binding, &updated.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.abandon_state, OutboxAbandonState::Requested);
    assert_eq!(current.bytes, LATER);
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap(),
        Some(rig.intent.clone())
    );
    let checkpoint = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.bytes, LATER);
    assert_eq!(checkpoint.generation, 3);
    assert_eq!(checkpoint.base_bytes, BASE);
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.unrelated.operation_id)
            .await
            .unwrap(),
        Some(rig.unrelated.clone())
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_later_followup_rejects_payload_changed_after_display() {
    let rig = deleted_copy_rig().await;
    let updated = later_followup(&rig).await;
    let request = later_request(&rig, &updated);
    let edit_session = {
        let state = rig.runtime.0.lock().await;
        state
            .product_sessions
            .iter()
            .find(|(_, session)| {
                session.document_id == rig.document && session.binding == rig.binding
            })
            .unwrap()
            .0
            .clone()
    };
    let newer = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.document,
                &edit_session,
                4,
                b"after approval screen\n",
            )
            .await
            .unwrap();
        state
            .store
            .queue_checkpoint(&rig.binding, &rig.document, 4)
            .await
            .unwrap()
    };
    assert_eq!(newer.operation_id, updated.operation_id);
    assert_ne!(newer.payload_hash, updated.payload_hash);
    assert!(approve_followup(&rig, request).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &newer.operation_id)
            .await
            .unwrap(),
        Some(newer)
    );
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap(),
        Some(rig.intent.clone())
    );
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"after approval screen\n"
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}
