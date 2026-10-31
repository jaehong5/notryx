use crate::conflict_snapshot_native::test_support::choose_remote::*;

async fn seed_reconsideration() -> (Rig, notryx_client_store::ContentConflictIntent, String) {
    let rig = rig_with_followup(true).await;
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: 2,
        remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::SaveMerged { bytes: b"earlier merged\n".to_vec() },
    };
    let (intent, followup) = {
        let mut state = rig.runtime.0.lock().await;
        let intent = state.store.record_content_conflict_intent(&rig.binding, &rig.document, &rig.operation.operation_id, &request).await.unwrap();
        let followup = state.store.document_outbox_state(&rig.binding, &rig.document).await.unwrap().latest.unwrap().operation_id;
        (intent, followup)
    };
    (rig, intent, followup)
}

async fn check_unmodified_reconsideration_data(rig: &Rig, followup_id: &str, intent: &notryx_client_store::ContentConflictIntent, local: &[u8]) {
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent.clone()));
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    let followup = state.store.read_outbox_operation(&rig.binding, followup_id).await.unwrap().unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.operation_seq, rig.operation.operation_seq);
    assert_eq!(failed.bytes, LOCAL);
    assert_eq!(failed.expected_revision, rig.operation.expected_revision);
    assert_eq!(followup.operation_seq, failed.operation_seq + 1);
    assert_eq!(followup.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(followup.abandon_state, notryx_client_store::OutboxAbandonState::None);
    assert_eq!(followup.bytes, b"newer local\n");
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, local);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
    assert!(state.product_sessions.contains_key(&rig.edit_session));
}

#[tokio::test]
async fn reconsideration_records_one_new_choice_and_idempotent_retry_stays_pending() {
    let (rig, old, followup) = seed_reconsideration().await;
    let request = choice_request(&rig, 2, Some(old.version));
    let head = rig.session.server_info().await.unwrap().head_seq;
    assert_eq!(choose(&rig, request.clone()).await.unwrap()["status"], "pending");
    let current = rig.runtime.0.lock().await.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(current.version, old.version + 1);
    assert_eq!(current.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
    assert_eq!(current.local_bytes, b"newer local\n");
    assert_eq!(current.base_bytes, BASE);
    assert_eq!(current.remote_bytes, REMOTE);
    assert_eq!(choose(&rig, request).await.unwrap()["status"], "pending");
    assert_eq!(rig.runtime.0.lock().await.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, old.version).await.unwrap(), Some(old));
    assert!(rig.runtime.0.lock().await.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, current.version).await.unwrap().is_none());
    check_unmodified_reconsideration_data(&rig, &followup, &current, b"newer local\n").await;
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconsideration_rejects_stale_version_binding_or_snapshot_identity() {
    let (rig, old, followup) = seed_reconsideration().await;
    let request = choice_request(&rig, 2, Some(old.version));
    let mut wrong_version = serde_json::to_value(&request).unwrap();
    wrong_version["expected_intent_version"] = serde_json::json!((old.version + 1).to_string());
    assert!(choose(&rig, serde_json::from_value(wrong_version).unwrap()).await.is_err());
    let mut wrong_binding = serde_json::to_value(&request).unwrap();
    wrong_binding["binding"]["device_stream_id"] = serde_json::json!(Uuid::new_v4().to_string());
    assert!(choose(&rig, serde_json::from_value(wrong_binding).unwrap()).await.is_err());
    let mut wrong_identity = serde_json::to_value(&request).unwrap();
    wrong_identity["local_hash"] = serde_json::json!(BASE_HASH);
    assert!(choose(&rig, serde_json::from_value(wrong_identity).unwrap()).await.is_err());
    check_unmodified_reconsideration_data(&rig, &followup, &old, b"newer local\n").await;
    assert!(rig.runtime.0.lock().await.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0).await.unwrap().is_none());
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconsideration_rejects_concurrent_owner_registration_local_remote_or_choice_change() {
    for change in ["owner", "registration", "local", "remote", "version"] {
        let (rig, old, followup) = seed_reconsideration().await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_choice(&rig, choice_request(&rig, 2, Some(old.version)));
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
        let mut expected = old.clone();
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
                let receipt = rig.peer.put_content(&ContentMutation { binding: rig.peer.binding().clone(), operation_id: Uuid::now_v7().to_string().parse().unwrap(), operation_seq: 2,
                    node_id: rig.initial.node_id, expected_revision: rig.first_peer.content_rev.clone().unwrap(), bytes: b"later peer\n".to_vec() }).await.unwrap();
                assert!(matches!(receipt, OperationReceipt::Committed { .. }));
                let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
                crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session).await.unwrap();
            }
            "version" => {
                let request = notryx_client_store::ContentConflictIntentRequest { generation: 2,
                    remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(), remote_hash: rig.first_peer.content_hash.clone().unwrap(),
                    remote_bytes: REMOTE.to_vec(), choice: notryx_client_store::ContentConflictChoice::KeepLocalCopy };
                expected = rig.runtime.0.lock().await.store.replace_pending_content_conflict_intent(&rig.binding, &rig.document, &rig.operation.operation_id, &old, &request).await.unwrap();
            }
            _ => unreachable!(),
        }
        rig.release.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap(), "{change}");
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(expected));
        let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
        assert_eq!(draft.bytes, if change == "local" { b"later local\n".as_slice() } else { b"newer local\n".as_slice() });
        assert_eq!(draft.base_bytes, BASE);
        let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
        let later = state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap();
        assert_eq!(failed.bytes, LOCAL);
        assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(later.status, notryx_client_store::OutboxStatus::Queued);
        assert_eq!(later.abandon_state, notryx_client_store::OutboxAbandonState::None);
        assert_eq!(later.bytes, b"newer local\n");
        drop(state);
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), if change == "remote" { b"later peer\n".as_slice() } else { REMOTE });
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
