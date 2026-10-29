include!("frozen_content_apply_role_tests.rs");

#[derive(Clone)]
struct DismissRequest {
    renderer_session_id: String,
    inspected: notryx_protocol::WorkspaceFrozenContentInspection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DismissReceipt {
    old_operation_id: String,
    old_operation_seq: String,
}

async fn dismiss_for_renderer(
    runtime: &Runtime,
    request: DismissRequest,
) -> Result<DismissReceipt, String> {
    let result = crate::frozen_content_native::dismiss_frozen_content_for_renderer(
        runtime,
        notryx_protocol::WorkspaceFrozenContentDismissRequest {
            renderer_session_id: request.renderer_session_id,
            inspected: request.inspected,
        },
    )
    .await?;
    Ok(DismissReceipt {
        old_operation_id: result.operation_id,
        old_operation_seq: result.operation_seq,
    })
}

async fn dismissal_status_for_renderer(
    runtime: &Runtime,
    renderer_owner: &str,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
) -> Result<Option<DismissReceipt>, String> {
    let result = crate::frozen_content_native::frozen_content_decision_status_for_renderer(
        runtime,
        notryx_protocol::WorkspaceFrozenContentDecisionStatusRequest {
            renderer_session_id: renderer_owner.into(),
            binding: crate::to_contract(current.clone()),
            old_binding: crate::to_contract(old.clone()),
            document_id: document.into(),
            old_operation_id: operation_id.into(),
        },
    )
    .await?;
    Ok(result.dismissed.map(|item| DismissReceipt {
        old_operation_id: item.operation_id,
        old_operation_seq: item.operation_seq,
    }))
}

async fn dismiss_request(rig: &Rig) -> DismissRequest {
    DismissRequest {
        renderer_session_id: "owner".into(),
        inspected: inspect_frozen_content_command_for_renderer(
            &rig.runtime,
            frozen_command_request(rig),
        )
        .await
        .unwrap(),
    }
}

async fn dismiss_status(rig: &Rig, owner: &str) -> Result<Option<DismissReceipt>, String> {
    dismissal_status_for_renderer(
        &rig.runtime,
        owner,
        &rig.current,
        &rig.old,
        &rig.document,
        &rig.sent.operation_id,
    )
    .await
}

#[tokio::test]
async fn frozen_content_dismiss_contract_reader_decision_preserves_sent_sources_and_recovers_offline()
 {
    let (rig, principal) = editor_rig().await;
    let request = dismiss_request(&rig).await;
    assert_eq!(
        request.inspected.operation_status,
        notryx_protocol::WorkspaceOutboxStatus::Sent
    );
    assert!(request.inspected.outcome_unknown);
    let before = command_sources(&rig).await;
    let db_path = rig
        ._directory
        .path()
        .join("frozen-content-inspection.sqlite");
    let watermarks = stream_watermarks(&db_path).await;
    let old_operation = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .read_outbox_operation(&rig.old, &rig.sent.operation_id)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(before.1.entries.len(), 1);
    assert!(before.2.entries.is_empty());
    change_role(&rig, &principal, WorkspaceRole::Reader).await;
    let result = dismiss_for_renderer(&rig.runtime, request.clone())
        .await
        .unwrap();
    assert_eq!(result.old_operation_id, rig.sent.operation_id);
    assert_eq!(result.old_operation_seq, rig.sent.operation_seq.to_string());
    assert!(command_sources(&rig).await == before);
    assert_eq!(stream_watermarks(&db_path).await, watermarks);
    {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_outbox_operation(&rig.old, &rig.sent.operation_id)
                .await
                .unwrap()
                .unwrap()
                == old_operation
        );
    }
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    let Rig {
        _directory: directory,
        server,
        runtime,
        session,
        old,
        current,
        document,
        ..
    } = rig;
    server.shutdown().await.unwrap();
    drop(runtime);
    drop(session);
    let store = ClientStore::open(directory.path().join("frozen-content-inspection.sqlite"))
        .await
        .unwrap();
    let runtime = Runtime(Arc::new(Mutex::new(RuntimeState::new(
        store,
        "new-owner".into(),
    ))));
    assert_eq!(
        dismissal_status_for_renderer(
            &runtime,
            "new-owner",
            &current,
            &old,
            &document,
            &result.old_operation_id
        )
        .await
        .unwrap(),
        Some(result.clone())
    );
    let mut retried = request;
    retried.renderer_session_id = "new-owner".into();
    assert_eq!(
        dismiss_for_renderer(&runtime, retried).await.unwrap(),
        result
    );
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap()
            .unwrap(),
        before.0
    );
    assert_eq!(
        state.store.list_outbox(&old, 0, 100).await.unwrap(),
        before.1
    );
    assert_eq!(
        state.store.list_outbox(&current, 0, 100).await.unwrap(),
        before.2
    );
}

