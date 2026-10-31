use crate::conflict_snapshot_native::test_support::snapshot::*;

use base64::{Engine as _, engine::general_purpose::STANDARD};

async fn reopen_pending_rig() -> (Rig, notryx_client_store::ContentConflictIntent, String) {
    let rig = rig_with_followup(true).await;
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: 2,
        remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(),
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

async fn reopen(rig: &Rig, owner: &str, binding: &RegistrationBinding, prior: &str) -> Result<(notryx_client_store::ContentConflictResolutionState, notryx_protocol::WorkspaceOpenedDocument), String> {
    crate::conflict_snapshot_native::reopen_conflict_checkpoint_for_renderer(
        &rig.runtime, owner, binding, &rig.document, &rig.operation.operation_id, prior,
    ).await
}

async fn assert_opened(rig: &Rig, opened: &notryx_protocol::WorkspaceOpenedDocument, bytes: &[u8], generation: &str) {
    assert_eq!(opened.document_id, rig.document);
    assert_eq!(opened.binding, crate::to_contract(rig.binding.clone()));
    assert_ne!(opened.edit_session_id, rig.edit_session);
    assert_eq!(opened.generation, generation);
    assert_eq!(STANDARD.decode(&opened.bytes_base64).unwrap(), bytes);
    assert!(!opened.readonly);
    let state = rig.runtime.0.lock().await;
    assert!(!state.product_sessions.contains_key(&rig.edit_session));
    assert!(state.product_sessions.contains_key(&opened.edit_session_id));
}

#[tokio::test]
async fn pending_reopen_closes_surviving_session_without_queue_or_abandon() {
    let (rig, intent, followup) = reopen_pending_rig().await;
    let (status, opened) = reopen(&rig, OWNER, &rig.binding, &rig.edit_session).await.unwrap();
    assert!(matches!(status, notryx_client_store::ContentConflictResolutionState::Pending { version: 0, needs_review: false, .. }));
    assert_opened(&rig, &opened, b"newer local\n", "2").await;
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    let later = state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap();
    assert_eq!(failed.bytes, LOCAL);
    assert_eq!(later.operation_seq, failed.operation_seq + 1);
    assert_eq!(later.bytes, b"newer local\n");
    assert_eq!(later.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(later.abandon_state, notryx_client_store::OutboxAbandonState::None);
    assert_eq!(state.store.list_outbox(&rig.binding, 0, 100).await.unwrap().entries.len(), 2);
    assert!(state.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 3, b"stale\n").await.is_err());
    drop(state);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_reopen_accepts_already_discarded_prior_session_and_restores_durable_checkpoint() {
    let (rig, intent, followup) = reopen_pending_rig().await;
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.close_registered_document(&rig.binding, &rig.document, &rig.edit_session).await.unwrap();
        state.gate.close_session(&rig.edit_session).unwrap();
        state.product_sessions.remove(&rig.edit_session);
    }
    let (status, opened) = reopen(&rig, OWNER, &rig.binding, &rig.edit_session).await.unwrap();
    assert!(matches!(status, notryx_client_store::ContentConflictResolutionState::Pending { version: 0, .. }));
    assert_opened(&rig, &opened, b"newer local\n", "2").await;
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    let later = state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap();
    assert_eq!(later.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(later.abandon_state, notryx_client_store::OutboxAbandonState::None);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_response_loss_reopens_remote_checkpoint_after_actual_followup_ack() {
    let (rig, intent, followup) = reopen_pending_rig().await;
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.request_content_conflict_followup_abandon(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    }
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER).await.unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Abandoned);
    assert_eq!(sent.operation_id.as_deref(), Some(followup.as_str()));
    {
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), rig.operation.operation_seq + 1);
        state.store.close_registered_document(&rig.binding, &rig.document, &rig.edit_session).await.unwrap();
        state.gate.close_session(&rig.edit_session).unwrap();
        state.product_sessions.remove(&rig.edit_session);
        state.store.complete_content_conflict_choose_remote(&rig.binding, &rig.operation.operation_id).await.unwrap();
    }
    let (status, opened) = reopen(&rig, OWNER, &rig.binding, &rig.edit_session).await.unwrap();
    assert!(matches!(status, notryx_client_store::ContentConflictResolutionState::Completed { generation: 3, .. }));
    assert_opened(&rig, &opened, REMOTE, "3").await;
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, intent.remote_revision);
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap().bytes, b"newer local\n");
    drop(state);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reopen_rejects_wrong_owner_binding_or_prior_session_without_closing_current() {
    let (rig, intent, followup) = reopen_pending_rig().await;
    assert!(reopen(&rig, "old-owner", &rig.binding, &rig.edit_session).await.is_err());
    let mut wrong_binding = rig.binding.clone();
    wrong_binding.device_stream_id = Uuid::new_v4().to_string();
    assert!(reopen(&rig, OWNER, &wrong_binding, &rig.edit_session).await.is_err());
    assert!(reopen(&rig, OWNER, &rig.binding, "different-session").await.is_err());
    let mut state = rig.runtime.0.lock().await;
    assert!(state.product_sessions.contains_key(&rig.edit_session));
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    let item = state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap();
    assert_eq!(item.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(item.abandon_state, notryx_client_store::OutboxAbandonState::None);
    state.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 3, b"still editable\n").await.unwrap();
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reopen_rejects_replaced_owner_and_registration_without_reattaching_frozen_draft() {
    for registration in [false, true] {
        let (rig, intent, followup) = reopen_pending_rig().await;
        if registration {
            let pairing = server_test_support::pairing_request(&rig.config).await.0;
            let replacement = Arc::new(Session::pair(&format!("http://{}", rig.server.local_addr()), pairing.workspace_id, &pairing.pairing_secret).await.unwrap());
            let binding = registration_binding(replacement.binding());
            let mut state = rig.runtime.0.lock().await;
            state.store.reset_edit_sessions();
            state.product_sessions.clear();
            state.store.activate_registration(&binding).await.unwrap();
            state.session = Some(replacement);
        } else {
            let mut state = rig.runtime.0.lock().await;
            state.store.reset_edit_sessions();
            state.product_sessions.clear();
            crate::search_native::replace_renderer(&mut state, "new-renderer".into());
        }
        assert!(reopen(&rig, OWNER, &rig.binding, &rig.edit_session).await.is_err());
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
        let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
        assert_eq!(draft.bytes, b"newer local\n");
        assert_eq!(draft.base_bytes, BASE);
        let later = state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap();
        assert_eq!(later.status, notryx_client_store::OutboxStatus::Queued);
        assert_eq!(later.abandon_state, notryx_client_store::OutboxAbandonState::None);
        drop(state);
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn absent_intent_reopens_durable_checkpoint_with_or_without_prior_session() {
    for disposed in [false, true] {
        let rig = rig_with_followup(true).await;
        let followup = rig.runtime.0.lock().await.store.document_outbox_state(&rig.binding, &rig.document).await.unwrap().latest.unwrap().operation_id;
        if disposed {
            let mut state = rig.runtime.0.lock().await;
            state.store.close_registered_document(&rig.binding, &rig.document, &rig.edit_session).await.unwrap();
            state.gate.close_session(&rig.edit_session).unwrap();
            state.product_sessions.remove(&rig.edit_session);
        }
        let (status, opened) = reopen(&rig, OWNER, &rig.binding, &rig.edit_session).await.unwrap();
        assert!(matches!(status, notryx_client_store::ContentConflictResolutionState::NotFound));
        assert_opened(&rig, &opened, b"newer local\n", "2").await;
        let mut state = rig.runtime.0.lock().await;
        assert!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().is_none());
        let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
        assert_eq!(draft.generation, 2);
        assert_eq!(draft.bytes, b"newer local\n");
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
        let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
        let later = state.store.read_outbox_operation(&rig.binding, &followup).await.unwrap().unwrap();
        assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(failed.bytes, LOCAL);
        assert_eq!(later.operation_seq, failed.operation_seq + 1);
        assert_eq!(later.status, notryx_client_store::OutboxStatus::Queued);
        assert_eq!(later.abandon_state, notryx_client_store::OutboxAbandonState::None);
        assert_eq!(later.bytes, b"newer local\n");
        assert_eq!(state.store.list_outbox(&rig.binding, 0, 100).await.unwrap().entries.len(), 2);
        drop(state);
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
