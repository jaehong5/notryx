include!("send_content_conflict_copy_history_tests.rs");

#[tokio::test]
async fn saved_copy_queue_retry_preserves_original_operation_after_payload_retirement() {
    let (rig, copy, first, later) = saved_copy_and_later_edit().await;
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(later.operation_id.as_str())
    );
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
    )
    .await
    .unwrap();
    let retired: i64 =
        sqlx::query_scalar("SELECT payload_retired FROM client_outbox WHERE operation_id=?")
            .bind(&first.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(retired, 1);
    db.close().await.unwrap();
    let retry = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_content_conflict_copy_content(
            &rig.binding,
            &rig.operation.operation_id,
            copy.intent_version,
        )
        .await
        .unwrap();
    assert_eq!(retry.operation_id, first.operation_id);
    assert_eq!(retry.operation_seq, first.operation_seq);
    assert_eq!(retry.status, OutboxStatus::Committed);
    assert_eq!(retry.bytes, b"newer local\n");
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::Saved {
            operation_id: first.operation_id
        }
    );
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
