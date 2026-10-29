#[tokio::test]
async fn choose_remote_restart_then_edit_commits_against_selected_revision_without_losing_conflict()
{
    let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(
        send_one(rig.state.clone(), "owner").await.unwrap().status,
        WorkspaceSendStatus::Committed
    );
    let session = rig.state.lock().await.session.clone().unwrap();
    let runtime = crate::Runtime(rig.state.clone());
    let gate = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_bootstrap::bootstrap_metadata(&runtime, &gate, &session)
        .await
        .unwrap();
    let original = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "Other.md")
        .unwrap();
    let document = original.node_id.to_string();
    let (edit, first, followup) = {
        let mut state = rig.state.lock().await;
        let edit = state
            .store
            .open_registered_document(
                &rig.binding,
                &document,
                b"other base\n",
                original.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &document,
                &edit.edit_session_id,
                1,
                b"first local\n",
            )
            .await
            .unwrap();
        let first = state
            .store
            .queue_checkpoint(&rig.binding, &document, 1)
            .await
            .unwrap();
        state
            .store
            .prepare_outbox(&rig.binding)
            .await
            .unwrap()
            .unwrap();
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &document,
                &edit.edit_session_id,
                2,
                b"newer local\n",
            )
            .await
            .unwrap();
        let followup = state
            .store
            .queue_checkpoint(&rig.binding, &document, 2)
            .await
            .unwrap();
        (edit, first, followup)
    };
    assert_eq!((first.operation_seq, followup.operation_seq), (2, 3));
    let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&rig.config)
        .await
        .0;
    let peer = Session::pair(
        &format!("http://{}", rig.server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let peer_bytes = b"\xef\xbb\xbfpeer version\r\n";
    let peer_receipt = peer
        .put_content(&notryx_client_runtime::ContentMutation {
            binding: peer.binding().clone(),
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: original.node_id,
            expected_revision: original.content_rev.clone().unwrap(),
            bytes: peer_bytes.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(
        peer_receipt,
        notryx_protocol::OperationReceipt::Committed { .. }
    ));
    let result = send_one(rig.state.clone(), "owner").await.unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Conflict);
    assert_eq!(
        result.operation_id.as_deref(),
        Some(first.operation_id.as_str())
    );
    let gate = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_catchup::catch_up_metadata(&runtime, &gate, &session)
        .await
        .unwrap();
    let remote = runtime
        .0
        .lock()
        .await
        .store
        .current_remote_file(&rig.binding, original.node_id)
        .await
        .unwrap()
        .unwrap();
    let remote_bytes = session.content(&remote).await.unwrap();
    assert_eq!(remote_bytes, peer_bytes);
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: 2,
        remote_revision: remote.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: remote.content_hash.clone().unwrap(),
        remote_bytes: remote_bytes.clone(),
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    let intent = {
        let mut state = rig.state.lock().await;
        let intent = state
            .store
            .record_content_conflict_intent(&rig.binding, &document, &first.operation_id, &request)
            .await
            .unwrap();
        let marked = state
            .store
            .request_content_conflict_followup_abandon(&rig.binding, &first.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(marked.operation_id, followup.operation_id);
        intent
    };
    let abandoned = send_one(rig.state.clone(), "owner").await.unwrap();
    assert_eq!(abandoned.status, WorkspaceSendStatus::Abandoned);
    assert_eq!(
        abandoned.operation_id.as_deref(),
        Some(followup.operation_id.as_str())
    );
    let head_before_choice = session.server_info().await.unwrap().head_seq;
    let selected = {
        let mut state = rig.state.lock().await;
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            3
        );
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            3
        );
        state
            .store
            .close_registered_document(&rig.binding, &document, &edit.edit_session_id)
            .await
            .unwrap();
        let result = state
            .store
            .complete_content_conflict_choose_remote(&rig.binding, &first.operation_id)
            .await
            .unwrap();
        assert_eq!(result.remote_bytes, peer_bytes);
        assert_eq!(
            result.remote_revision,
            remote.content_rev.as_ref().unwrap().as_str()
        );
        assert!(result.generation > 2);
        assert!(
            state
                .store
                .next_pending_outbox(&rig.binding)
                .await
                .unwrap()
                .is_none()
        );
        result
    };
    assert_eq!(
        std::fs::read(rig.notes.join("Other.md")).unwrap(),
        peer_bytes
    );
    assert_eq!(
        session.server_info().await.unwrap().head_seq,
        head_before_choice
    );
    drop(runtime);
    let RestoreSenderRig {
        _directory,
        notes,
        server,
        state,
        binding,
        ..
    } = rig;
    let state = Arc::try_unwrap(state)
        .unwrap_or_else(|_| panic!("unexpected runtime owner"))
        .into_inner();
    state.store.close().await.unwrap();
    let mut store = ClientStore::open(_directory.path().join("client.sqlite3"))
        .await
        .unwrap();
    assert_eq!(
        store
            .complete_content_conflict_choose_remote(&binding, &first.operation_id)
            .await
            .unwrap(),
        selected
    );
    let reopened = store
        .open_registered_document(&binding, &document, peer_bytes, &selected.remote_revision)
        .await
        .unwrap();
    assert_eq!(reopened.bytes, peer_bytes);
    assert_eq!(reopened.generation, selected.generation);
    let draft = store
        .read_registered_checkpoint(&binding, &document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, peer_bytes);
    assert_eq!(draft.base_bytes, peer_bytes);
    assert_eq!(draft.base_revision, selected.remote_revision);
    let original_operation = store
        .read_outbox_operation(&binding, &first.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original_operation.status, OutboxStatus::Conflict);
    assert_eq!(original_operation.bytes, b"first local\n");
    assert_eq!(
        store
            .read_content_conflict_intent(&binding, &first.operation_id)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    let next_generation = selected.generation + 1;
    let edited = b"\xef\xbb\xbfafter edit\n";
    store
        .save_registered_checkpoint(
            &binding,
            &document,
            &reopened.edit_session_id,
            next_generation,
            edited,
        )
        .await
        .unwrap();
    let next = store
        .queue_checkpoint(&binding, &document, next_generation)
        .await
        .unwrap();
    assert_eq!(next.operation_seq, 4);
    assert_ne!(next.operation_id, first.operation_id);
    assert_ne!(next.operation_id, followup.operation_id);
    assert_eq!(
        next.expected_revision.as_deref(),
        Some(selected.remote_revision.as_str())
    );
    store
        .close_registered_document(&binding, &document, &reopened.edit_session_id)
        .await
        .unwrap();
    let mut reopened_state = RuntimeState::new(store, "owner".into());
    reopened_state.session = Some(session.clone());
    let state = Arc::new(Mutex::new(reopened_state));
    let committed = send_one(state.clone(), "owner").await.unwrap();
    assert_eq!(committed.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        committed.operation_id.as_deref(),
        Some(next.operation_id.as_str())
    );
    assert_eq!(std::fs::read(notes.join("Other.md")).unwrap(), edited);
    let mut state = state.lock().await;
    assert_eq!(state.store.receipt_ack_through(&binding).await.unwrap(), 4);
    assert_eq!(
        state
            .store
            .confirmed_receipt_through(&binding)
            .await
            .unwrap(),
        4
    );
    let conflict = state
        .store
        .read_outbox_operation(&binding, &first.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conflict.status, OutboxStatus::Conflict);
    assert_eq!(conflict.bytes, b"first local\n");
    let preserved = state
        .store
        .read_content_conflict_intent(&binding, &first.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preserved, intent);
    assert_eq!(preserved.base_bytes, b"other base\n");
    assert_eq!(preserved.local_bytes, b"newer local\n");
    assert_eq!(preserved.remote_bytes, peer_bytes);
    drop(state);
    server.shutdown().await.unwrap();
}
