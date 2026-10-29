include!("conflict_snapshot_server_tests.rs");

async fn continue_rig() -> (Rig, notryx_client_store::ContentConflictIntent, String) {
    let rig = rig_with_followup(true).await;
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: 2, remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(), remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    let (intent, followup) = {
        let mut state = rig.runtime.0.lock().await;
        let intent = state.store.record_content_conflict_intent(&rig.binding, &rig.document, &rig.operation.operation_id, &request).await.unwrap();
        let followup = state.store.document_outbox_state(&rig.binding, &rig.document).await.unwrap().latest.unwrap().operation_id;
        (intent, followup)
    };
    (rig, intent, followup)
}

async fn continue_choice(rig: &Rig, version: u64) -> Result<serde_json::Value, String> {
    let result = crate::conflict_snapshot_native::continue_remote_choice_for_renderer(
        &rig.runtime, OWNER, &rig.binding, &rig.document, &rig.edit_session, &rig.operation.operation_id, version,
    ).await?;
    Ok(serde_json::to_value(result).unwrap())
}

fn delayed_continue(rig: &Rig, version: u64, ready: Arc<Notify>, resume: Arc<Notify>) -> tokio::task::JoinHandle<bool> {
    let runtime = Runtime(rig.runtime.0.clone());
    let binding = rig.binding.clone();
    let document = rig.document.clone();
    let edit = rig.edit_session.clone();
    let operation = rig.operation.operation_id.clone();
    tokio::spawn(async move {
        crate::conflict_snapshot_native::continue_remote_choice_for_renderer_with_hook(
            &runtime, OWNER, &binding, &document, &edit, &operation, version, ready, resume,
        ).await.is_err()
    })
}

