use crate::conflict_snapshot_native::test_support::snapshot::*;

#[tokio::test]
async fn confirmed_remote_choice_preserves_evidence_and_reopens_selected_checkpoint() {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    assert_eq!(shown.generation, 1);
    assert_eq!(shown.local_bytes, LOCAL);
    assert_eq!(shown.base_bytes, BASE);
    assert_eq!(shown.remote_bytes, REMOTE);
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    let head_before = rig.session.server_info().await.unwrap().head_seq;
    let bytes_before = std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap();
    assert_eq!(bytes_before, REMOTE);

    crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime,
        OWNER,
        &rig.document,
        &rig.edit_session,
        shown.generation,
        &shown.operation_id,
        &shown.remote_revision,
        rig.first_peer.content_hash.as_deref().unwrap(),
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
        &shown.base_revision,
        &shown.conflict_revision,
    ).await.unwrap();

    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head_before);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), bytes_before);
    assert_eq!(rig.peer.content(&rig.first_peer).await.unwrap(), REMOTE);
    let mut state = rig.runtime.0.lock().await;
    assert!(!state.product_sessions.contains_key(&rig.edit_session));
    assert!(state.gate.close_session(&rig.edit_session).is_err());
    let intent = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(intent.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
    assert_eq!(intent.generation, shown.generation);
    assert_eq!(intent.local_bytes, LOCAL);
    assert_eq!(intent.base_bytes, BASE);
    assert_eq!(intent.base_revision, shown.base_revision);
    assert_eq!(intent.remote_bytes, REMOTE);
    assert_eq!(intent.remote_revision, shown.remote_revision);
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.operation_seq, rig.operation.operation_seq);
    assert_eq!(failed.bytes, rig.operation.bytes);
    assert_eq!(failed.expected_revision, rig.operation.expected_revision);
    assert_eq!(failed.conflict_revision.as_deref(), rig.first_peer.content_rev.as_ref().map(|revision| revision.as_str()));
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, shown.generation + 1);
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, shown.remote_revision);
    assert!(state.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, draft.generation + 1, b"stale edit\n").await.is_err());
    let reopened = state.store.open_registered_document(&rig.binding, &rig.document, REMOTE, &shown.remote_revision).await.unwrap();
    assert_ne!(reopened.edit_session_id, rig.edit_session);
    assert_eq!(reopened.generation, draft.generation);
    assert_eq!(reopened.bytes, REMOTE);
    state.store.close_registered_document(&rig.binding, &rig.document, &reopened.edit_session_id).await.unwrap();
    assert!(state.store.first_content_conflict(&rig.binding, &rig.document).await.unwrap().is_none());
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
