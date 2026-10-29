include!("frozen_content_apply_contract_tests.rs");

struct DecisionStatusRequest {
    renderer_session_id: String,
    current: RegistrationBinding,
    old: RegistrationBinding,
    document_id: String,
    old_operation_id: String,
}

#[derive(Debug, PartialEq, Eq)]
struct DecisionStatus {
    operation_id: String,
    operation_seq: String,
    status: WorkspaceOutboxStatus,
    approved_generation: String,
    approved_hash: String,
}

async fn decision_status_for_test(
    runtime: &Runtime,
    request: DecisionStatusRequest,
) -> Result<Option<DecisionStatus>, String> {
    let response = crate::frozen_content_native::frozen_content_decision_status_for_renderer(
        runtime,
        notryx_protocol::WorkspaceFrozenContentDecisionStatusRequest {
            renderer_session_id: request.renderer_session_id,
            binding: crate::to_contract(request.current),
            old_binding: crate::to_contract(request.old),
            document_id: request.document_id,
            old_operation_id: request.old_operation_id,
        },
    )
    .await?;
    Ok(response.decision.map(|item| DecisionStatus {
        operation_id: item.operation_id,
        operation_seq: item.operation_seq,
        status: item.status,
        approved_generation: item.approved_generation,
        approved_hash: item.approved_hash,
    }))
}

fn status_request(rig: &Rig) -> DecisionStatusRequest {
    DecisionStatusRequest {
        renderer_session_id: "owner".into(),
        current: rig.current.clone(),
        old: rig.old.clone(),
        document_id: rig.document.clone(),
        old_operation_id: rig.sent.operation_id.clone(),
    }
}

#[tokio::test]
async fn frozen_content_decision_status_contract_reopens_offline_without_display_snapshot() {
    let rig = single_operation_rig().await;
    let approval = inspected_request(&rig).await;
    let result = apply_frozen_content_for_renderer(&rig.runtime, approval)
        .await
        .unwrap();
    let before = command_sources(&rig).await;
    let mut request = status_request(&rig);
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
    request.renderer_session_id = "new-owner".into();
    let decision = decision_status_for_test(&runtime, request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decision.operation_id, result.operation_id);
    assert_eq!(decision.operation_seq, result.operation_seq);
    assert_eq!(decision.status, WorkspaceOutboxStatus::Queued);
    assert_eq!(decision.approved_generation, "2");
    assert_eq!(decision.approved_hash, content_hash(LOCAL));
    let mut state = runtime.0.lock().await;
    assert!(
        state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap()
            .unwrap()
            == before.0
    );
    assert!(state.store.list_outbox(&old, 0, 100).await.unwrap() == before.1);
    assert!(state.store.list_outbox(&current, 0, 100).await.unwrap() == before.2);
}

#[tokio::test]
async fn frozen_content_decision_status_contract_missing_is_read_only_and_owner_checked_offline() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    let mut request = status_request(&rig);
    rig.server.shutdown().await.unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state.session = None;
    }
    request.renderer_session_id = "other-owner".into();
    assert_eq!(
        decision_status_for_test(&rig.runtime, request)
            .await
            .unwrap_err(),
        "stale_renderer_session"
    );
    let request = DecisionStatusRequest {
        renderer_session_id: "owner".into(),
        current: rig.current.clone(),
        old: rig.old.clone(),
        document_id: rig.document.clone(),
        old_operation_id: rig.sent.operation_id.clone(),
    };
    assert_eq!(
        decision_status_for_test(&rig.runtime, request)
            .await
            .unwrap(),
        None
    );
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap()
            == before.0
    );
    assert!(state.store.list_outbox(&rig.old, 0, 100).await.unwrap() == before.1);
    assert!(state.store.list_outbox(&rig.current, 0, 100).await.unwrap() == before.2);
}
