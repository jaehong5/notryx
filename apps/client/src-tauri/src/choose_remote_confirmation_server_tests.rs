use crate::conflict_snapshot_native::test_support::snapshot::*;

async fn choose_confirmed(rig: &Rig) -> Result<(), String> {
    crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime,
        OWNER,
        &rig.document,
        &rig.edit_session,
        1,
        &rig.operation.operation_id,
        rig.first_peer.content_rev.as_ref().unwrap().as_str(),
        rig.first_peer.content_hash.as_deref().unwrap(),
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
        rig.initial.content_rev.as_ref().unwrap().as_str(),
        rig.first_peer.content_rev.as_ref().unwrap().as_str(),
    ).await.map(|_| ())
}

fn spawn_confirmed_choice(rig: &Rig) -> tokio::task::JoinHandle<bool> {
    let runtime = Runtime(rig.runtime.0.clone());
    let document = rig.document.clone();
    let edit = rig.edit_session.clone();
    let operation = rig.operation.operation_id.clone();
    let revision = rig.first_peer.content_rev.as_ref().unwrap().as_str().to_owned();
    let hash = rig.first_peer.content_hash.clone().unwrap();
    let base_revision = rig.initial.content_rev.as_ref().unwrap().as_str().to_owned();
    let conflict_revision = rig.first_peer.content_rev.as_ref().unwrap().as_str().to_owned();
    tokio::spawn(async move {
        crate::conflict_snapshot_native::choose_remote_after_confirmation(
            &runtime, OWNER, &document, &edit, 1, &operation, &revision, &hash,
            "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
            "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
            &base_revision,
            &conflict_revision,
        ).await.is_err()
    })
}

async fn assert_choice_preserved(rig: &Rig, expected_local: &[u8], generation: u64) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.generation, generation);
    assert_eq!(draft.bytes, expected_local);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
    let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.bytes, rig.operation.bytes);
    assert_eq!(failed.expected_revision, rig.operation.expected_revision);
    assert_eq!(failed.conflict_revision.as_deref(), rig.first_peer.content_rev.as_ref().map(|r| r.as_str()));
    assert!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().is_none());
}

#[tokio::test]
async fn confirmed_choice_rejects_mismatched_snapshot_identity_and_prior_local_edit() {
    let rig = rig().await;
    let _shown = prepare(&rig).await.unwrap();
    let wrong = crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, 1,
        &rig.operation.operation_id, "wrong-revision", rig.first_peer.content_hash.as_deref().unwrap(),
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
        rig.initial.content_rev.as_ref().unwrap().as_str(),
        rig.first_peer.content_rev.as_ref().unwrap().as_str(),
    ).await;
    assert!(wrong.is_err());
    let wrong = crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime, OWNER, &rig.document, &rig.edit_session, 1,
        &rig.operation.operation_id, rig.first_peer.content_rev.as_ref().unwrap().as_str(), "sha256:invalid",
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
        rig.initial.content_rev.as_ref().unwrap().as_str(),
        rig.first_peer.content_rev.as_ref().unwrap().as_str(),
    ).await;
    assert!(wrong.is_err());
    assert_choice_preserved(&rig, LOCAL, 1).await;
    let later = b"local edit before confirmation\n";
    rig.runtime.0.lock().await.store.save_registered_checkpoint(
        &rig.binding, &rig.document, &rig.edit_session, 2, later,
    ).await.unwrap();
    assert!(choose_confirmed(&rig).await.is_err());
    assert_choice_preserved(&rig, later, 2).await;
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirmed_choice_rejects_local_edit_while_remote_check_is_pending() {
    let rig = rig().await;
    let _shown = prepare(&rig).await.unwrap();
    rig.armed.store(true, Ordering::SeqCst);
    let pending = spawn_confirmed_choice(&rig);
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
    let later = b"local edit after confirmation\n";
    rig.runtime.0.lock().await.store.save_registered_checkpoint(
        &rig.binding, &rig.document, &rig.edit_session, 2, later,
    ).await.unwrap();
    rig.release.notify_one();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap());
    assert_choice_preserved(&rig, later, 2).await;
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirmed_choice_rejects_owner_or_registration_replacement_during_remote_check() {
    for replace_registration in [false, true] {
        let rig = rig().await;
        let _shown = prepare(&rig).await.unwrap();
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_confirmed_choice(&rig);
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
        if replace_registration {
            let pairing = server_test_support::pairing_request(&rig.config).await.0;
            let replacement = Arc::new(Session::pair(
                &format!("http://{}", rig.server.local_addr()), pairing.workspace_id, &pairing.pairing_secret,
            ).await.unwrap());
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
        assert_choice_preserved(&rig, LOCAL, 1).await;
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn confirmed_choice_rejects_remote_change_or_deletion_during_remote_check() {
    for delete in [false, true] {
        let rig = rig().await;
        let _shown = prepare(&rig).await.unwrap();
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_confirmed_choice(&rig);
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified()).await.unwrap();
        if delete {
            let receipt = rig.peer.delete(&DeleteMutation {
                binding: rig.peer.binding().clone(), operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 2, node_id: rig.initial.node_id, expected_node_etag: rig.first_peer.node_etag.clone(),
            }).await.unwrap();
            assert!(matches!(receipt, OperationReceipt::Deleted { .. }));
        } else {
            let receipt = rig.peer.put_content(&ContentMutation {
                binding: rig.peer.binding().clone(), operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 2, node_id: rig.initial.node_id, expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                bytes: b"peer later change\n".to_vec(),
            }).await.unwrap();
            assert!(matches!(receipt, OperationReceipt::Committed { .. }));
        }
        let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session).await.unwrap();
        rig.release.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap());
        assert_choice_preserved(&rig, LOCAL, 1).await;
        if !delete {
            assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), b"peer later change\n");
        }
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
