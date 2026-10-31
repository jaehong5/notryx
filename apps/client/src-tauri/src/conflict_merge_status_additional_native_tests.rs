use crate::conflict_snapshot_native::test_support::merge_recovery::*;

#[tokio::test]
async fn merge_status_returns_same_id_and_seq_after_later_ack_retires_payload() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let merged = queue_approved_merge(&rig, intent.version).await.unwrap();
    let (_, opened) = crate::conflict_snapshot_native::reopen_conflict_checkpoint_for_renderer(
        &rig.runtime,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.operation.operation_id,
        &rig.edit_session,
    )
    .await
    .unwrap();
    let first = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        first.status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    assert_eq!(
        first.operation_id.as_deref(),
        Some(merged.operation_id.as_str())
    );
    let later = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.document,
                &opened.edit_session_id,
                merged.generation + 1,
                LATER,
            )
            .await
            .unwrap();
        state
            .store
            .queue_checkpoint(&rig.binding, &rig.document, merged.generation + 1)
            .await
            .unwrap()
    };
    assert_ne!(later.operation_id, merged.operation_id);
    let second = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        second.status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    assert_eq!(
        second.operation_id.as_deref(),
        Some(later.operation_id.as_str())
    );
    let database = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&database),
    )
    .await
    .unwrap();
    let retired: (i64, i64, String) = sqlx::query_as(
        "SELECT payload_retired,length(bytes),status FROM client_outbox WHERE operation_id=?",
    )
    .bind(&merged.operation_id)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(retired, (1, 0, "committed".into()));
    db.close().await.unwrap();
    let found = read_queued_merge(
        &rig,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.operation.operation_id,
        intent.version,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(found.operation_id, merged.operation_id);
    assert_eq!(found.operation_seq, merged.operation_seq);
    assert_eq!(found.bytes, MERGED);
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap()
            >= later.operation_seq
    );
    assert!(matches!(
        state
            .store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        ContentConflictResolutionState::Completed { .. }
    ));
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LATER);
    assert_eq!(draft.base_bytes, LATER);
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        LATER
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_status_is_available_offline_to_current_owner_after_renderer_replacement() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let merged = queue_approved_merge(&rig, intent.version).await.unwrap();
    rig.proxy.abort();
    {
        let mut state = rig.runtime.0.lock().await;
        state.session = None;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        crate::search_native::replace_renderer(&mut state, "new-owner".into());
    }
    assert!(
        read_queued_merge(
            &rig,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            intent.version
        )
        .await
        .is_err()
    );
    let found = read_queued_merge(
        &rig,
        "new-owner",
        &rig.binding,
        &rig.document,
        &rig.operation.operation_id,
        intent.version,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(found.operation_id, merged.operation_id);
    assert_eq!(found.operation_seq, merged.operation_seq);
    assert_eq!(found.status, OutboxStatus::Queued);
    assert_eq!(found.bytes, MERGED);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, MERGED);
    assert_eq!(draft.base_bytes, REMOTE);
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_status_rejects_prior_and_new_bindings_after_registration_replacement() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let merged = queue_approved_merge(&rig, intent.version).await.unwrap();
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
    assert!(
        read_queued_merge(
            &rig,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            intent.version
        )
        .await
        .is_err()
    );
    assert!(
        read_queued_merge(
            &rig,
            OWNER,
            &new_binding,
            &rig.document,
            &rig.operation.operation_id,
            intent.version
        )
        .await
        .is_err()
    );
    let mut state = rig.runtime.0.lock().await;
    let saved = state
        .store
        .read_queued_content_conflict_merge(
            &rig.binding,
            &rig.operation.operation_id,
            intent.version,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(saved.frozen);
    assert_eq!(saved.operation_id, merged.operation_id);
    assert_eq!(saved.operation_seq, merged.operation_seq);
    assert_eq!(saved.bytes, MERGED);
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, MERGED);
    assert_eq!(draft.base_bytes, REMOTE);
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
