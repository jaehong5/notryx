include!("conflict_merge_queue_native_tests.rs");

async fn read_queued_merge(
    rig: &Rig,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    version: u64,
) -> Result<Option<OutboxOperation>, String> {
    crate::conflict_snapshot_native::read_queued_merge_for_renderer(
        &rig.runtime,
        owner,
        binding,
        document_id,
        operation_id,
        version,
    )
    .await
}

#[tokio::test]
async fn merge_recovery_status_before_queue_does_not_create_an_operation() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    assert!(
        read_queued_merge(
            &rig,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            intent.version
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_merge_queue_sources(&rig).await;
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .product_sessions
            .contains_key(&rig.edit_session)
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_recovery_after_lost_queue_response_returns_same_operation_preserves_later_draft_and_sends()
 {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let queued = queue_approved_merge(&rig, intent.version).await.unwrap();
    let operation_id = queued.operation_id.clone();
    let seq = queued.operation_seq;
    let generation = queued.generation;
    let (resolution, opened) =
        crate::conflict_snapshot_native::reopen_conflict_checkpoint_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &rig.edit_session,
        )
        .await
        .unwrap();
    assert!(matches!(
        resolution,
        ContentConflictResolutionState::Pending { .. }
    ));
    assert_ne!(opened.edit_session_id, rig.edit_session);
    {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.document,
                &opened.edit_session_id,
                generation + 1,
                LATER,
            )
            .await
            .unwrap();
    }
    let status = read_queued_merge(
        &rig,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.operation.operation_id,
        intent.version,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(status.operation_id, operation_id);
    assert_eq!(status.operation_seq, seq);
    assert_eq!(status.generation, generation);
    assert_eq!(status.bytes, MERGED);
    assert_eq!(
        status.expected_revision.as_deref(),
        Some(intent.remote_revision.as_str())
    );
    assert_eq!(status.status, OutboxStatus::Queued);
    assert_eq!(
        read_queued_merge(
            &rig,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            intent.version
        )
        .await
        .unwrap(),
        Some(status)
    );
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, generation + 1);
    assert_eq!(draft.bytes, LATER);
    assert_eq!(draft.base_bytes, REMOTE);
    assert!(state.product_sessions.contains_key(&opened.edit_session_id));
    drop(state);
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Committed);
    assert_eq!(sent.operation_id.as_deref(), Some(operation_id.as_str()));
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        MERGED
    );
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap()
            >= seq
    );
    assert!(
        matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(), ContentConflictResolutionState::Completed { generation: saved, frozen:false } if saved == generation)
    );
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, generation + 1);
    assert_eq!(draft.bytes, LATER);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_recovery_status_rejects_stale_owner_binding_document_operation_and_version() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let queued = queue_approved_merge(&rig, intent.version).await.unwrap();
    let mut wrong_binding = rig.binding.clone();
    wrong_binding.device_stream_id = Uuid::new_v4().to_string();
    let wrong_document = Uuid::nil().to_string();
    for (owner, binding, document, operation, version) in [
        (
            "wrong-owner",
            &rig.binding,
            rig.document.as_str(),
            rig.operation.operation_id.as_str(),
            intent.version,
        ),
        (
            OWNER,
            &wrong_binding,
            rig.document.as_str(),
            rig.operation.operation_id.as_str(),
            intent.version,
        ),
        (
            OWNER,
            &rig.binding,
            wrong_document.as_str(),
            rig.operation.operation_id.as_str(),
            intent.version,
        ),
    ] {
        assert!(
            read_queued_merge(&rig, owner, binding, document, operation, version)
                .await
                .is_err()
        );
    }
    assert!(
        read_queued_merge(
            &rig,
            OWNER,
            &rig.binding,
            &rig.document,
            &Uuid::new_v4().to_string(),
            intent.version
        )
        .await
        .is_err()
    );
    assert!(
        read_queued_merge(
            &rig,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            intent.version + 1
        )
        .await
        .is_err()
    );
    let status = read_queued_merge(
        &rig,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.operation.operation_id,
        intent.version,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(status.operation_id, queued.operation_id);
    assert_eq!(status.bytes, MERGED);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
