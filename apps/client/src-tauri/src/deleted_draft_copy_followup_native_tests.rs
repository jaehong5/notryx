include!("deleted_draft_copy_blocked_followup_tests.rs");

type FollowupRequest = notryx_protocol::WorkspaceDeletedDraftCopyFollowupAbandonRequest;

fn followup_request(rig: &DeletedCopyRig) -> FollowupRequest {
    serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.document,
        "node_id": rig.intent.node_id,
        "create_operation_id": rig.intent.create_operation_id,
        "blocker_operation_id": rig.original.operation_id,
        "followup_operation_id": rig.followup.operation_id,
        "followup_operation_seq": rig.followup.operation_seq.to_string(),
        "followup_generation": rig.followup.generation.to_string(),
        "followup_hash": rig.followup.payload_hash,
    }))
    .unwrap()
}

async fn copy_status(rig: &DeletedCopyRig) -> serde_json::Value {
    let request = serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.document,
    }))
    .unwrap();
    serde_json::to_value(
        crate::remote_deleted_copy_native::status_for_renderer(&rig.runtime, request)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

async fn approve_followup(
    rig: &DeletedCopyRig,
    request: FollowupRequest,
) -> Result<serde_json::Value, String> {
    let result = crate::remote_deleted_copy_native::request_followup_abandon_for_renderer(
        &rig.runtime,
        request,
    )
    .await?;
    Ok(serde_json::to_value(result).unwrap())
}

#[tokio::test]
async fn deleted_copy_followup_native_status_displays_exact_block_before_any_approval() {
    let rig = deleted_copy_rig().await;
    let status = copy_status(&rig).await;
    assert_eq!(
        status["blocking_followup"]["blocker_operation_id"],
        rig.original.operation_id
    );
    assert_eq!(
        status["blocking_followup"]["followup_operation_id"],
        rig.followup.operation_id
    );
    assert_eq!(
        status["blocking_followup"]["followup_operation_seq"],
        rig.followup.operation_seq.to_string()
    );
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_outbox_operation(&rig.binding, &rig.followup.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current, rig.followup);
    assert_eq!(current.abandon_state, OutboxAbandonState::None);
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.unrelated.operation_id)
            .await
            .unwrap(),
        Some(rig.unrelated.clone())
    );
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap(),
        Some(rig.intent.clone())
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_followup_native_requires_explicit_exact_approval_and_preserves_evidence() {
    let rig = deleted_copy_rig().await;
    let request = followup_request(&rig);
    for field in [
        "owner",
        "binding",
        "node_id",
        "create_operation_id",
        "blocker_operation_id",
        "followup_operation_id",
    ] {
        let mut value = serde_json::to_value(&request).unwrap();
        match field {
            "owner" => value["renderer_session_id"] = serde_json::json!("old-owner"),
            "binding" => {
                value["binding"]["device_stream_id"] = serde_json::json!(Uuid::new_v4().to_string())
            }
            _ => value[field] = serde_json::json!(Uuid::new_v4().to_string()),
        }
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
                .read_outbox_operation(&rig.binding, &rig.followup.operation_id)
                .await
                .unwrap(),
            Some(rig.followup.clone())
        );
    }
    let result = approve_followup(&rig, request.clone()).await.unwrap();
    assert_eq!(result["operation_id"], rig.followup.operation_id);
    assert_eq!(
        approve_followup(&rig, request.clone()).await.unwrap(),
        result
    );
    let mut state = rig.runtime.0.lock().await;
    let followup = state
        .store
        .read_outbox_operation(&rig.binding, &rig.followup.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(followup.abandon_state, OutboxAbandonState::Requested);
    assert_eq!(followup.operation_seq, rig.followup.operation_seq);
    assert_eq!(followup.bytes, rig.followup.bytes);
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.original.operation_id)
            .await
            .unwrap()
            .unwrap()
            .failure_kind,
        Some(OutboxFailureKind::NodeDeleted)
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.unrelated.operation_id)
            .await
            .unwrap(),
        Some(rig.unrelated.clone())
    );
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
    assert_eq!(checkpoint.base_bytes, BASE);
    drop(state);
    let acknowledged = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(acknowledged.status, WorkspaceSendStatus::Abandoned);
    assert_eq!(
        acknowledged.operation_id.as_deref(),
        Some(rig.followup.operation_id.as_str())
    );
    assert_eq!(approve_followup(&rig, request).await.unwrap(), result);
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap(),
        rig.followup.operation_seq
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_followup_native_preserves_unrelated_transmission_blocks() {
    for reason in [
        crate::send::TransmissionBlock::AuthRequired,
        crate::send::TransmissionBlock::ResyncRequired,
    ] {
        let rig = deleted_copy_rig().await;
        rig.runtime
            .0
            .lock()
            .await
            .transmission_gate
            .block(&rig.binding, reason);
        let _ = approve_followup(&rig, followup_request(&rig)).await;
        let state = rig.runtime.0.lock().await;
        assert!(state.transmission_gate.has_block(&rig.binding, reason));
        drop(state);
        rig.server.shutdown().await.unwrap();
    }
}
