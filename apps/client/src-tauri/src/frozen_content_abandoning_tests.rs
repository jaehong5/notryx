include!("frozen_content_inspection_tests.rs");

use notryx_client_store::OutboxAbandonState;
use notryx_protocol::{
    WorkspaceAbandonState, WorkspaceFrozenContentInspectRequest, WorkspaceOutboxStatus,
};

async fn check_frozen_abandoning(sent: bool, different_epoch: bool) {
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
    let mut old = registration_binding(original.binding());
    if different_epoch {
        // This is a synthetic local binding, not evidence of a real server restore.
        old.workspace_epoch = uuid::Uuid::new_v4().to_string();
        old.device_id = uuid::Uuid::new_v4().to_string();
        old.device_stream_id = uuid::Uuid::new_v4().to_string();
    }
    let store_path = directory.path().join("frozen-abandoning.sqlite");
    let mut store = ClientStore::open(&store_path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    let opened = store
        .open_registered_document(&old, &document, BASE, &base_revision)
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, &document, &opened.edit_session_id, 1, SENT)
        .await
        .unwrap();
    let queued = store.queue_checkpoint(&old, &document, 1).await.unwrap();
    store
        .request_outbox_abandon(&old, &queued.operation_id)
        .await
        .unwrap();
    if sent {
        // Preparing changes durable local state; no abandon HTTP request is sent here.
        let prepared = store.prepare_outbox(&old).await.unwrap().unwrap();
        assert_eq!(prepared.abandon_state, OutboxAbandonState::Sent);
    }
    store
        .save_registered_checkpoint(&old, &document, &opened.edit_session_id, 2, LOCAL)
        .await
        .unwrap();
    store
        .close_registered_document(&old, &document, &opened.edit_session_id)
        .await
        .unwrap();
    let state = RuntimeState::new(store, "owner".into());
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
    assert_eq!(
        old.workspace_epoch != current.workspace_epoch,
        different_epoch
    );
    assert_ne!(old.device_stream_id, current.device_stream_id);
    {
        let mut state = runtime.0.lock().await;
        let pair = state.gate.begin_pair("owner").unwrap();
        state.store.activate_registration(&current).await.unwrap();
        state.gate.commit_pair(&pair).unwrap();
        state.session = Some(session.clone());
    }
    let gate = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    bootstrap_metadata(&runtime, &gate, &session).await.unwrap();
    let (draft, operation, old_page, new_page) = {
        let mut state = runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&old, &document)
                .await
                .unwrap()
                .unwrap(),
            state
                .store
                .read_outbox_operation(&old, &queued.operation_id)
                .await
                .unwrap()
                .unwrap(),
            state.store.list_outbox(&old, 0, 100).await.unwrap(),
            state.store.list_outbox(&current, 0, 100).await.unwrap(),
        )
    };
    assert_eq!(operation.status, OutboxStatus::Abandoning);
    assert_eq!(
        operation.abandon_state,
        if sent {
            OutboxAbandonState::Sent
        } else {
            OutboxAbandonState::Requested
        }
    );
    assert!(operation.frozen);
    assert_eq!(old_page.entries[0].status, OutboxStatus::Abandoning);
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(operation.bytes, SENT);
    let head = session.server_info().await.unwrap().head_seq;
    let watermarks = stream_watermarks(&store_path).await;

    let result = inspect_frozen_content_for_renderer(
        &runtime,
        "owner",
        &current,
        &old,
        &queued.operation_id,
    )
    .await
    .unwrap();
    assert!(result.operation == operation);
    assert_eq!(result.operation.status, OutboxStatus::Abandoning);
    assert_eq!(result.operation.abandon_state, operation.abandon_state);
    assert!(!result.outcome_unknown);
    assert_eq!(result.local_bytes, LOCAL);
    assert_eq!(result.base_bytes, BASE);
    assert_eq!(result.base_revision, base_revision);
    assert_eq!(result.remote_bytes, BASE);
    let response = crate::frozen_content_native::inspect_frozen_content_command_for_renderer(
        &runtime,
        WorkspaceFrozenContentInspectRequest {
            renderer_session_id: "owner".into(),
            current_binding: crate::to_contract(current.clone()),
            old_binding: crate::to_contract(old.clone()),
            operation_id: queued.operation_id.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(response.operation_status, WorkspaceOutboxStatus::Abandoning);
    assert_eq!(
        response.abandon_state,
        if sent {
            WorkspaceAbandonState::Sent
        } else {
            WorkspaceAbandonState::Requested
        }
    );
    assert!(!response.outcome_unknown);
    assert_eq!(response.old_binding.workspace_epoch, old.workspace_epoch);
    assert_eq!(
        response.current_binding.workspace_epoch,
        current.workspace_epoch
    );
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap()
            .unwrap(),
        draft
    );
    assert!(
        state
            .store
            .read_outbox_operation(&old, &queued.operation_id)
            .await
            .unwrap()
            .unwrap()
            == operation
    );
    assert_eq!(
        state.store.list_outbox(&old, 0, 100).await.unwrap(),
        old_page
    );
    assert_eq!(
        state.store.list_outbox(&current, 0, 100).await.unwrap(),
        new_page
    );
    drop(state);
    assert_eq!(stream_watermarks(&store_path).await, watermarks);
    assert_eq!(session.server_info().await.unwrap().head_seq, head);
    assert_eq!(
        std::fs::read(directory.path().join("notes/move.md")).unwrap(),
        BASE
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_abandon_requested_keeps_data_and_reports_pending_same_epoch() {
    check_frozen_abandoning(false, false).await;
}

#[tokio::test]
async fn frozen_abandon_sent_keeps_data_and_reports_pending_same_epoch() {
    check_frozen_abandoning(true, false).await;
}

#[tokio::test]
async fn frozen_abandon_requested_keeps_data_and_reports_pending_different_epoch() {
    check_frozen_abandoning(false, true).await;
}

#[tokio::test]
async fn frozen_abandon_sent_keeps_data_and_reports_pending_different_epoch() {
    check_frozen_abandoning(true, true).await;
}
