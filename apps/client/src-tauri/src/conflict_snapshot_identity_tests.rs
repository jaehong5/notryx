include!("conflict_snapshot_server_tests.rs");

use sqlx::Connection;

const BASE_HASH: &str = "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573";
const LOCAL_HASH: &str = "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de";
const LATER_LOCAL: &[u8] = b"later local checkpoint\n";
const LATER_HASH: &str = "sha256:72cc1bbd07ca9bc61e4e25258bdf4e81351cc88437d6417c76f233eaea93168d";
const DAMAGED: &[u8] = b"synthetic altered bytes\n";

async fn corrupt_same_generation(rig: &Rig, which: &str) {
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(path)).await.unwrap();
    let query = match which {
        "base" => "UPDATE draft_bases SET bytes=? WHERE document_id=(SELECT storage_id FROM registered_documents WHERE document_id=?)",
        "local" => "UPDATE draft_checkpoints SET bytes=? WHERE document_id=(SELECT storage_id FROM registered_documents WHERE document_id=?)",
        _ => panic!("invalid synthetic corruption"),
    };
    assert_eq!(sqlx::query(query).bind(DAMAGED).bind(&rig.document).execute(&mut db).await.unwrap().rows_affected(), 1);
    db.close().await.unwrap();
}

async fn check_identity_evidence(rig: &Rig, local: &[u8], base: &[u8], generation: u64, expected_intent: Option<&notryx_client_store::ContentConflictIntent>) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, generation);
    assert_eq!(draft.bytes, local);
    assert_eq!(draft.base_bytes, base);
    assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.operation_seq, rig.operation.operation_seq);
    assert_eq!(failed.bytes, rig.operation.bytes);
    assert_eq!(failed.expected_revision, rig.operation.expected_revision);
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().as_ref(), expected_intent);
}

#[tokio::test]
async fn confirmation_rejects_display_hash_mismatch_or_changed_bytes_at_same_generation() {
    for case in ["base_hash", "local_hash", "base", "local"] {
        let rig = rig().await;
        let shown = prepare(&rig).await.unwrap();
        let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
        rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
        if matches!(case, "base" | "local") { corrupt_same_generation(&rig, case).await; }
        let base_hash = if case == "base_hash" { LOCAL_HASH } else { BASE_HASH };
        let local_hash = if case == "local_hash" { BASE_HASH } else { LOCAL_HASH };
        let result = crate::conflict_snapshot_native::choose_remote_after_confirmation(
            &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
            &shown.operation_id, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
            base_hash, local_hash,
            &shown.base_revision,
            &shown.conflict_revision,
        ).await;
        assert!(result.is_err(), "{case}");
        check_identity_evidence(&rig, if case == "local" { DAMAGED } else { LOCAL },
            if case == "base" { DAMAGED } else { BASE }, 1, None).await;
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn confirmation_accepts_exact_raw_base_and_local_hashes() {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
        BASE_HASH, LOCAL_HASH,
        &shown.base_revision,
        &shown.conflict_revision,
    ).await.unwrap();
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    let intent = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(intent.base_bytes, BASE);
    assert_eq!(intent.local_bytes, LOCAL);
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(failed.bytes, LOCAL);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

async fn pending_for_identity() -> (Rig, notryx_client_store::ContentConflictIntent) {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: 1, remote_revision: shown.remote_revision, remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(), choice: notryx_client_store::ContentConflictChoice::SaveMerged { bytes: b"manual merge\n".to_vec() },
    };
    let old = rig.runtime.0.lock().await.store.record_content_conflict_intent(&rig.binding, &rig.document, &rig.operation.operation_id, &request).await.unwrap();
    rig.runtime.0.lock().await.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 2, LATER_LOCAL).await.unwrap();
    (rig, old)
}

#[tokio::test]
async fn reconsideration_rejects_display_hash_mismatch_or_changed_bytes_at_same_generation() {
    for case in ["base_hash", "local_hash", "base", "local"] {
        let (rig, old) = pending_for_identity().await;
        let shown = prepare_conflict_snapshot(&rig.runtime, OWNER, &rig.document, &rig.edit_session, 2, &rig.operation.operation_id).await.unwrap();
        if matches!(case, "base" | "local") { corrupt_same_generation(&rig, case).await; }
        let base_hash = if case == "base_hash" { LATER_HASH } else { BASE_HASH };
        let local_hash = if case == "local_hash" { BASE_HASH } else { LATER_HASH };
        let result = crate::conflict_snapshot_native::choose_remote_after_reconsideration(
            &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
            &shown.operation_id, old.version, &shown.remote_revision,
            rig.first_peer.content_hash.as_deref().unwrap(), base_hash, local_hash,
            &shown.base_revision,
            &shown.conflict_revision,
        ).await;
        assert!(result.is_err(), "{case}");
        check_identity_evidence(&rig, if case == "local" { DAMAGED } else { LATER_LOCAL },
            if case == "base" { DAMAGED } else { BASE }, 2, Some(&old)).await;
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn reconsideration_accepts_exact_raw_hashes_and_preserves_prior_intent() {
    let (rig, old) = pending_for_identity().await;
    let shown = prepare_conflict_snapshot(&rig.runtime, OWNER, &rig.document, &rig.edit_session, 2, &rig.operation.operation_id).await.unwrap();
    crate::conflict_snapshot_native::choose_remote_after_reconsideration(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, old.version, &shown.remote_revision,
        rig.first_peer.content_hash.as_deref().unwrap(), BASE_HASH, LATER_HASH,
        &shown.base_revision,
        &shown.conflict_revision,
    ).await.unwrap();
    let mut state = rig.runtime.0.lock().await;
    let current = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(current.version, old.version + 1);
    assert_eq!(current.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
    assert_eq!(current.local_bytes, LATER_LOCAL);
    assert_eq!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, old.version).await.unwrap(), Some(old));
    drop(state);
    check_identity_evidence(&rig, LATER_LOCAL, BASE, 2, Some(&current)).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
