include!("remote_delete_observation_tests.rs");

#[tokio::test]
async fn sent_unknown_live_or_restored_node_is_pending_after_latest_catchup() {
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
    assert_eq!(
        inspect_dirty_remote(&runtime, "owner", &document, &edit_session, 1)
            .await
            .unwrap(),
        DirtyRemoteState::Pending
    );
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
    let OperationReceipt::Deleted { node: deleted, .. } = receipt else {
        panic!("delete receipt")
    };
    assert_eq!(
        inspect_dirty_remote(&runtime, "owner", &document, &edit_session, 1)
            .await
            .unwrap(),
        DirtyRemoteState::Deleted
    );
    let restored = peer
        .restore(&notryx_client_runtime::RestoreMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: node.node_id,
            request: notryx_protocol::RestoreNodeRequest {
                expected_meta_rev: deleted.meta_rev,
                parent_id: node.parent_id.unwrap(),
                name: node.name.clone(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(restored, OperationReceipt::Restored { .. }));
    let head = peer.server_info().await.unwrap().head_seq;
    assert_eq!(
        inspect_dirty_remote(&runtime, "owner", &document, &edit_session, 1)
            .await
            .unwrap(),
        DirtyRemoteState::Pending
    );
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state.store.sync_index(&binding).await.unwrap().unwrap().0,
        head
    );
    let observed = state
        .store
        .current_remote_file(&binding, node.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!observed.deleted);
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
    drop(state);
    server.shutdown().await.unwrap();
}
