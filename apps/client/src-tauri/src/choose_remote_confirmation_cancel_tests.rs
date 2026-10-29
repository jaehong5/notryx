include!("conflict_snapshot_server_tests.rs");

#[tokio::test]
async fn cancellation_before_or_after_completion_invalidates_session_and_reopens_durable_state() {
    for phase in ["before_store_completion", "after_store_completion"] {
        let rig = rig().await;
        let shown = prepare(&rig).await.unwrap();
        let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
        rig.runtime.0.lock().await.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
        let head_before = rig.session.server_info().await.unwrap().head_seq;
        let ready = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let pending = tokio::spawn({
            let runtime = Runtime(rig.runtime.0.clone());
            let document = rig.document.clone();
            let edit = rig.edit_session.clone();
            let operation = shown.operation_id.clone();
            let revision = shown.remote_revision.clone();
            let hash = rig.first_peer.content_hash.clone().unwrap();
            let base_revision = shown.base_revision.clone();
            let conflict_revision = shown.conflict_revision.clone();
            let ready = ready.clone();
            let resume = resume.clone();
            async move {
                crate::conflict_snapshot_native::choose_remote_after_confirmation_with_hook(
                    &runtime, OWNER, &document, &edit, 1, &operation, &revision, &hash,
                    "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
                    "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de",
                    &base_revision,
                    &conflict_revision,
                    phase, ready, resume,
                ).await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified()).await.unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert_eq!(rig.session.server_info().await.unwrap().head_seq, head_before);
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        let mut state = rig.runtime.0.lock().await;
        assert!(!state.product_sessions.contains_key(&rig.edit_session), "phase={phase}");
        assert!(state.gate.close_session(&rig.edit_session).is_err(), "phase={phase}");
        let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
        let (expected_generation, expected_bytes, expected_base, expected_revision) =
            if phase == "before_store_completion" {
                (shown.generation, LOCAL, BASE, shown.base_revision.as_str())
            } else {
                (shown.generation + 1, REMOTE, REMOTE, shown.remote_revision.as_str())
            };
        assert_eq!(draft.generation, expected_generation, "phase={phase}");
        assert_eq!(draft.bytes, expected_bytes, "phase={phase}");
        assert_eq!(draft.base_bytes, expected_base, "phase={phase}");
        assert_eq!(draft.base_revision, expected_revision, "phase={phase}");
        let failed = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
        assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(failed.bytes, rig.operation.bytes);
        assert_eq!(failed.operation_seq, rig.operation.operation_seq);
        assert_eq!(failed.expected_revision, rig.operation.expected_revision);
        let intent = state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
        assert_eq!(intent.choice, notryx_client_store::ContentConflictChoice::ChooseRemote);
        assert_eq!(intent.local_bytes, LOCAL);
        assert_eq!(intent.base_bytes, BASE);
        assert_eq!(intent.base_revision, shown.base_revision);
        assert_eq!(intent.remote_bytes, REMOTE);
        assert_eq!(intent.remote_revision, shown.remote_revision);
        assert!(state.store.save_registered_checkpoint(
            &rig.binding, &rig.document, &rig.edit_session, expected_generation + 1, b"stale write\n",
        ).await.is_err(), "phase={phase}");
        let reopened = state.store.open_registered_document(
            &rig.binding, &rig.document, expected_base, expected_revision,
        ).await.unwrap();
        assert_ne!(reopened.edit_session_id, rig.edit_session);
        assert_eq!(reopened.generation, expected_generation);
        assert_eq!(reopened.bytes, expected_bytes);
        state.store.close_registered_document(&rig.binding, &rig.document, &reopened.edit_session_id).await.unwrap();
        drop(state);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
