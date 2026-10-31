include!("outbox_v18_migration.rs");

async fn fixture_v18(path: &Path, obstruct: bool) {
    fixture_v17(path, false).await;
    let mut db = connect(path).await;
    sqlx::raw_sql("ALTER TABLE content_conflict_intents ADD COLUMN intent_version INTEGER NOT NULL DEFAULT 0 CHECK(intent_version >= 0);
        CREATE TABLE content_conflict_intent_history (
            operation_id TEXT NOT NULL REFERENCES client_outbox(operation_id),
            registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            history_seq INTEGER NOT NULL CHECK(history_seq > 0), document_id TEXT NOT NULL,
            generation INTEGER NOT NULL CHECK(generation > 0), base_revision TEXT NOT NULL,
            base_bytes BLOB NOT NULL CHECK(length(base_bytes) <= 5242880),
            local_bytes BLOB NOT NULL CHECK(length(local_bytes) <= 5242880),
            remote_revision TEXT NOT NULL, remote_hash TEXT NOT NULL,
            remote_bytes BLOB NOT NULL CHECK(length(remote_bytes) <= 5242880),
            choice TEXT NOT NULL CHECK(choice IN ('keep_local_copy','choose_remote','save_merged')),
            merged_bytes BLOB CHECK(merged_bytes IS NULL OR length(merged_bytes) <= 5242880),
            CHECK((choice='save_merged' AND merged_bytes IS NOT NULL) OR (choice<>'save_merged' AND merged_bytes IS NULL)),
            PRIMARY KEY(operation_id, history_seq)); PRAGMA user_version=18;")
        .execute(&mut db).await.unwrap();
    if obstruct {
        sqlx::raw_sql("CREATE TABLE content_conflict_copies(sentinel TEXT NOT NULL); INSERT INTO content_conflict_copies VALUES('preserve-me');")
            .execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn v18_to_v19_preserves_old_state_and_starts_with_no_copies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v18(&path, false).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    let mut db = connect(&path).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM content_conflict_copies")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn failed_v19_migration_rolls_back_and_retains_existing_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v18(&path, true).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_integrity(&path, 18).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    let mut db = connect(&path).await;
    let sentinel: String = sqlx::query_scalar("SELECT sentinel FROM content_conflict_copies")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(sentinel, "preserve-me");
    sqlx::query("DROP TABLE content_conflict_copies")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
}
