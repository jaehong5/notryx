use crate::conflict_snapshot_native::test_support::snapshot::*;

#[tokio::test]
async fn missing_gate_session_rejects_choice_before_durable_completion() {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    let head_before = rig.session.server_info().await.unwrap().head_seq;
    {
        let mut state = rig.runtime.0.lock().await;
        state.gate.close_session(&rig.edit_session).unwrap();
        assert!(state.product_sessions.contains_key(&rig.edit_session));
    }
    let result = crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
        &shown.base_revision,
        &shown.conflict_revision,
    ).await;
    assert!(result.is_err());
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head_before);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, shown.generation);
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, shown.base_revision);
    let operation = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(operation.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(operation.operation_seq, rig.operation.operation_seq);
    assert_eq!(operation.bytes, rig.operation.bytes);
    assert_eq!(operation.expected_revision, rig.operation.expected_revision);
    assert!(state.product_sessions.contains_key(&rig.edit_session));
    let later = b"continued local draft\n";
    state.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, shown.generation + 1, later).await.unwrap();
    let saved = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(saved.bytes, later);
    assert_eq!(saved.base_bytes, BASE);
    assert_eq!(saved.base_revision, shown.base_revision);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
