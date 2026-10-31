use crate::conflict_snapshot_native::test_support::snapshot::*;

const LATER_LOCAL: &[u8] = b"later local checkpoint\n";

async fn pending_rig() -> (Rig, notryx_client_store::ContentConflictIntent) {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: shown.generation,
        remote_revision: shown.remote_revision,
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: shown.remote_bytes,
        choice: notryx_client_store::ContentConflictChoice::SaveMerged { bytes: b"manual merge\n".to_vec() },
    };
    let intent = rig.runtime.0.lock().await.store.record_content_conflict_intent(
        &rig.binding, &rig.document, &rig.operation.operation_id, &request,
    ).await.unwrap();
    rig.runtime.0.lock().await.store.save_registered_checkpoint(
        &rig.binding, &rig.document, &rig.edit_session, 2, LATER_LOCAL,
    ).await.unwrap();
    (rig, intent)
}

async fn pending_intent(rig: &Rig) -> Result<Option<notryx_client_store::ContentConflictIntent>, String> {
    crate::conflict_snapshot_native::read_pending_content_conflict_intent(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, &rig.operation.operation_id,
    ).await
}

fn spawn_reconsider(rig: &Rig, version: u64) -> tokio::task::JoinHandle<bool> {
    let runtime = Runtime(rig.runtime.0.clone());
    let document = rig.document.clone();
    let edit = rig.edit_session.clone();
    let operation = rig.operation.operation_id.clone();
    let revision = rig.first_peer.content_rev.as_ref().unwrap().as_str().to_owned();
    let hash = rig.first_peer.content_hash.clone().unwrap();
    let base_revision = rig.initial.content_rev.as_ref().unwrap().as_str().to_owned();
    let conflict_revision = rig.first_peer.content_rev.as_ref().unwrap().as_str().to_owned();
    tokio::spawn(async move {
        crate::conflict_snapshot_native::choose_remote_after_reconsideration(
            &runtime, OWNER, &document, &edit, 2, &operation, version, &revision, &hash,
            "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
            "sha256:72cc1bbd07ca9bc61e4e25258bdf4e81351cc88437d6417c76f233eaea93168d",
            &base_revision,
            &conflict_revision,
        ).await.is_err()
    })
}

async fn assert_reconsider_evidence(rig: &Rig, intent: &notryx_client_store::ContentConflictIntent, local: &[u8], generation: u64) {
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent.clone()));
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
}