#[tokio::test]
async fn frozen_content_dismiss_contract_leaves_successor_and_draft_untouched() {
    let rig = rig().await;
    let before = command_sources(&rig).await;
    assert_eq!(before.1.entries.len(), 2);
    let request = dismiss_request(&rig).await;
    let result = dismiss_for_renderer(&rig.runtime, request).await.unwrap();
    assert_eq!(result.old_operation_id, rig.sent.operation_id);
    assert!(command_sources(&rig).await == before);
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), Some(result));
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_dismiss_contract_remote_change_does_not_invalidate_old_operation_choice() {
    let rig = single_operation_rig().await;
    let request = dismiss_request(&rig).await;
    let before = command_sources(&rig).await;
    set_remote_bytes(&rig, b"another device changed the server\n").await;
    let result = dismiss_for_renderer(&rig.runtime, request).await.unwrap();
    assert_eq!(result.old_operation_id, rig.sent.operation_id);
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        b"another device changed the server\n"
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_dismiss_contract_first_offline_request_does_not_create_decision() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    let request = dismiss_request(&rig).await;
    {
        let mut state = rig.runtime.0.lock().await;
        state.session = None;
    }
    assert_eq!(
        dismiss_for_renderer(&rig.runtime, request)
            .await
            .unwrap_err(),
        "not_connected"
    );
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
    assert!(command_sources(&rig).await == before);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_dismiss_contract_role_lookup_failure_does_not_create_decision() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    let request = dismiss_request(&rig).await;
    let Rig {
        _directory,
        server,
        runtime,
        old,
        current,
        document,
        sent,
        ..
    } = rig;
    server.shutdown().await.unwrap();
    assert_eq!(
        dismiss_for_renderer(&runtime, request).await.unwrap_err(),
        "Network"
    );
    assert_eq!(
        dismissal_status_for_renderer(
            &runtime,
            "owner",
            &current,
            &old,
            &document,
            &sent.operation_id
        )
        .await
        .unwrap(),
        None
    );
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap()
            .unwrap(),
        before.0
    );
    assert_eq!(
        state.store.list_outbox(&old, 0, 100).await.unwrap(),
        before.1
    );
    assert_eq!(
        state.store.list_outbox(&current, 0, 100).await.unwrap(),
        before.2
    );
}

#[tokio::test]
async fn frozen_content_dismiss_contract_rejects_modified_display_and_late_renderer() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    let request = dismiss_request(&rig).await;
    let mut modified = request.clone();
    modified.inspected.operation_bytes_base64 = STANDARD.encode(b"different selected payload\n");
    modified.inspected.operation_hash = content_hash(b"different selected payload\n");
    let error = dismiss_for_renderer(&rig.runtime, modified)
        .await
        .unwrap_err();
    assert!(matches!(
        error.as_str(),
        "ReconciliationRequired" | "InvalidRequest"
    ));
    let mut stale = request;
    stale.renderer_session_id = "other-renderer".into();
    assert_eq!(
        dismiss_for_renderer(&rig.runtime, stale).await.unwrap_err(),
        "stale_renderer_session"
    );
    assert_eq!(
        dismiss_status(&rig, "other-renderer").await.unwrap_err(),
        "stale_renderer_session"
    );
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
    assert!(command_sources(&rig).await == before);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_dismiss_contract_changed_checkpoint_requires_review_and_preserves_both() {
    let rig = single_operation_rig().await;
    let request = dismiss_request(&rig).await;
    {
        let mut state = rig.runtime.0.lock().await;
        let edit = state
            .store
            .open_recovery_draft(&rig.old, &rig.document)
            .await
            .unwrap();
        state
            .store
            .save_recovery_checkpoint(
                &rig.old,
                &rig.document,
                &edit.edit_session_id,
                3,
                b"new recovery edit\n",
            )
            .await
            .unwrap();
        state
            .store
            .close_recovery_draft(&rig.old, &rig.document, &edit.edit_session_id)
            .await
            .unwrap();
    }
    let before = command_sources(&rig).await;
    let error = dismiss_for_renderer(&rig.runtime, request)
        .await
        .unwrap_err();
    assert!(matches!(
        error.as_str(),
        "GenerationMismatch" | "ReconciliationRequired"
    ));
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
    assert!(command_sources(&rig).await == before);
    assert_eq!(before.0.bytes, b"new recovery edit\n");
    rig.server.shutdown().await.unwrap();
}
