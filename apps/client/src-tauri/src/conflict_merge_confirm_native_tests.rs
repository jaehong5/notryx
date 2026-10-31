use crate::conflict_snapshot_native::test_support::merge_confirm::*;

#[tokio::test]
async fn merge_confirm_persists_displayed_bytes_and_idempotent_intent_without_sending() {
    let rig = rig_with_followup(true).await;
    let selection = choice_request(&rig, 2, None);
    let head = rig.session.server_info().await.unwrap().head_seq;
    let version = confirm_merge(&rig.runtime, selection.clone())
        .await
        .unwrap();
    assert_eq!(version, "0");
    let mut state = rig.runtime.0.lock().await;
    let saved = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.version, 0);
    assert_eq!(saved.generation, 2);
    assert_eq!(
        saved.choice,
        notryx_client_store::ContentConflictChoice::SaveMerged {
            bytes: b"newer local\n".to_vec()
        }
    );
    assert_eq!(saved.merged_hash.as_deref(), Some(FOLLOWUP_HASH));
    assert_eq!(saved.local_bytes, b"newer local\n");
    assert_eq!(saved.base_bytes, BASE);
    assert_eq!(saved.remote_bytes, REMOTE);
    assert_eq!(saved.remote_revision, selection.remote_revision);
    drop(state);
    assert_eq!(
        confirm_merge(&rig.runtime, selection.clone())
            .await
            .unwrap(),
        version
    );
    let mut different = selection;
    different.local_hash = BASE_HASH.into();
    assert!(confirm_merge(&rig.runtime, different).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(saved)
    );
    drop(state);
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    assert_unmodified_sources(&rig, b"newer local\n", false).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_confirm_rejects_stale_display_binding_session_and_hash() {
    for field in [
        "base_hash",
        "local_hash",
        "remote_hash",
        "base_revision",
        "conflict_revision",
        "remote_revision",
        "binding",
        "session",
        "generation",
        "version",
    ] {
        let rig = rig_with_followup(true).await;
        let mut request = choice_request(&rig, 2, None);
        match field {
            "base_hash" => request.base_hash = FOLLOWUP_HASH.into(),
            "local_hash" => request.local_hash = BASE_HASH.into(),
            "remote_hash" => request.remote_hash = BASE_HASH.into(),
            "base_revision" => request.base_revision = "other-rev".into(),
            "conflict_revision" => request.conflict_revision = "other-rev".into(),
            "remote_revision" => request.remote_revision = "other-rev".into(),
            "binding" => request.binding.device_stream_id = Uuid::new_v4().to_string(),
            "session" => request.edit_session_id = Uuid::new_v4().to_string(),
            "generation" => request.generation = "1".into(),
            "version" => request.expected_intent_version = Some("4".into()),
            _ => unreachable!(),
        }
        assert!(
            confirm_merge(&rig.runtime, request).await.is_err(),
            "{field}"
        );
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        assert_unmodified_sources(&rig, b"newer local\n", false).await;
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
            REMOTE
        );
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn merge_confirm_rejects_unconverted_cr_without_changing_the_checkpoint() {
    let rig = rig_with_followup(true).await;
    rig.runtime
        .0
        .lock()
        .await
        .store
        .save_registered_checkpoint(
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            3,
            b"newer\r\n",
        )
        .await
        .unwrap();
    let mut selection = choice_request(&rig, 3, None);
    selection.local_hash =
        "sha256:dc940f381de87a86206c3612472273afd27ffb15ced4eea5cb461766a38792ab".into();
    assert!(confirm_merge(&rig.runtime, selection).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    drop(state);
    assert_unmodified_sources(&rig, b"newer\r\n", false).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_confirm_rejects_late_owner_registration_local_or_remote_change() {
    for change in [
        "owner",
        "registration",
        "session",
        "local",
        "remote",
        "deleted",
    ] {
        let rig = rig().await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_merge(&rig, choice_request(&rig, 1, None));
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
            .await
            .unwrap();
        match change {
            "owner" => {
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                crate::search_native::replace_renderer(&mut state, "new-owner".into());
            }
            "registration" => {
                let pairing = server_test_support::pairing_request(&rig.config).await.0;
                let replacement = Arc::new(
                    Session::pair(
                        &format!("http://{}", rig.server.local_addr()),
                        pairing.workspace_id,
                        &pairing.pairing_secret,
                    )
                    .await
                    .unwrap(),
                );
                let binding = registration_binding(replacement.binding());
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                state.store.activate_registration(&binding).await.unwrap();
                state.session = Some(replacement);
            }
            "session" => {
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
            }
            "local" => {
                rig.runtime
                    .0
                    .lock()
                    .await
                    .store
                    .save_registered_checkpoint(
                        &rig.binding,
                        &rig.document,
                        &rig.edit_session,
                        2,
                        b"later local\n",
                    )
                    .await
                    .unwrap();
            }
            "remote" => {
                let result = rig
                    .peer
                    .put_content(&ContentMutation {
                        binding: rig.peer.binding().clone(),
                        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                        operation_seq: 2,
                        node_id: rig.initial.node_id,
                        expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                        bytes: b"later peer\n".to_vec(),
                    })
                    .await
                    .unwrap();
                assert!(matches!(result, OperationReceipt::Committed { .. }));
                let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
                crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session)
                    .await
                    .unwrap();
            }
            "deleted" => {
                let result = rig
                    .peer
                    .delete(&DeleteMutation {
                        binding: rig.peer.binding().clone(),
                        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                        operation_seq: 2,
                        node_id: rig.initial.node_id,
                        expected_node_etag: rig.first_peer.node_etag.clone(),
                    })
                    .await
                    .unwrap();
                assert!(matches!(result, OperationReceipt::Deleted { .. }));
                let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
                crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        rig.release.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "{change}"
        );
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        assert_unmodified_sources(
            &rig,
            if change == "local" {
                b"later local\n"
            } else {
                LOCAL
            },
            change == "registration",
        )
        .await;
        let server_path = rig.config.workspace.notes_root.join("example.md");
        if change == "deleted" {
            assert!(!server_path.exists());
        } else {
            let expected_server: &[u8] = if change == "remote" {
                b"later peer\n"
            } else {
                REMOTE
            };
            assert_eq!(std::fs::read(server_path).unwrap(), expected_server);
        }
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
