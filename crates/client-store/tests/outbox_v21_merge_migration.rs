include!("outbox_v20_migration.rs");

#[tokio::test]
async fn v21_merge_link_collision_rolls_back_everything_and_leaves_recovery_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v20_for_v21(&path, false).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let mut db = connect(&path).await;
    sqlx::raw_sql("CREATE TABLE content_conflict_merged_operations(sentinel TEXT NOT NULL); INSERT INTO content_conflict_merged_operations VALUES('preserve-me');")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_integrity(&path, 20).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    let mut db = connect(&path).await;
    let sentinel: String =
        sqlx::query_scalar("SELECT sentinel FROM content_conflict_merged_operations")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(sentinel, "preserve-me");
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('deleted_draft_copy_intents','deleted_draft_copy_attempts')")
        .fetch_one(&mut db).await.unwrap();
    assert_eq!(created, 0);
    for table in [
        "content_conflict_intents",
        "content_conflict_intent_history",
    ] {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pragma_table_info(?) WHERE name='merged_hash'",
        )
        .bind(table)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
    db.close().await.unwrap();
    let mut recovery = notryx_client_store::RecoveryStore::open(&path)
        .await
        .unwrap();
    assert!(!recovery.drafts(None).await.unwrap().items.is_empty());
    assert!(!recovery.operations(None).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn v20_nonmerge_intent_upgrades_without_creating_a_merge_digest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v20_for_v21(&path, false).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let intent = store
        .read_content_conflict_intent(&frozen_binding_v16(), CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        intent.choice,
        notryx_client_store::ContentConflictChoice::KeepLocalCopy
    ));
    assert_eq!(intent.merged_hash, None);
    store.close().await.unwrap();
    assert_integrity(&path, 23).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
}
