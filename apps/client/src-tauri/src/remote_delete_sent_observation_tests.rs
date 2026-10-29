include!("remote_delete_observation_tests.rs");

#[tokio::test]
async fn sent_unknown_tombstone_is_visible_and_can_be_selected_for_copy() {
    let (directory, server, runtime, session, document) = setup().await;
    let binding = registration_binding(session.binding());
    let (snapshot, edit_session, sent, before) = {
        let mut state = runtime.0.lock().await;
        let (opened, active) = state
            .store
            .try_open_registered_draft(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert!(active);
        state
            .gate
            .open_session(opened.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            opened.edit_session_id.clone(),
            ProductSession {
                document_id: document.clone(),
                binding: binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let sent = state.store.prepare_outbox(&binding).await.unwrap().unwrap();
        let before = (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        );
        (
            state.gate.snapshot("owner").unwrap(),
            opened.edit_session_id,
            sent,
            before,
        )
    };
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let node = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == document)
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
    let pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let receipt = peer
        .delete(&DeleteMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_node_etag: node.node_etag.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Deleted { .. }));
    let observed = inspect_dirty_remote(&runtime, "owner", &document, &edit_session, 1)
        .await
        .unwrap();
    assert_eq!(observed, DirtyRemoteState::Deleted);
    let request = notryx_protocol::WorkspaceDeletedDraftCopySnapshotRequest {
        renderer_session_id: "owner".into(),
        binding: crate::to_contract(binding.clone()),
        document_id: document.clone(),
        edit_session_id: edit_session.clone(),
        generation: "1".into(),
    };
    let snapshot = crate::remote_deleted_copy_native::prepare_for_renderer(&runtime, request)
        .await
        .unwrap();
    let accepted = crate::remote_deleted_copy_native::confirm_for_renderer(
        &runtime,
        notryx_protocol::WorkspaceDeletedDraftCopyConfirmRequest {
            renderer_session_id: "owner".into(),
            binding: crate::to_contract(binding.clone()),
            document_id: document.clone(),
            edit_session_id: edit_session.clone(),
            generation: "1".into(),
            local_hash: snapshot.local_hash,
            parent_id: node.parent_id.unwrap().to_string(),
            name: "recovered-sent.md".into(),
            tombstone_seq: snapshot.tombstone_seq,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        accepted.status,
        notryx_protocol::WorkspaceDeletedDraftCopyConfirmStatus::Pending
    );
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap(),
        before.0
    );
    assert_eq!(
        state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        before.1
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&binding, &sent.operation_id)
            .await
            .unwrap(),
        Some(sent)
    );
    let intent = state
        .store
        .read_deleted_draft_copy(&binding, &document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intent.node_id, accepted.node_id);
    assert_eq!(intent.create_operation_id, accepted.create_operation_id);
    assert_eq!(intent.local_bytes, before.0.as_ref().unwrap().bytes);
    assert!(!directory.path().join("notes/recovered-sent.md").exists());
    drop(state);
    server.shutdown().await.unwrap();
}
