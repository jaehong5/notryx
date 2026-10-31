use crate::conflict_snapshot_native::test_support::complete_pending::*;

#[tokio::test]
async fn completed_pending_remote_preserves_old_evidence_and_closes_old_editor() {
    let (rig, intent, followup) = ready_for_completion().await;
    let head = rig.session.server_info().await.unwrap().head_seq;
    assert_eq!(complete_pending(&rig, intent.version).await.unwrap()["status"], "completed");
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    let mut state = rig.runtime.0.lock().await;
    assert!(matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Completed { generation: 3, .. }));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, 3);
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, intent.remote_revision);
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap().bytes, b"newer local\n");
    assert!(!state.product_sessions.contains_key(&rig.edit_session));
    assert!(state.gate.close_session(&rig.edit_session).is_err());
    assert!(state.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 4, b"stale\n").await.is_err());
    let opened = state.store.open_registered_document(&rig.binding, &rig.document, REMOTE, &draft.base_revision).await.unwrap();
    assert_ne!(opened.edit_session_id, rig.edit_session);
    assert_eq!(opened.bytes, REMOTE);
    state.store.close_registered_document(&rig.binding, &rig.document, &opened.edit_session_id).await.unwrap();
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_completion_rejects_unconfirmed_followup_and_stale_version() {
    let (rig, intent, followup) = continue_rig().await;
    assert_eq!(continue_choice(&rig, intent.version).await.unwrap()["status"], "pending");
    assert!(complete_pending(&rig, intent.version).await.is_err());
    assert!(complete_pending(&rig, intent.version + 1).await.is_err());
    assert_pending_versions(&rig, &intent, &followup, b"newer local\n", notryx_client_store::OutboxStatus::Abandoning).await;
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_completion_rejects_remote_change_or_delete_after_followup_ack() {
    for deleted in [false, true] {
        let (rig, intent, followup) = ready_for_completion().await;
        if deleted {
            let receipt = rig.peer.delete(&DeleteMutation { binding: rig.peer.binding().clone(), operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 2, node_id: rig.initial.node_id, expected_node_etag: rig.first_peer.node_etag.clone() }).await.unwrap();
            assert!(matches!(receipt, OperationReceipt::Deleted { .. }));
        } else {
            let receipt = rig.peer.put_content(&ContentMutation { binding: rig.peer.binding().clone(), operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 2, node_id: rig.initial.node_id, expected_revision: rig.first_peer.content_rev.clone().unwrap(), bytes: b"later peer\n".to_vec() }).await.unwrap();
            assert!(matches!(receipt, OperationReceipt::Committed { .. }));
        }
        assert!(complete_pending(&rig, intent.version).await.is_err());
        assert_pending_versions(&rig, &intent, &followup, b"newer local\n", notryx_client_store::OutboxStatus::Abandoned).await;
        if !deleted { assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), b"later peer\n"); }
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn pending_completion_rejects_changed_local_owner_or_registration_before_commit() {
    for change in ["local", "owner", "registration"] {
        let (rig, intent, followup) = ready_for_completion().await;
        let ready = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let pending = delayed_completion(&rig, intent.version, "before_store_completion", ready.clone(), resume.clone());
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified()).await.unwrap();
        match change {
            "local" => {
                rig.runtime.0.lock().await.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 3, b"later local\n").await.unwrap();
            }
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
            _ => unreachable!(),
        }
        resume.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap(), "{change}");
        assert_pending_versions(&rig, &intent, &followup, if change == "local" { b"later local\n" } else { b"newer local\n" }, notryx_client_store::OutboxStatus::Abandoned).await;
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn lost_completion_response_is_recovered_from_durable_completed_state() {
    let (rig, intent, followup) = ready_for_completion().await;
    let ready = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let pending = delayed_completion(&rig, intent.version, "after_store_completion", ready.clone(), resume);
    tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified()).await.unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let mut state = rig.runtime.0.lock().await;
    assert!(matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Completed { generation: 3, .. }));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap().bytes, b"newer local\n");
    assert!(!state.product_sessions.contains_key(&rig.edit_session));
    assert!(state.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 4, b"stale\n").await.is_err());
    let reopened = state.store.open_registered_document(&rig.binding, &rig.document, REMOTE, &draft.base_revision).await.unwrap();
    assert_eq!(reopened.bytes, REMOTE);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
