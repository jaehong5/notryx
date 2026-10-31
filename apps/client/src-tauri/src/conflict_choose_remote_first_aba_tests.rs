use crate::conflict_snapshot_native::test_support::choose_remote::*;

#[tokio::test]
async fn first_choice_does_not_complete_or_reuse_newer_identical_decision_after_aba() {
    let rig = rig().await;
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    assert_eq!(rig.runtime.0.lock().await.store.confirmed_receipt_through(&rig.binding).await.unwrap(), rig.operation.operation_seq);
    rig.armed.store(true, Ordering::SeqCst);
    let pending = spawn_choice(&rig, choice_request(&rig, 1, None));
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
    let mut request = notryx_client_store::ContentConflictIntentRequest {
        generation: 1,
        remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    let (initial, intermediate, latest) = {
        let mut state = rig.runtime.0.lock().await;
        let first = state.store.record_content_conflict_intent(
            &rig.binding, &rig.document, &rig.operation.operation_id, &request,
        ).await.unwrap();
        assert_eq!(first.version, 0);
        request.choice = notryx_client_store::ContentConflictChoice::KeepLocalCopy;
        let middle = state.store.replace_pending_content_conflict_intent(
            &rig.binding, &rig.document, &rig.operation.operation_id, &first, &request,
        ).await.unwrap();
        assert_eq!(middle.version, 1);
        request.choice = notryx_client_store::ContentConflictChoice::ChooseRemote;
        let final_intent = state.store.replace_pending_content_conflict_intent(
            &rig.binding, &rig.document, &rig.operation.operation_id, &middle, &request,
        ).await.unwrap();
        assert_eq!(final_intent.version, 2);
        assert_eq!(final_intent.choice, first.choice);
        assert_eq!(final_intent.generation, first.generation);
        assert_eq!(final_intent.base_bytes, first.base_bytes);
        assert_eq!(final_intent.local_bytes, first.local_bytes);
        assert_eq!(final_intent.remote_bytes, first.remote_bytes);
        (first, middle, final_intent)
    };
    rig.release.notify_one();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap());
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(latest));
    assert_eq!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0).await.unwrap(), Some(initial));
    assert_eq!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 1).await.unwrap(), Some(intermediate));
    assert!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 2).await.unwrap().is_none());
    assert!(matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending { version: 2, .. }));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, 1);
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.bytes, LOCAL);
    assert_eq!(failed.expected_revision, rig.operation.expected_revision);
    assert_eq!(failed.operation_seq, rig.operation.operation_seq);
    drop(state);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
