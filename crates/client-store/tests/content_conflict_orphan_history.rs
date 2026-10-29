include!("content_conflict_intent_replace.rs");

type ConflictSnapshotRow = (i64, i64, Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, String);

#[tokio::test]
async fn orphaned_history_blocks_new_intent_without_changing_evidence() {
    let (rig, mut store) = rig().await;
    let (old, request) = reviewed(&mut store, &rig).await;
    let current = replace(&mut store, &rig, &old, &request).await.unwrap();
    assert_eq!(history(&mut store, &rig).await, vec![old]);
    let mut db = connect(&rig.path).await;
    let sql = "SELECT history_seq,generation,base_bytes,local_bytes,remote_bytes,merged_bytes,choice FROM content_conflict_intent_history WHERE operation_id=? ORDER BY history_seq";
    let before: Vec<ConflictSnapshotRow> = sqlx::query_as(sql)
        .bind(&rig.operation)
        .fetch_all(&mut db)
        .await
        .unwrap();
    assert_eq!(before.len(), 1);
    let deleted = sqlx::query("DELETE FROM content_conflict_intents WHERE operation_id=?")
        .bind(&rig.operation)
        .execute(&mut db)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(deleted, 1);
    db.close().await.unwrap();

    let result = record(&mut store, &rig, &request).await;
    let mut db = connect(&rig.path).await;
    let after: Vec<ConflictSnapshotRow> = sqlx::query_as(sql)
        .bind(&rig.operation)
        .fetch_all(&mut db)
        .await
        .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM content_conflict_intents WHERE operation_id=?")
            .bind(&rig.operation)
            .fetch_one(&mut db)
            .await
            .unwrap();
    db.close().await.unwrap();
    assert_eq!(after, before);
    let failed = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let followup = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.operation_seq, 1);
    assert_eq!(failed.bytes, SENT);
    assert_eq!(followup.operation_seq, 2);
    assert_eq!(followup.bytes, LOCAL);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, current.generation);
    assert_eq!(draft.bytes, current.local_bytes);
    assert_eq!(draft.base_bytes, current.base_bytes);
    assert_eq!(draft.base_revision, current.base_revision);
    assert!(result.is_err());
    assert_eq!(count, 0);
}
