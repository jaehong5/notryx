include!("frozen_content_inspection_tests.rs");

#[tokio::test]
async fn frozen_content_different_epoch_keeps_old_and_current_epochs_explicit_without_deciding_outcome()
 {
    let (directory, server, _unused_runtime, current_session, _) = setup().await;
    let node = current_session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    assert_eq!(current_session.content(&node).await.unwrap(), BASE);
    let current = registration_binding(current_session.binding());
    let mut old = current.clone();
    old.workspace_epoch = uuid::Uuid::new_v4().to_string();
    old.device_id = uuid::Uuid::new_v4().to_string();
    old.device_stream_id = uuid::Uuid::new_v4().to_string();
    let document = node.node_id.to_string();
    let base_revision = node.content_rev.as_ref().unwrap().as_str().to_owned();
    let store_path = directory.path().join("frozen-content-epoch.sqlite");
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
    store.queue_checkpoint(&old, &document, 1).await.unwrap();
    let sent = store.prepare_outbox(&old).await.unwrap().unwrap();
    assert_eq!(sent.status, OutboxStatus::Sent);
    store
        .save_registered_checkpoint(&old, &document, &opened.edit_session_id, 2, LOCAL)
        .await
        .unwrap();
    let followup = store.queue_checkpoint(&old, &document, 2).await.unwrap();
    store
        .close_registered_document(&old, &document, &opened.edit_session_id)
        .await
        .unwrap();
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
    let peer_pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        peer_pairing.workspace_id,
        &peer_pairing.pairing_secret,
    )
    .await
    .unwrap();
    let remote = b"synthetic current epoch content\n";
    let committed = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_revision: node.content_rev.clone().unwrap(),
            bytes: remote.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(committed, OperationReceipt::Committed { .. }));
    let mut state = RuntimeState::new(store, "owner".into());
    let pair = state.gate.begin_pair("owner").unwrap();
    state.store.activate_registration(&current).await.unwrap();
    state.gate.commit_pair(&pair).unwrap();
    state.session = Some(current_session.clone());
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let gate = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    bootstrap_metadata(&runtime, &gate, &current_session)
        .await
        .unwrap();

    let (draft, sent_before, queued_before, old_page, new_page) = {
        let mut state = runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&old, &document)
                .await
                .unwrap(),
            state
                .store
                .read_outbox_operation(&old, &sent.operation_id)
                .await
                .unwrap(),
            state
                .store
                .read_outbox_operation(&old, &followup.operation_id)
                .await
                .unwrap(),
            state.store.list_outbox(&old, 0, 100).await.unwrap(),
            state.store.list_outbox(&current, 0, 100).await.unwrap(),
        )
    };
    let head = current_session.server_info().await.unwrap().head_seq;
    let marks = stream_watermarks(&store_path).await;
    let result =
        inspect_frozen_content_for_renderer(&runtime, "owner", &current, &old, &sent.operation_id)
            .await
            .unwrap();
    assert_eq!(
        result.operation.binding.workspace_epoch,
        old.workspace_epoch
    );
    let response = crate::frozen_content_native::inspect_frozen_content_command_for_renderer(
        &runtime,
        notryx_protocol::WorkspaceFrozenContentInspectRequest {
            renderer_session_id: "owner".into(),
            current_binding: crate::to_contract(current.clone()),
            old_binding: crate::to_contract(old.clone()),
            operation_id: sent.operation_id.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        response.current_binding.workspace_epoch,
        current.workspace_epoch
    );
    assert_ne!(old.workspace_epoch, current.workspace_epoch);
    assert!(result.outcome_unknown);
    assert_eq!(result.local_bytes, LOCAL);
    assert_eq!(result.base_bytes, BASE);
    assert_eq!(result.base_revision, base_revision);
    assert_eq!(result.remote_bytes, remote);
    assert_ne!(result.remote_revision, base_revision);
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap(),
        draft
    );
    assert!(
        state
            .store
            .read_outbox_operation(&old, &sent.operation_id)
            .await
            .unwrap()
            == sent_before
    );
    assert!(
        state
            .store
            .read_outbox_operation(&old, &followup.operation_id)
            .await
            .unwrap()
            == queued_before
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
    assert_eq!(stream_watermarks(&store_path).await, marks);
    assert_eq!(current_session.server_info().await.unwrap().head_seq, head);
    assert_eq!(
        std::fs::read(directory.path().join("notes/move.md")).unwrap(),
        remote
    );
    server.shutdown().await.unwrap();
}
