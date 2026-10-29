include!("send_content_conflict_copy_status_tests.rs");

#[tokio::test]
async fn terminal_copy_conflict_without_local_ack_is_rejected_as_corrupt() {
    let rig = rig_with_followup(true).await;
    let (copy, create, created) = created_copy(&rig).await;
    let item = rig
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
    let peer = rig
        .peer
        .put_content(&ContentMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: created.node_id,
            expected_revision: created.content_rev.unwrap(),
            bytes: b"peer copy edit\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Committed { .. }));
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::Conflict
    );
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::NeedsReview {
            operation_id: item.operation_id
        }
    );
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE outbox_streams SET ack_through=?,confirmed_through=? WHERE registration_id=1",
    )
    .bind(create.operation_seq as i64)
    .bind(create.operation_seq as i64)
    .execute(&mut db)
    .await
    .unwrap();
    assert!(status(&rig, copy.intent_version).await.is_err());
    db.close().await.unwrap();
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
