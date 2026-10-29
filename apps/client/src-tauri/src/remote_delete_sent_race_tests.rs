include!("remote_delete_observation_tests.rs");

#[tokio::test]
async fn deleted_observation_retries_once_if_queued_becomes_sent_during_catchup() {
    use crate::remote_change_dirty::inspect_dirty_remote_notifying;
    use notryx_protocol::WorkspaceDirtyRemoteRequest;
    use std::time::Duration;

    for _transition in ["sent"] {
        let (directory, server, _unused, _unused_session, _document) = setup().await;
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
        let (address, mut observed, release, proxy) =
            crate::sync_bootstrap::tests::delayed_inventory_proxy(
                server.local_addr(),
                vec!["GET /v1/changes?"],
                false,
            )
            .await;
        let pairing = server_test_support::pairing_request(&config).await.0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let document = node.node_id.to_string();
        let mut store = ClientStore::open(directory.path().join("delete-late.sqlite"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let opened = store
            .open_registered_document(
                &binding,
                &document,
                b"move source\n",
                node.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &binding,
                &document,
                &opened.edit_session_id,
                1,
                b"local first\n",
            )
            .await
            .unwrap();
        store
            .queue_checkpoint(&binding, &document, 1)
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session.clone());
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
        let runtime = Runtime(Arc::new(Mutex::new(state)));
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let peer_pairing = server_test_support::pairing_request(&config).await.0;
        let peer = Session::pair(
            &format!("http://{}", server.local_addr()),
            peer_pairing.workspace_id,
            &peer_pairing.pairing_secret,
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
        let request = WorkspaceDirtyRemoteRequest {
            renderer_session_id: "owner".into(),
            document_id: document.clone(),
            edit_session_id: opened.edit_session_id.clone(),
            generation: "1".into(),
        };
        let task = tokio::spawn({
            let runtime = Runtime(runtime.0.clone());
            let request = request.clone();
            async move {
                let mut events = Vec::new();
                let result = inspect_dirty_remote_notifying(
                    &runtime,
                    &request,
                    1,
                    || Ok(()),
                    |event| events.push(event.to_owned()),
                )
                .await;
                (result, events)
            }
        });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = runtime.0.lock().await;
            let sent = state.store.prepare_outbox(&binding).await.unwrap().unwrap();
            assert_eq!(sent.status, notryx_client_store::OutboxStatus::Sent);
            assert_eq!(sent.bytes, b"local first\n");
        }
        release.send(()).await.unwrap();
        let (result, events) = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap(), DirtyRemoteState::Deleted);
        assert!(
            events
                .iter()
                .any(|event| event == "workspace-metadata-changed")
        );
        let mut state = runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local first\n");
        let entries = state.store.list_outbox(&binding, 0, 100).await.unwrap();
        assert_eq!(entries.entries.len(), 1);
        let sent = state
            .store
            .read_outbox_operation(&binding, &entries.entries[0].operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent.status, notryx_client_store::OutboxStatus::Sent);
        assert_eq!(sent.bytes, b"local first\n");
        drop(state);
        assert!(!directory.path().join("notes/move.md").exists());
        assert_eq!(
            peer.server_info().await.unwrap().head_seq,
            match receipt {
                OperationReceipt::Deleted { commit_seq, .. } => commit_seq,
                _ => unreachable!(),
            }
        );
        proxy.abort();
        server.shutdown().await.unwrap();
    }
}
