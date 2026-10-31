use crate::conflict_snapshot_native::test_support::choose_remote::*;

#[tokio::test]
async fn confirmed_choice_records_pending_without_abandoning_followup_and_retries_idempotently() {
    let rig = rig_with_followup(true).await;
    let request = choice_request(&rig, 2, None);
    assert_eq!(choose(&rig, request.clone()).await.unwrap()["status"], "pending");
    let mut state = rig.runtime.0.lock().await;
    let intent = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(intent.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
    assert_eq!(intent.generation, 2);
    assert_eq!(intent.local_bytes, b"newer local\n");
    let latest = state.store.document_outbox_state(&rig.binding, &rig.document).await.unwrap().latest.unwrap();
    let followup = state.store.read_outbox_operation(&rig.binding, &latest.operation_id).await.unwrap().unwrap();
    assert_eq!(followup.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(followup.abandon_state, notryx_client_store::OutboxAbandonState::None);
    assert_eq!(followup.bytes, b"newer local\n");
    assert!(matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending { version: 0, needs_review: false, .. }));
    drop(state);
    assert_eq!(choose(&rig, request).await.unwrap()["status"], "pending");
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    let current = state.store.read_outbox_operation(&rig.binding, &followup.operation_id).await.unwrap().unwrap();
    assert_eq!(current.abandon_state, notryx_client_store::OutboxAbandonState::None);
    assert_eq!(current.operation_seq, followup.operation_seq);
    assert!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0).await.unwrap().is_none());
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    drop(state);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirmed_choice_reports_completed_when_existing_receipts_allow_immediate_completion() {
    let rig = rig().await;
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    let request = choice_request(&rig, 1, None);
    let before = rig.session.server_info().await.unwrap().head_seq;
    assert_eq!(choose(&rig, request).await.unwrap()["status"], "completed");
    let mut state = rig.runtime.0.lock().await;
    assert!(matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Completed { generation: 2, frozen: false }));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
    drop(state);
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, before);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn wrong_snapshot_or_binding_never_records_a_choice() {
    let rig = rig().await;
    let request = choice_request(&rig, 1, None);
    let mut value = serde_json::to_value(&request).unwrap();
    value["binding"]["device_stream_id"] = serde_json::json!(Uuid::new_v4().to_string());
    assert!(choose(&rig, serde_json::from_value(value).unwrap()).await.is_err());
    let mut value = serde_json::to_value(&request).unwrap();
    value["local_hash"] = serde_json::json!(BASE_HASH);
    assert!(choose(&rig, serde_json::from_value(value).unwrap()).await.is_err());
    assert!(rig.runtime.0.lock().await.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().is_none());
    assert_eq!(choose(&rig, request).await.unwrap()["status"], "pending");
    let saved = rig.runtime.0.lock().await.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(saved));
    assert!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0).await.unwrap().is_none());
    drop(state);
    assert_preserved(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn selection_rejects_owner_registration_remote_or_local_change_during_fetch() {
    for change in ["owner", "registration", "remote", "local"] {
        let rig = rig().await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_choice(&rig, choice_request(&rig, 1, None));
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
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
            "remote" => {
                let receipt = rig.peer.put_content(&ContentMutation { binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(), operation_seq: 2,
                    node_id: rig.initial.node_id, expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                    bytes: b"changed peer\n".to_vec() }).await.unwrap();
                assert!(matches!(receipt, OperationReceipt::Committed { .. }));
                let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
                crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session).await.unwrap();
            }
            "local" => {
                rig.runtime.0.lock().await.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 2, b"later local\n").await.unwrap();
            }
            _ => unreachable!(),
        }
        rig.release.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap(), "{change}");
        let mut state = rig.runtime.0.lock().await;
        assert!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().is_none());
        let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
        assert_eq!(draft.bytes, if change == "local" { b"later local\n".as_slice() } else { LOCAL });
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
        drop(state);
        let server = std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap();
        assert_eq!(server, if change == "remote" { b"changed peer\n".as_slice() } else { REMOTE });
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
