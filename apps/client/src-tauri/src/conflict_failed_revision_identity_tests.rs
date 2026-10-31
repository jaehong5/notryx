use crate::conflict_snapshot_native::test_support::snapshot::*;

const BASE_HASH: &str = "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573";
const LOCAL_HASH: &str = "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de";
const LATER: &[u8] = b"later local checkpoint\n";
const LATER_HASH: &str = "sha256:72cc1bbd07ca9bc61e4e25258bdf4e81351cc88437d6417c76f233eaea93168d";

async fn assert_base_revision_evidence(rig: &Rig, generation: u64, local: &[u8], intent: Option<&notryx_client_store::ContentConflictIntent>) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, generation);
    assert_eq!(draft.bytes, local);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.operation_seq, rig.operation.operation_seq);
    assert_eq!(failed.bytes, rig.operation.bytes);
    assert_eq!(failed.expected_revision, rig.operation.expected_revision);
    assert_eq!(failed.conflict_revision.as_deref(), rig.first_peer.content_rev.as_ref().map(|revision| revision.as_str()));
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().as_ref(), intent);
}

#[tokio::test]
async fn confirmation_checks_displayed_412_revision_even_when_bytes_and_generation_match() {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    let before = rig.session.server_info().await.unwrap().head_seq;
    assert!(crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
        BASE_HASH, LOCAL_HASH, &shown.base_revision, "different-412-revision",
    ).await.is_err());
    assert_base_revision_evidence(&rig, 1, LOCAL, None).await;
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, before);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
        BASE_HASH, LOCAL_HASH, &shown.base_revision, &shown.conflict_revision,
    ).await.unwrap();
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    let intent = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(intent.base_bytes, BASE);
    assert_eq!(intent.base_revision, shown.base_revision);
    assert_eq!(intent.local_bytes, LOCAL);
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
    drop(state);
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, before);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconsideration_checks_displayed_412_revision_even_when_bytes_and_generation_match() {
    let rig = rig().await;
    let first = prepare(&rig).await.unwrap();
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: first.generation, remote_revision: first.remote_revision,
        remote_hash: rig.first_peer.content_hash.clone().unwrap(), remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::SaveMerged { bytes: b"manual merge\n".to_vec() },
    };
    let old = rig.runtime.0.lock().await.store.record_content_conflict_intent(
        &rig.binding, &rig.document, &rig.operation.operation_id, &request,
    ).await.unwrap();
    rig.runtime.0.lock().await.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 2, LATER).await.unwrap();
    let shown = prepare_conflict_snapshot(&rig.runtime, OWNER, &rig.document, &rig.edit_session, 2, &rig.operation.operation_id).await.unwrap();
    let before = rig.session.server_info().await.unwrap().head_seq;
    assert!(crate::conflict_snapshot_native::choose_remote_after_reconsideration(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, old.version, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
        BASE_HASH, LATER_HASH, &shown.base_revision, "different-412-revision",
    ).await.is_err());
    assert_base_revision_evidence(&rig, 2, LATER, Some(&old)).await;
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    crate::conflict_snapshot_native::choose_remote_after_reconsideration(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
        &shown.operation_id, old.version, &shown.remote_revision, rig.first_peer.content_hash.as_deref().unwrap(),
        BASE_HASH, LATER_HASH, &shown.base_revision, &shown.conflict_revision,
    ).await.unwrap();
    let mut state = rig.runtime.0.lock().await;
    let current = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(current.base_revision, shown.base_revision);
    assert_eq!(current.base_bytes, BASE);
    assert_eq!(current.local_bytes, LATER);
    assert_eq!(current.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
    assert_eq!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, old.version).await.unwrap(), Some(old));
    drop(state);
    assert_base_revision_evidence(&rig, 2, LATER, Some(&current)).await;
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, before);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
