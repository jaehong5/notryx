include!("frozen_content_command_tests.rs");

use crate::frozen_content_native::{
    FrozenContentApplyChoice, FrozenContentApplyRequest, FrozenContentApplyResult,
    apply_frozen_content_for_renderer,
};

async fn single_operation_rig() -> Rig {
    let (directory, server, _unused_runtime, original, _) = setup().await;
    let node = original
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    let document = node.node_id.to_string();
    let base_revision = node.content_rev.as_ref().unwrap().as_str().to_owned();
    let old = registration_binding(original.binding());
    let mut store = ClientStore::open(directory.path().join("frozen-content-inspection.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&old).await.unwrap();
    let opened = store
        .open_registered_document(&old, &document, BASE, &base_revision)
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, &document, &opened.edit_session_id, 1, SENT)
        .await
        .unwrap();
    store.queue_checkpoint(&old, &document, 1).await.unwrap();
    let sent = store.prepare_outbox(&old).await.unwrap().unwrap();
    let receipt = original
        .put_content(&ContentMutation {
            binding: original.binding().clone(),
            operation_id: sent.operation_id.parse().unwrap(),
            operation_seq: sent.operation_seq,
            node_id: node.node_id,
            expected_revision: node.content_rev.clone().unwrap(),
            bytes: SENT.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Committed { .. }));
    store
        .save_registered_checkpoint(&old, &document, &opened.edit_session_id, 2, LOCAL)
        .await
        .unwrap();
    store
        .close_registered_document(&old, &document, &opened.edit_session_id)
        .await
        .unwrap();
    let mut state = RuntimeState::new(store, "owner".into());
    state.session = Some(original.clone());
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: server.local_addr(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("server-state"),
            uploads: Default::default(),
        },
    };
    let pairing = server_test_support::pairing_request(&config).await.0;
    let session = Arc::new(
        Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let current = registration_binding(session.binding());
    assert_eq!(old.workspace_id, current.workspace_id);
    assert_eq!(old.workspace_epoch, current.workspace_epoch);
    assert_ne!(old.device_stream_id, current.device_stream_id);
    {
        let mut state = runtime.0.lock().await;
        let pair = state.gate.begin_pair("owner").unwrap();
        state.store.activate_registration(&current).await.unwrap();
        state.gate.commit_pair(&pair).unwrap();
        state.session = Some(session.clone());
    }
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    Rig {
        _directory: directory,
        server,
        runtime,
        session,
        old,
        current,
        document,
        base_revision,
        sent,
    }
}

async fn inspected_request(rig: &Rig) -> FrozenContentApplyRequest {
    FrozenContentApplyRequest {
        renderer_session_id: "owner".into(),
        choice: FrozenContentApplyChoice::LatestDraft,
        inspected: inspect_frozen_content_command_for_renderer(
            &rig.runtime,
            frozen_command_request(rig),
        )
        .await
        .unwrap(),
    }
}

#[tokio::test]
async fn frozen_content_apply_contract_creates_fixed_operation_and_recovers_same_decision_offline()
{
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    assert_eq!(before.1.entries.len(), 1);
    assert!(before.2.entries.is_empty());
    let request = inspected_request(&rig).await;
    assert_eq!(
        decoded(
            &request.inspected.local_bytes_base64,
            &request.inspected.local_hash
        ),
        LOCAL
    );
    assert_eq!(
        decoded(
            &request.inspected.operation_bytes_base64,
            &request.inspected.operation_hash
        ),
        SENT
    );
    let result: FrozenContentApplyResult =
        apply_frozen_content_for_renderer(&rig.runtime, request.clone())
            .await
            .unwrap();
    let after = command_sources(&rig).await;
    assert!(after.0 == before.0);
    assert!(after.1 == before.1);
    assert_eq!(after.2.entries.len(), 1);
    assert_eq!(after.2.entries[0].operation_id, result.operation_id);
    assert_eq!(
        after.2.entries[0].operation_seq.to_string(),
        result.operation_seq
    );
    let item = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .read_outbox_operation(&rig.current, &result.operation_id)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(item.status, OutboxStatus::Queued);
    assert_eq!(item.bytes, LOCAL);
    assert_eq!(
        item.expected_revision.as_deref(),
        Some(request.inspected.remote_revision.as_str())
    );
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state.session = None;
    }
    let retried = apply_frozen_content_for_renderer(&rig.runtime, request.clone())
        .await
        .unwrap();
    assert_eq!(retried, result);
    let mut different = request;
    different.inspected.local_bytes_base64 = STANDARD.encode(b"different approved draft\n");
    different.inspected.local_hash = content_hash(b"different approved draft\n");
    assert_eq!(
        apply_frozen_content_for_renderer(&rig.runtime, different)
            .await
            .unwrap_err(),
        "InvalidRequest"
    );
    let mut state = rig.runtime.0.lock().await;
    let after_retry = state.store.list_outbox(&rig.current, 0, 100).await.unwrap();
    assert!(after_retry == after.2);
}

#[tokio::test]
async fn frozen_content_apply_contract_rejects_stale_display_without_new_decision() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    let request = inspected_request(&rig).await;
    set_remote_bytes(&rig, b"new remote content\n").await;
    assert_eq!(
        apply_frozen_content_for_renderer(&rig.runtime, request)
            .await
            .unwrap_err(),
        "stale_inspection"
    );
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        b"new remote content\n"
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_apply_contract_rejects_unreviewed_successor_without_mutation() {
    let rig = rig().await;
    let before = command_sources(&rig).await;
    assert_eq!(before.1.entries.len(), 2);
    let request = inspected_request(&rig).await;
    assert_eq!(
        apply_frozen_content_for_renderer(&rig.runtime, request)
            .await
            .unwrap_err(),
        "ReconciliationRequired"
    );
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_apply_contract_does_not_create_decision_when_first_request_is_offline() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    let request = inspected_request(&rig).await;
    rig.server.shutdown().await.unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state.session = None;
    }
    assert_eq!(
        apply_frozen_content_for_renderer(&rig.runtime, request)
            .await
            .unwrap_err(),
        "not_connected"
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