#[tokio::test]
async fn pending_intent_lookup_and_new_preview_do_not_automatically_replace_choice() {
    let (rig, old) = pending_rig().await;
    assert_eq!(pending_intent(&rig).await.unwrap(), Some(old.clone()));
    let shown = prepare_conflict_snapshot(&rig.runtime, OWNER, &rig.document, &rig.edit_session, 2, &rig.operation.operation_id).await.unwrap();
    assert_eq!(shown.local_bytes, LATER_LOCAL);
    assert_eq!(shown.base_bytes, BASE);
    assert_eq!(shown.remote_bytes, REMOTE);
    assert_eq!(pending_intent(&rig).await.unwrap(), Some(old.clone()));
    assert!(spawn_reconsider(&rig, old.version + 1).await.unwrap());
    assert_reconsider_evidence(&rig, &old, LATER_LOCAL, 2).await;
    assert!(rig.runtime.0.lock().await.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0).await.unwrap().is_none());
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconsideration_rejects_local_edit_during_remote_snapshot_fetch() {
    let (rig, old) = pending_rig().await;
    rig.armed.store(true, Ordering::SeqCst);
    let pending = spawn_reconsider(&rig, old.version);
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
    let latest = b"local changed during reconsideration\n";
    rig.runtime.0.lock().await.store.save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 3, latest).await.unwrap();
    rig.release.notify_one();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap());
    assert_reconsider_evidence(&rig, &old, latest, 3).await;
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconsideration_rejects_changed_or_deleted_remote_during_fetch() {
    for deleted in [false, true] {
        let (rig, old) = pending_rig().await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_reconsider(&rig, old.version);
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
        if deleted {
            let receipt = rig.peer.delete(&DeleteMutation { binding: rig.peer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(), operation_seq: 2,
                node_id: rig.initial.node_id, expected_node_etag: rig.first_peer.node_etag.clone() }).await.unwrap();
            assert!(matches!(receipt, OperationReceipt::Deleted { .. }));
        } else {
            let receipt = rig.peer.put_content(&ContentMutation { binding: rig.peer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(), operation_seq: 2,
                node_id: rig.initial.node_id, expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                bytes: b"peer later\n".to_vec() }).await.unwrap();
            assert!(matches!(receipt, OperationReceipt::Committed { .. }));
        }
        let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session).await.unwrap();
        rig.release.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap());
        assert_reconsider_evidence(&rig, &old, LATER_LOCAL, 2).await;
        if !deleted { assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), b"peer later\n"); }
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn reconsideration_rejects_replaced_intent_version_during_fetch() {
    let (rig, old) = pending_rig().await;
    rig.armed.store(true, Ordering::SeqCst);
    let pending = spawn_reconsider(&rig, old.version);
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: 2, remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(), remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::KeepLocalCopy,
    };
    let newer = rig.runtime.0.lock().await.store.replace_pending_content_conflict_intent(
        &rig.binding, &rig.document, &rig.operation.operation_id, &old, &request,
    ).await.unwrap();
    rig.release.notify_one();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap());
    assert_reconsider_evidence(&rig, &newer, LATER_LOCAL, 2).await;
    assert_eq!(rig.runtime.0.lock().await.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0).await.unwrap(), Some(old));
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconsideration_rejects_owner_or_registration_replacement_during_fetch() {
    for replace_registration in [false, true] {
        let (rig, old) = pending_rig().await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_reconsider(&rig, old.version);
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
        if replace_registration {
            let pairing = server_test_support::pairing_request(&rig.config).await.0;
            let replacement = Arc::new(Session::pair(&format!("http://{}", rig.server.local_addr()),
                pairing.workspace_id, &pairing.pairing_secret).await.unwrap());
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
            state.remote_refresh_candidates.clear();
            crate::search_native::replace_renderer(&mut state, "new-renderer".into());
        }
        rig.release.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap());
        assert_reconsider_evidence(&rig, &old, LATER_LOCAL, 2).await;
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn explicit_reconsideration_preserves_prior_merge_and_retries_without_duplicate_history() {
    let (rig, old) = pending_rig().await;
    let shown = prepare_conflict_snapshot(&rig.runtime, OWNER, &rig.document, &rig.edit_session, 2, &rig.operation.operation_id).await.unwrap();
    assert_eq!(shown.local_bytes, LATER_LOCAL);
    assert_eq!(shown.remote_bytes, REMOTE);
    let head_before = rig.session.server_info().await.unwrap().head_seq;
    for _ in 0..2 {
        crate::conflict_snapshot_native::choose_remote_after_reconsideration(
            &rig.runtime, OWNER, &rig.document, &rig.edit_session, shown.generation,
            &rig.operation.operation_id, old.version, &shown.remote_revision,
            rig.first_peer.content_hash.as_deref().unwrap(),
            "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
            "sha256:72cc1bbd07ca9bc61e4e25258bdf4e81351cc88437d6417c76f233eaea93168d",
            &shown.base_revision,
            &shown.conflict_revision,
        ).await.unwrap();
        let current = pending_intent(&rig).await.unwrap().unwrap();
        assert_eq!(current.version, old.version + 1);
        assert_eq!(current.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
        assert_eq!(current.generation, 2);
        assert_eq!(current.local_bytes, LATER_LOCAL);
        assert_eq!(current.base_bytes, BASE);
        assert_eq!(current.base_revision, shown.base_revision);
        assert_eq!(current.remote_bytes, REMOTE);
        assert_eq!(current.remote_revision, shown.remote_revision);
        assert_reconsider_evidence(&rig, &current, LATER_LOCAL, 2).await;
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, old.version).await.unwrap(), Some(old.clone()));
        assert!(state.store.read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, current.version).await.unwrap().is_none());
        assert!(state.product_sessions.contains_key(&rig.edit_session));
    }
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head_before);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    assert_eq!(rig.peer.content(&rig.first_peer).await.unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