#[tokio::test]
async fn explicit_continuation_marks_one_followup_and_existing_sender_confirms_it_in_fifo() {
    let (rig, intent, followup_id) = continue_rig().await;
    let unrelated_id = Uuid::now_v7().to_string();
    let unrelated = {
        let mut state = rig.runtime.0.lock().await;
        let opened = state.store.open_registered_document(&rig.binding, &unrelated_id, b"other base\n", "other-base").await.unwrap();
        state.store.save_registered_checkpoint(&rig.binding, &unrelated_id, &opened.edit_session_id, 1, b"other pending\n").await.unwrap();
        state.store.close_registered_document(&rig.binding, &unrelated_id, &opened.edit_session_id).await.unwrap();
        state.store.queue_checkpoint(&rig.binding, &unrelated_id, 1).await.unwrap()
    };
    let response = continue_choice(&rig, intent.version).await.unwrap();
    assert_eq!(response["status"], "pending");
    let mut state = rig.runtime.0.lock().await;
    let followup = state.store.read_outbox_operation(&rig.binding, &followup_id).await.unwrap().unwrap();
    assert_eq!(followup.operation_seq, rig.operation.operation_seq + 1);
    assert_eq!(followup.abandon_state, notryx_client_store::OutboxAbandonState::Requested);
    assert_eq!(followup.bytes, b"newer local\n");
    let other = state.store.read_outbox_operation(&rig.binding, &unrelated.operation_id).await.unwrap().unwrap();
    assert_eq!(other.operation_seq, followup.operation_seq + 1);
    assert_eq!(other.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(other.abandon_state, notryx_client_store::OutboxAbandonState::None);
    assert_eq!(other.bytes, b"other pending\n");
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent.clone()));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    drop(state);
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER).await.unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Abandoned);
    assert_eq!(sent.operation_id.as_deref(), Some(followup_id.as_str()));
    let mut state = rig.runtime.0.lock().await;
    let confirmed = state.store.read_outbox_operation(&rig.binding, &followup_id).await.unwrap().unwrap();
    assert_eq!(confirmed.operation_id, followup.operation_id);
    assert_eq!(confirmed.operation_seq, followup.operation_seq);
    assert_eq!(confirmed.status, notryx_client_store::OutboxStatus::Abandoned);
    assert_eq!(confirmed.bytes, followup.bytes);
    assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), followup.operation_seq);
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &unrelated.operation_id).await.unwrap().unwrap().status, notryx_client_store::OutboxStatus::Queued);
    assert!(matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending { version: 0, .. }));
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
    drop(state);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn continuation_requires_matching_binding_session_version_and_chosen_remote() {
    let (rig, intent, followup_id) = continue_rig().await;
    assert!(continue_choice(&rig, intent.version + 1).await.is_err());
    let mut binding = rig.binding.clone();
    binding.device_stream_id = Uuid::new_v4().to_string();
    assert!(crate::conflict_snapshot_native::continue_remote_choice_for_renderer(
        &rig.runtime, OWNER, &binding, &rig.document, &rig.edit_session, &rig.operation.operation_id, intent.version,
    ).await.is_err());
    assert!(crate::conflict_snapshot_native::continue_remote_choice_for_renderer(
        &rig.runtime, OWNER, &rig.binding, &rig.document, "old-session", &rig.operation.operation_id, intent.version,
    ).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    let followup = state.store.read_outbox_operation(&rig.binding, &followup_id).await.unwrap().unwrap();
    assert_eq!(followup.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(followup.abandon_state, notryx_client_store::OutboxAbandonState::None);
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn delayed_continuation_rejects_owner_registration_local_or_remote_change() {
    for change in ["owner", "registration", "local", "remote"] {
        let (rig, intent, followup_id) = continue_rig().await;
        let ready = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let pending = delayed_continue(&rig, intent.version, ready.clone(), resume.clone());
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified()).await.unwrap();
        match change {
            "owner" => {
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                crate::search_native::replace_renderer(&mut state, "new-renderer".into());
            }
            "registration" => {
                let pairing = server_test_support::pairing_request(&rig.config).await.0;
                let replacement = Arc::new(Session::pair(&format!("http://{}", rig.server.local_addr()), pairing.workspace_id, &pairing.pairing_secret).await.unwrap());
                let binding = registration_binding(replacement.binding());
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                state.store.activate_registration(&binding).await.unwrap();
                state.session = Some(replacement);
            }
            "local" => {
                rig.runtime.0.lock().await.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 3, b"later local\n").await.unwrap();
            }
            "remote" => {
                let receipt = rig.peer.put_content(&ContentMutation { binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(), operation_seq: 2,
                    node_id: rig.initial.node_id, expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                    bytes: b"later peer\n".to_vec() }).await.unwrap();
                assert!(matches!(receipt, OperationReceipt::Committed { .. }));
                let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
                crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session).await.unwrap();
            }
            _ => unreachable!(),
        }
        resume.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap(), "{change}");
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
        let followup = state.store.read_outbox_operation(&rig.binding, &followup_id).await.unwrap().unwrap();
        assert_eq!(followup.status, notryx_client_store::OutboxStatus::Queued);
        assert_eq!(followup.abandon_state, notryx_client_store::OutboxAbandonState::None);
        assert_eq!(followup.bytes, b"newer local\n");
        let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
        assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(failed.bytes, LOCAL);
        let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
        assert_eq!(draft.bytes, if change == "local" { b"later local\n".as_slice() } else { b"newer local\n".as_slice() });
        assert_eq!(draft.base_bytes, BASE);
        drop(state);
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), if change == "remote" { b"later peer\n".as_slice() } else { REMOTE });
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn lost_abandon_receipt_reuses_same_request_and_server_ack_without_new_selection() {
    let (rig, intent, followup_id) = continue_rig().await;
    assert_eq!(continue_choice(&rig, intent.version).await.unwrap()["status"], "pending");
    let sent = {
        let mut state = rig.runtime.0.lock().await;
        let marked = state.store.read_outbox_operation(&rig.binding, &followup_id).await.unwrap().unwrap();
        state.store.prepare_outbox_matching(&rig.binding, &marked).await.unwrap().unwrap()
    };
    let receipt = rig.session.abandon(&notryx_client_runtime::AbandonMutation {
        binding: rig.session.binding().clone(),
        operation_id: sent.operation_id.parse().unwrap(),
        operation_seq: sent.operation_seq,
        node_id: sent.document_id.parse().unwrap(),
        bytes: sent.bytes.clone(),
    }).await.unwrap();
    assert!(matches!(receipt, OperationReceipt::Abandoned { .. }));
    let head = rig.session.server_info().await.unwrap().head_seq;
    let retry = crate::send::send_one(rig.runtime.0.clone(), OWNER).await.unwrap();
    assert_eq!(retry.status, notryx_protocol::WorkspaceSendStatus::Abandoned);
    assert_eq!(retry.operation_id.as_deref(), Some(followup_id.as_str()));
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    let mut state = rig.runtime.0.lock().await;
    let resolved = state.store.read_outbox_operation(&rig.binding, &followup_id).await.unwrap().unwrap();
    assert_eq!(resolved.operation_seq, sent.operation_seq);
    assert_eq!(resolved.bytes, sent.bytes);
    assert_eq!(resolved.status, notryx_client_store::OutboxStatus::Abandoned);
    assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), sent.operation_seq);
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    assert!(matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending { version: 0, .. }));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    drop(state);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
