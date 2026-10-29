include!("content_conflict_intent.rs");

#[tokio::test]
async fn keep_copy_intent_with_oversized_hidden_merged_blob_is_rejected_without_data_loss() {
    let (rig, mut store) = rig().await;
    let mut request = rig.request.clone();
    request.choice = ContentConflictChoice::KeepLocalCopy;
    record(&mut store, &rig, &request).await.unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::query("PRAGMA ignore_check_constraints=ON")
        .execute(&mut db)
        .await
        .unwrap();
    let changed = sqlx::query(
        "UPDATE content_conflict_intents SET merged_bytes=zeroblob(5242881) WHERE operation_id=?",
    )
    .bind(&rig.operation)
    .execute(&mut db)
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    db.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    assert!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    assert_originals(&mut store, &rig).await;
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    let length: i64 = sqlx::query_scalar(
        "SELECT length(merged_bytes) FROM content_conflict_intents WHERE operation_id=?",
    )
    .bind(&rig.operation)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(length, 5242881);
    db.close().await.unwrap();
}
