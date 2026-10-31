use crate::conflict_snapshot_native::test_support::snapshot::*;

#[tokio::test]
async fn conflict_snapshot_reads_real_412_base_local_and_latest_peer_bytes_without_mutation() {
    let rig = rig().await;
    let result = prepare(&rig).await.unwrap();
    assert_eq!(result.binding, rig.binding);
    assert_eq!(result.node_id, rig.initial.node_id);
    assert_eq!(result.operation_id, rig.operation.operation_id);
    assert_eq!(result.generation, 1);
    assert_eq!(
        result.base_revision,
        rig.initial.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(result.base_bytes, BASE);
    assert_eq!(result.local_bytes, LOCAL);
    assert_eq!(
        result.remote_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(result.remote_bytes, REMOTE);
    assert!(
        prepare_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.document,
            &rig.edit_session,
            1,
            &Uuid::now_v7().to_string()
        )
        .await
        .is_err()
    );
    assert!(
        prepare_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.document,
            &rig.edit_session,
            2,
            &rig.operation.operation_id
        )
        .await
        .is_err()
    );
    assert_preserved(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_rejects_old_response_after_second_remote_write_or_delete() {
    for delete in [false, true] {
        let rig = rig().await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = tokio::spawn({
            let (runtime, document, edit, operation) = (
                Runtime(rig.runtime.0.clone()),
                rig.document.clone(),
                rig.edit_session.clone(),
                rig.operation.operation_id.clone(),
            );
            async move {
                prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
            .await
            .unwrap();
        if delete {
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
        } else {
            let result = rig
                .peer
                .put_content(&ContentMutation {
                    binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                    operation_seq: 2,
                    node_id: rig.initial.node_id,
                    expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                    bytes: b"peer second\n".to_vec(),
                })
                .await
                .unwrap();
            assert!(matches!(result, OperationReceipt::Committed { .. }));
        }
        let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session)
            .await
            .unwrap();
        rig.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "an older captured response cannot describe the current document"
        );
        assert_preserved(&rig).await;
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn conflict_snapshot_rejects_delayed_response_after_registration_replacement() {
    let rig = rig().await;
    rig.armed.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let (runtime, document, edit, operation) = (
            Runtime(rig.runtime.0.clone()),
            rig.document.clone(),
            rig.edit_session.clone(),
            rig.operation.operation_id.clone(),
        );
        async move { prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
        .await
        .unwrap();
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
    let new_binding = registration_binding(replacement.binding());
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state
            .store
            .activate_registration(&new_binding)
            .await
            .unwrap();
        state.session = Some(replacement);
    }
    rig.release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_preserved(&rig).await;
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .list_outbox(&new_binding, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_rejects_non_utf8_peer_document_without_changing_data() {
    let rig = rig().await;
    let bytes = b"peer invalid \xff\xfe\n";
    let remote = write_external_and_observe(&rig, bytes).await;
    assert_ne!(remote.content_rev, rig.first_peer.content_rev);
    assert_eq!(
        prepare_after_external_change(&rig).await.err().as_deref(),
        Some("unsupported_content")
    );
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        bytes
    );
    assert_eq!(rig.peer.content(&remote).await.unwrap(), bytes);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_rejects_over_5_mib_peer_document_without_changing_data() {
    let rig = rig().await;
    let bytes = vec![b'x'; notryx_client_store::MAX_DOCUMENT_BYTES + 1];
    let remote = write_external_and_observe(&rig, &bytes).await;
    assert_ne!(remote.content_rev, rig.first_peer.content_rev);
    assert_eq!(
        prepare_after_external_change(&rig).await.err().as_deref(),
        Some("unsupported_content")
    );
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        bytes
    );
    assert_eq!(rig.peer.content(&remote).await.unwrap(), bytes);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_reports_newer_remote_revision_not_old_412_revision() {
    let rig = rig().await;
    let current = b"peer second version\n";
    let receipt = rig
        .peer
        .put_content(&ContentMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: rig.initial.node_id,
            expected_revision: rig.first_peer.content_rev.clone().unwrap(),
            bytes: current.to_vec(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed { content_rev, .. } = receipt else {
        panic!("peer change must commit")
    };
    let snapshot = prepare(&rig).await.unwrap();
    assert_eq!(snapshot.remote_revision, content_rev.as_str());
    assert_eq!(snapshot.remote_bytes, current);
    assert_ne!(
        snapshot.remote_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(snapshot.local_bytes, LOCAL);
    assert_eq!(snapshot.base_bytes, BASE);
    let saved = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .inspect_content_conflict(&rig.binding, &rig.document, &rig.operation.operation_id)
        .await
        .unwrap();
    assert_eq!(
        saved.conflict_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(
        saved.conflict_etag,
        rig.first_peer.content_etag.as_deref().unwrap()
    );
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        current
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_rejects_old_response_after_newer_local_checkpoint() {
    let rig = rig().await;
    rig.armed.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let (runtime, document, edit, operation) = (
            Runtime(rig.runtime.0.clone()),
            rig.document.clone(),
            rig.edit_session.clone(),
            rig.operation.operation_id.clone(),
        );
        async move { prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
        .await
        .unwrap();
    let latest = b"newer local checkpoint\n";
    rig.runtime
        .0
        .lock()
        .await
        .store
        .save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 2, latest)
        .await
        .unwrap();
    rig.release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, latest);
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        draft.base_revision,
        rig.initial.content_rev.as_ref().unwrap().as_str()
    );
    let operation = state
        .store
        .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.bytes, rig.operation.bytes);
    assert_eq!(operation.payload_hash, rig.operation.payload_hash);
    assert_eq!(
        operation.status,
        notryx_client_store::OutboxStatus::Conflict
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_old_renderer_response_cannot_commit_cache_or_ready_state() {
    let rig = rig().await;
    let before = cache_observation(&rig).await;
    assert_eq!(before, (0, 0));
    rig.armed.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let (runtime, document, edit, operation) = (
            Runtime(rig.runtime.0.clone()),
            rig.document.clone(),
            rig.edit_session.clone(),
            rig.operation.operation_id.clone(),
        );
        async move { prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
        .await
        .unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state.remote_refresh_candidates.clear();
        crate::search_native::replace_renderer(&mut state, "new-renderer".into());
    }
    rig.release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(cache_observation(&rig).await, before);
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
