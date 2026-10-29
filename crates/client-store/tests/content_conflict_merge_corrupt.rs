include!("content_conflict_choose_remote_guards.rs");

async fn assert_same_length_merge_corruption_is_rejected(queue: bool) {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();

    let approved = b"\xef\xbb\xbfmerged\n";
    let corrupted = b"\xef\xbb\xbfmutate\n";
    assert_eq!(approved.len(), corrupted.len());
    assert_ne!(approved, corrupted);
    let mut db = connect(&rig.path).await;
    let next_before: i64 = sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active')")
        .fetch_one(&mut db).await.unwrap();
    let count_before: i64 = sqlx::query_scalar("SELECT count(*) FROM client_outbox")
        .fetch_one(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE content_conflict_intents SET merged_bytes=? WHERE operation_id=?")
        .bind(corrupted.as_slice())
        .bind(&rig.operation)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    if queue {
        assert!(
            store
                .queue_content_conflict_merge(&rig.binding, &rig.operation, intent.version)
                .await
                .is_err()
        );
    } else {
        assert!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .is_err()
        );
    }
    let checkpoint = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.generation, 2);
    assert_eq!(checkpoint.bytes, LOCAL);
    assert_eq!(checkpoint.base_bytes, BASE);
    assert_eq!(checkpoint.base_revision, "base-rev");
    let original = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.status, OutboxStatus::Conflict);
    assert_eq!(original.bytes, SENT);
    assert_eq!(original.expected_revision.as_deref(), Some("base-rev"));
    let followup = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(followup.status, OutboxStatus::Abandoned);
    assert_eq!(followup.bytes, LOCAL);
    assert_eq!(
        store.confirmed_receipt_through(&rig.binding).await.unwrap(),
        2
    );

    let mut db = connect(&rig.path).await;
    let next_after: i64 = sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active')")
        .fetch_one(&mut db).await.unwrap();
    let count_after: i64 = sqlx::query_scalar("SELECT count(*) FROM client_outbox")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let retained: Vec<u8> = sqlx::query_scalar(
        "SELECT merged_bytes FROM content_conflict_intents WHERE operation_id=?",
    )
    .bind(&rig.operation)
    .fetch_one(&mut db)
    .await
    .unwrap();
    let links: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM content_conflict_merged_operations WHERE operation_id=?",
    )
    .bind(&rig.operation)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(next_after, next_before);
    assert_eq!(count_after, count_before);
    assert_eq!(retained, corrupted.as_slice());
    assert_eq!(links, 0);
}

#[tokio::test]
async fn same_length_corrupt_merge_read_is_rejected_without_losing_originals() {
    assert_same_length_merge_corruption_is_rejected(false).await;
}

#[tokio::test]
async fn same_length_corrupt_merge_queue_is_rejected_without_losing_originals() {
    assert_same_length_merge_corruption_is_rejected(true).await;
}
