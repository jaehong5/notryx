include!("frozen_content_inspection_tests.rs");

#[tokio::test]
async fn frozen_sent_with_damaged_base_revision_is_rejected_without_mutation() {
    let rig = rig().await;
    let old_expected = rig.sent.expected_revision.clone().unwrap();
    assert_eq!(old_expected, rig.base_revision);
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(
                rig._directory
                    .path()
                    .join("frozen-content-inspection.sqlite"),
            )
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let damage = "synthetic-other-valid-revision";
    assert_ne!(damage, old_expected);
    assert!(notryx_domain::ContentRevision::new(damage).is_ok());
    let changed = sqlx::query(
        "UPDATE draft_bases SET revision=? WHERE document_id=(SELECT d.storage_id FROM registered_documents d JOIN client_registrations r ON r.id=d.registration_id WHERE r.device_id=? AND r.device_stream_id=? AND d.document_id=?)",
    )
    .bind(damage)
    .bind(&rig.old.device_id)
    .bind(&rig.old.device_stream_id)
    .bind(&rig.document)
    .execute(&mut db)
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    db.close().await.unwrap();

    let (draft, operation, old_page, new_page) = {
        let mut state = rig.runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&rig.old, &rig.document)
                .await
                .unwrap(),
            state
                .store
                .read_outbox_operation(&rig.old, &rig.sent.operation_id)
                .await
                .unwrap(),
            state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
            state.store.list_outbox(&rig.current, 0, 100).await.unwrap(),
        )
    };
    let head = rig.session.server_info().await.unwrap().head_seq;
    let path = rig
        ._directory
        .path()
        .join("frozen-content-inspection.sqlite");
    let marks = stream_watermarks(&path).await;
    assert!(
        inspect_frozen_content_for_renderer(
            &rig.runtime,
            "owner",
            &rig.current,
            &rig.old,
            &rig.sent.operation_id,
        )
        .await
        .is_err()
    );
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap(),
        draft
    );
    assert!(
        state
            .store
            .read_outbox_operation(&rig.old, &rig.sent.operation_id)
            .await
            .unwrap()
            == operation
    );
    assert_eq!(
        state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        old_page
    );
    assert_eq!(
        state.store.list_outbox(&rig.current, 0, 100).await.unwrap(),
        new_page
    );
    drop(state);
    assert_eq!(stream_watermarks(&path).await, marks);
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_queued_dependency_without_expected_revision_keeps_distinct_base() {
    let rig = rig().await;
    let followup = {
        let mut state = rig.runtime.0.lock().await;
        let page = state.store.list_outbox(&rig.old, 0, 100).await.unwrap();
        assert_eq!(page.entries.len(), 2);
        state
            .store
            .read_outbox_operation(&rig.old, &page.entries[1].operation_id)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(followup.status, OutboxStatus::Queued);
    assert!(followup.expected_revision.is_none());
    let result = inspect_frozen_content_for_renderer(
        &rig.runtime,
        "owner",
        &rig.current,
        &rig.old,
        &followup.operation_id,
    )
    .await
    .unwrap();
    assert!(result.operation == followup);
    assert!(result.operation.expected_revision.is_none());
    assert_eq!(result.base_revision, rig.base_revision);
    assert_eq!(result.base_bytes, BASE);
    assert_eq!(result.local_bytes, LOCAL);
    assert_eq!(result.remote_bytes, SENT);
    assert_eq!(result.operation.status, OutboxStatus::Queued);
    let previous = inspect_frozen_content_for_renderer(
        &rig.runtime,
        "owner",
        &rig.current,
        &rig.old,
        &rig.sent.operation_id,
    )
    .await
    .unwrap();
    assert!(previous.outcome_unknown);
    rig.server.shutdown().await.unwrap();
}
