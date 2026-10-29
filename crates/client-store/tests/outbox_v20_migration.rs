include!("outbox_v19_migration.rs");

const COPY_NODE_V19: &str = "019a1000-0000-7000-8000-000000000071";
const COPY_CREATE_V19: &str = "019a1000-0000-7000-8000-000000000072";

async fn fixture_v19(path: &Path, with_content_id: bool, attempts: bool, obstruct: bool) {
    fixture_v18(path, false).await;
    let mut db = connect(path).await;
    let create = if with_content_id {
        "CREATE TABLE content_conflict_copies(operation_id TEXT PRIMARY KEY NOT NULL REFERENCES content_conflict_intents(operation_id), registration_id INTEGER NOT NULL REFERENCES client_registrations(id), intent_version INTEGER NOT NULL CHECK(intent_version>=0), parent_id TEXT NOT NULL, name TEXT NOT NULL, node_id TEXT NOT NULL UNIQUE, create_operation_id TEXT NOT NULL UNIQUE, content_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id))"
    } else {
        "CREATE TABLE content_conflict_copies(operation_id TEXT PRIMARY KEY NOT NULL REFERENCES content_conflict_intents(operation_id), registration_id INTEGER NOT NULL REFERENCES client_registrations(id), intent_version INTEGER NOT NULL CHECK(intent_version>=0), parent_id TEXT NOT NULL, name TEXT NOT NULL, node_id TEXT NOT NULL UNIQUE, create_operation_id TEXT NOT NULL UNIQUE)"
    };
    sqlx::query(create).execute(&mut db).await.unwrap();
    sqlx::query(
        "UPDATE content_conflict_intents SET choice='keep_local_copy' WHERE operation_id=?",
    )
    .bind(CONFLICT_OP)
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content_conflict_copies(operation_id,registration_id,intent_version,parent_id,name,node_id,create_operation_id) VALUES(?,5,0,?,'copy.md',?,?)")
        .bind(CONFLICT_OP).bind(ROOT).bind(COPY_NODE_V19).bind(COPY_CREATE_V19).execute(&mut db).await.unwrap();
    if attempts {
        sqlx::raw_sql("CREATE TABLE content_conflict_copy_attempts(old_create_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),operation_id TEXT NOT NULL REFERENCES content_conflict_intents(operation_id),registration_id INTEGER NOT NULL REFERENCES client_registrations(id),intent_version INTEGER NOT NULL CHECK(intent_version>=0),old_parent_id TEXT NOT NULL,old_name TEXT NOT NULL,old_node_id TEXT NOT NULL,new_parent_id TEXT NOT NULL,new_name TEXT NOT NULL,new_node_id TEXT NOT NULL,new_create_operation_id TEXT NOT NULL UNIQUE)")
            .execute(&mut db).await.unwrap();
    }
    if obstruct {
        sqlx::raw_sql("CREATE TABLE content_conflict_copy_attempts(sentinel TEXT NOT NULL); INSERT INTO content_conflict_copy_attempts VALUES('preserve-me');").execute(&mut db).await.unwrap();
    }
    sqlx::query("PRAGMA user_version=19")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn v19_variants_upgrade_preserving_existing_copy_drafts_and_operations() {
    for (content_id, attempts) in [(false, false), (true, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite");
        fixture_v19(&path, content_id, attempts, false).await;
        let before = snapshot(&path).await;
        let intents = v16_intents(&path).await;
        let mut store = ClientStore::open(&path).await.unwrap();
        let copy = store
            .read_content_conflict_copy(&frozen_binding_v16(), CONFLICT_OP)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(copy.node_id, COPY_NODE_V19);
        assert_eq!(copy.create_operation_id, COPY_CREATE_V19);
        assert_eq!(copy.local_bytes, V16_LOCAL);
        assert_eq!(
            store
                .read_registered_checkpoint(&frozen_binding_v16(), CONFLICT_NODE)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            V16_LOCAL
        );
        assert_eq!(
            store
                .read_outbox_operation(&frozen_binding_v16(), CONFLICT_OP)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            V16_SENT
        );
        store.close().await.unwrap();
        assert_integrity(&path, 23).await;
        assert_eq!(snapshot(&path).await, before);
        assert_eq!(v16_intents(&path).await, intents);
        let mut db = connect(&path).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM content_conflict_copy_attempts")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let linked: Option<String> = sqlx::query_scalar(
            "SELECT content_operation_id FROM content_conflict_copies WHERE operation_id=?",
        )
        .bind(CONFLICT_OP)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert!(linked.is_none());
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn v20_migration_rejects_malformed_attempts_and_rolls_back_all_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v19(&path, false, false, true).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    assert!(ClientStore::open(&path).await.is_err());
    assert_integrity(&path, 19).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    let mut db = connect(&path).await;
    let sentinel: String =
        sqlx::query_scalar("SELECT sentinel FROM content_conflict_copy_attempts")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(sentinel, "preserve-me");
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('content_conflict_copies')")
            .fetch_all(&mut db)
            .await
            .unwrap();
    assert!(
        !columns
            .iter()
            .any(|column| column == "content_operation_id")
    );
    sqlx::query("DROP TABLE content_conflict_copy_attempts")
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
    assert_integrity(&path, 23).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
}

async fn fixture_v20_for_v21(path: &Path, obstruct: bool) {
    fixture_v19(path, true, true, false).await;
    let mut db = connect(path).await;
    sqlx::query("PRAGMA user_version=20")
        .execute(&mut db)
        .await
        .unwrap();
    if obstruct {
        sqlx::raw_sql("CREATE TABLE deleted_draft_copy_intents (sentinel TEXT NOT NULL); INSERT INTO deleted_draft_copy_intents VALUES ('preserve-me');")
            .execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn v20_to_v21_preserves_drafts_operations_receipts_and_existing_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v20_for_v21(&path, false).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    let create_delete = decisions(&path).await;
    let moves = move_decisions(&path).await;
    let mut db = connect(&path).await;
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM client_outbox WHERE status IN ('queued','sent')")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(pending >= 2);
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let copy = store
        .read_content_conflict_copy(&frozen_binding_v16(), CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.node_id, COPY_NODE_V19);
    assert_eq!(copy.create_operation_id, COPY_CREATE_V19);
    assert_eq!(copy.local_bytes, V16_LOCAL);
    assert_eq!(
        store
            .read_registered_checkpoint(&frozen_binding_v16(), CONFLICT_NODE)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        V16_LOCAL
    );
    assert_eq!(
        store
            .read_outbox_operation(&frozen_binding_v16(), CONFLICT_OP)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        V16_SENT
    );
    store.close().await.unwrap();
    assert_integrity(&path, 23).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, create_delete);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deleted_draft_copy_intents")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let content_fk: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_list('deleted_draft_copy_intents') WHERE \"table\"='client_outbox' AND \"from\"='content_operation_id' AND \"to\"='operation_id'").fetch_one(&mut db).await.unwrap();
    assert_eq!(content_fk, 1);
    let unique: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pragma_index_list('deleted_draft_copy_intents') x JOIN pragma_index_info(x.name) y WHERE x.\"unique\"=1 AND x.partial=0 GROUP BY x.name HAVING count(*)=1 AND max(y.name)='content_operation_id')").fetch_one(&mut db).await.unwrap();
    assert_eq!(unique, 1);
    let attempt_count: i64 = sqlx::query_scalar("SELECT count(*) FROM deleted_draft_copy_attempts")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(attempt_count, 0);
    let attempt_fk: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_list('deleted_draft_copy_attempts') WHERE \"table\"='client_outbox' AND \"from\"='old_create_operation_id' AND \"to\"='operation_id'")
        .fetch_one(&mut db).await.unwrap();
    assert_eq!(attempt_fk, 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn failed_v20_to_v21_attempt_table_rolls_back_intents_and_recovery_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v20_for_v21(&path, false).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let mut db = connect(&path).await;
    sqlx::raw_sql("CREATE TABLE deleted_draft_copy_attempts (sentinel TEXT NOT NULL); INSERT INTO deleted_draft_copy_attempts VALUES ('preserve-me');")
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
    let sentinel: String = sqlx::query_scalar("SELECT sentinel FROM deleted_draft_copy_attempts")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(sentinel, "preserve-me");
    let intents_table: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='deleted_draft_copy_intents'")
        .fetch_one(&mut db).await.unwrap();
    assert_eq!(intents_table, 0);
    db.close().await.unwrap();
    let mut recovery = notryx_client_store::RecoveryStore::open(&path)
        .await
        .unwrap();
    assert!(!recovery.drafts(None).await.unwrap().items.is_empty());
    assert!(!recovery.operations(None).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn failed_v20_to_v21_rolls_back_and_keeps_recovery_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v20_for_v21(&path, true).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_integrity(&path, 20).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    let mut db = connect(&path).await;
    let sentinel: String = sqlx::query_scalar("SELECT sentinel FROM deleted_draft_copy_intents")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(sentinel, "preserve-me");
    db.close().await.unwrap();
    let mut recovery = notryx_client_store::RecoveryStore::open(&path)
        .await
        .unwrap();
    assert!(!recovery.drafts(None).await.unwrap().items.is_empty());
    assert!(!recovery.operations(None).await.unwrap().items.is_empty());
    drop(recovery);
    let mut db = connect(&path).await;
    sqlx::query("DROP TABLE deleted_draft_copy_intents")
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
    assert_integrity(&path, 23).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
}
