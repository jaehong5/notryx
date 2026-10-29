include!("conflict_snapshot_server_tests.rs");

use sqlx::Connection;

#[tokio::test]
async fn completion_write_failure_keeps_existing_editor_and_conflict_evidence() {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    let head_before = rig.session.server_info().await.unwrap().head_seq;
    let database = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&database),
    ).await.unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_completion BEFORE UPDATE OF completed_generation ON content_conflict_intents BEGIN SELECT RAISE(ABORT, 'synthetic completion failure'); END")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();

    assert!(crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
        &shown.base_revision,
        &shown.conflict_revision,
    ).await.is_err());
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head_before);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    let mut state = rig.runtime.0.lock().await;
    let active = state.product_sessions.get(&rig.edit_session).unwrap();
    assert_eq!(active.document_id, rig.document);
    assert_eq!(active.binding, rig.binding);
    assert!(active.kind == ProductSessionKind::Active);
    assert!(state.gate.open_session(rig.edit_session.clone()).is_err());
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, shown.generation);
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, shown.base_revision);
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.bytes, rig.operation.bytes);
    assert_eq!(failed.expected_revision, rig.operation.expected_revision);
    assert_eq!(failed.operation_seq, rig.operation.operation_seq);
    let intent = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(intent.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
    assert_eq!(intent.generation, shown.generation);
    assert_eq!(intent.local_bytes, LOCAL);
    assert_eq!(intent.base_bytes, BASE);
    assert_eq!(intent.base_revision, shown.base_revision);
    assert_eq!(intent.remote_bytes, REMOTE);
    assert_eq!(intent.remote_revision, shown.remote_revision);
    let resumed = b"continued after failure\n";
    state.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, shown.generation + 1, resumed).await.unwrap();
    let resumed_draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(resumed_draft.generation, shown.generation + 1);
    assert_eq!(resumed_draft.bytes, resumed);
    assert_eq!(resumed_draft.base_bytes, BASE);
    assert_eq!(resumed_draft.base_revision, shown.base_revision);
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
