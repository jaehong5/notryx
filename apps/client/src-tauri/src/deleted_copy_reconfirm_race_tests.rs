include!("deleted_copy_reconfirm_native_tests.rs");

async fn delayed_reconfirm_rig() -> (
    DeletedCopyRig,
    tokio::sync::mpsc::Receiver<()>,
    tokio::sync::mpsc::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (directory, server, _runtime, _session, _) = setup().await;
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
            vec!["GET /v1/changes?", "GET /v1/changes?"],
            false,
        )
        .await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        observed.recv().await.unwrap();
        release.send(()).await.unwrap();
        ready_tx.send((observed, release)).unwrap();
    });
    let pairing_a = server_test_support::pairing_request(&config).await.0;
    let session_a = Arc::new(
        Session::pair(
            &format!("http://{address}"),
            pairing_a.workspace_id,
            &pairing_a.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let pairing_b = server_test_support::pairing_request(&config).await.0;
    let session_b = Session::pair(
        &format!("http://{}", server.local_addr()),
        pairing_b.workspace_id,
        &pairing_b.pairing_secret,
    )
    .await
    .unwrap();
    let binding = registration_binding(session_a.binding());
    let inventory = session_a.inventory(None).await.unwrap();
    let source = inventory
        .nodes
        .iter()
        .find(|node| node.name == "move.md")
        .unwrap()
        .clone();
    let document = source.node_id.to_string();
    let parent = source.parent_id.unwrap().to_string();
    let client_path = directory.path().join("deleted-copy.sqlite");
    let mut store = ClientStore::open(&client_path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    let opened = store
        .open_registered_document(
            &binding,
            &document,
            BASE,
            source.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 1, SENT)
        .await
        .unwrap();
    store
        .queue_checkpoint(&binding, &document, 1)
        .await
        .unwrap();
    let original = store.prepare_outbox(&binding).await.unwrap().unwrap();
    assert_eq!(original.status, OutboxStatus::Sent);
    let mut state = RuntimeState::new(store, OWNER.into());
    state.session = Some(session_a.clone());
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
    let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    bootstrap_metadata(&runtime, &snapshot, &session_a)
        .await
        .unwrap();

    let current = session_b
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == source.node_id)
        .unwrap();
    let deleted = session_b
        .delete(&DeleteMutation {
            binding: session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: source.node_id,
            expected_node_etag: current.node_etag,
        })
        .await
        .unwrap();
    assert!(matches!(deleted, OperationReceipt::Deleted { .. }));
    catch_up_metadata(&runtime, &snapshot, &session_a)
        .await
        .unwrap();
    let intent = {
        let mut state = runtime.0.lock().await;
        let (_, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        let tombstone = nodes
            .into_iter()
            .find(|node| node.node_id == source.node_id)
            .unwrap();
        assert!(tombstone.deleted);
        state
            .store
            .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 2, CONFIRMED)
            .await
            .unwrap();
        let intent = state
            .store
            .record_deleted_draft_copy(
                &binding,
                &document,
                &DeletedDraftCopyRequest {
                    edit_session_id: opened.edit_session_id.clone(),
                    generation: 2,
                    local_bytes: CONFIRMED.to_vec(),
                    parent_id: parent,
                    name: "deleted-copy.md".into(),
                    evidence: DeletedDraftCopyEvidence::Tombstone {
                        last_change_seq: tombstone.last_change_seq,
                    },
                },
            )
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 3, LATER)
            .await
            .unwrap();
        let current = state
            .store
            .read_outbox_operation(&binding, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current, original);
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            0
        );
        intent
    };
    let base_revision = source.content_rev.as_ref().unwrap().as_str().to_owned();
    let resolved = crate::send::send_one(runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(resolved.status, WorkspaceSendStatus::Failed);
    let rig = DeletedCopyRig {
        directory,
        server,
        config,
        runtime,
        session_a,
        session_b,
        binding,
        document,
        base_revision,
        intent,
    };
    let (observed, release) = ready_rx.await.unwrap();
    (rig, observed, release, proxy)
}

#[tokio::test]
async fn deleted_copy_reconfirm_rechecks_owner_and_binding_during_delayed_catchup() {
    for transition in ["owner", "registration"] {
        let (rig, mut observed, release, proxy) = delayed_reconfirm_rig().await;
        rig.runtime
            .0
            .lock()
            .await
            .store
            .queue_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap();
        let peer = rig
            .session_b
            .create(&CreateMutation {
                binding: rig.session_b.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 2,
                request: CreateNodeRequest {
                    node_id: Uuid::now_v7().to_string().parse().unwrap(),
                    parent_id: rig.intent.parent_id.parse().unwrap(),
                    name: rig.intent.name.clone(),
                    kind: NodeKind::File,
                    upload_id: None,
                },
            })
            .await
            .unwrap();
        assert!(matches!(peer, OperationReceipt::Created { .. }));
        let failed = crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap();
        assert_eq!(failed.status, WorkspaceSendStatus::Failed);
        let replacement = if transition == "registration" {
            let pairing = server_test_support::pairing_request(&rig.config).await.0;
            Some(Arc::new(
                Session::pair(
                    &format!("http://{}", rig.server.local_addr()),
                    pairing.workspace_id,
                    &pairing.pairing_secret,
                )
                .await
                .unwrap(),
            ))
        } else {
            None
        };
        let runtime = Runtime(rig.runtime.0.clone());
        let request = retry_request(&rig, "second-copy.md");
        let task = tokio::spawn(async move {
            crate::remote_deleted_copy_native::reconfirm_for_renderer(&runtime, request).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = rig.runtime.0.lock().await;
            state.store.reset_edit_sessions();
            state.product_sessions.clear();
            if let Some(replacement) = replacement {
                state.gate.replace_owner(OWNER.into());
                state
                    .store
                    .activate_registration(&registration_binding(replacement.binding()))
                    .await
                    .unwrap();
                state.session = Some(replacement);
            } else {
                state.gate.replace_owner("new-owner".into());
            }
        }
        release.send(()).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "{transition}"
        );
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_deleted_draft_copy(&rig.binding, &rig.document)
                .await
                .unwrap(),
            Some(rig.intent.clone())
        );
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, LATER);
        assert_eq!(draft.base_bytes, BASE);
        drop(state);
        assert!(
            !rig.config
                .workspace
                .notes_root
                .join("second-copy.md")
                .exists()
        );
        proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
