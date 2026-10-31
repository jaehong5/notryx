include!("outbox_v20_migration.rs");

// This fixture constructs the v21 schema without using the current ClientStore.
async fn fixture_v21_content(path: &Path, obstruct: bool) {
    fixture_v20_for_v21(path, false).await;
    let mut db = connect(path).await;
    sqlx::raw_sql(r#"
        ALTER TABLE content_conflict_intents ADD COLUMN merged_hash TEXT;
        ALTER TABLE content_conflict_intent_history ADD COLUMN merged_hash TEXT;
        CREATE TABLE deleted_draft_copy_intents (
            registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            document_id TEXT NOT NULL,
            generation INTEGER NOT NULL CHECK(generation > 0),
            local_bytes BLOB NOT NULL CHECK(length(local_bytes) <= 5242880),
            local_hash TEXT NOT NULL,
            parent_id TEXT NOT NULL,
            name TEXT NOT NULL,
            node_id TEXT PRIMARY KEY NOT NULL,
            create_operation_id TEXT NOT NULL UNIQUE,
            content_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
            evidence_kind TEXT NOT NULL CHECK(evidence_kind IN ('tombstone', 'node_deleted')),
            tombstone_seq TEXT,
            terminal_operation_id TEXT REFERENCES client_outbox(operation_id),
            CHECK ((evidence_kind = 'tombstone' AND tombstone_seq IS NOT NULL AND terminal_operation_id IS NULL)
                OR (evidence_kind = 'node_deleted' AND tombstone_seq IS NULL AND terminal_operation_id IS NOT NULL)),
            UNIQUE(registration_id, document_id)
        );
        CREATE TABLE deleted_draft_copy_attempts (
            old_create_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
            registration_id INTEGER NOT NULL,
            document_id TEXT NOT NULL,
            old_parent_id TEXT NOT NULL,
            old_name TEXT NOT NULL,
            old_node_id TEXT NOT NULL,
            new_parent_id TEXT NOT NULL,
            new_name TEXT NOT NULL,
            new_node_id TEXT NOT NULL,
            new_create_operation_id TEXT NOT NULL UNIQUE,
            FOREIGN KEY(registration_id, document_id) REFERENCES deleted_draft_copy_intents(registration_id, document_id)
        );
        CREATE TABLE content_conflict_merged_operations (
            operation_id TEXT PRIMARY KEY NOT NULL REFERENCES content_conflict_intents(operation_id),
            registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            intent_version INTEGER NOT NULL CHECK(intent_version >= 0),
            content_operation_id TEXT NOT NULL UNIQUE REFERENCES client_outbox(operation_id),
            abandoned_through INTEGER NOT NULL CHECK(abandoned_through > 0)
        );
        PRAGMA user_version = 21;
    "#).execute(&mut db).await.unwrap();
    if obstruct {
        sqlx::raw_sql("CREATE TABLE frozen_content_reconciliations (sentinel TEXT NOT NULL); INSERT INTO frozen_content_reconciliations VALUES ('preserve-me');")
            .execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn v21_all_rows(path: &Path) -> Vec<(String, Vec<String>)> {
    let mut db = connect(path).await;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT IN ('frozen_content_reconciliations', 'frozen_content_dismissals', 'credential_expiry') ORDER BY name",
    ).fetch_all(&mut db).await.unwrap();
    let mut result = Vec::new();
    for table in tables {
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info(?) ORDER BY cid")
                .bind(&table)
                .fetch_all(&mut db)
                .await
                .unwrap();
        let expression = columns
            .iter()
            .map(|column| format!("quote(\"{}\")", column.replace('\"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" || ':' || ");
        let query = format!(
            "SELECT {} FROM \"{}\" ORDER BY 1",
            expression,
            table.replace('\"', "\"\"")
        );
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .fetch_all(&mut db)
            .await
            .unwrap();
        result.push((table, rows));
    }
    db.close().await.unwrap();
    result
}

#[tokio::test]
async fn v22_content_migration_preserves_v21_drafts_outbox_and_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v21_content(&path, false).await;
    assert_integrity(&path, 21).await;
    let before = snapshot(&path).await;
    let all_rows = v21_all_rows(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    let structures = decisions(&path).await;
    let moves = move_decisions(&path).await;
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v21_all_rows(&path).await, all_rows);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, structures);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM frozen_content_reconciliations")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.close().await.unwrap();
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v21_all_rows(&path).await, all_rows);
}

#[tokio::test]
async fn v22_content_collision_rolls_back_schema_version_and_preserves_v21_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v21_content(&path, true).await;
    let before = snapshot(&path).await;
    let all_rows = v21_all_rows(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    let structures = decisions(&path).await;
    let moves = move_decisions(&path).await;
    assert!(ClientStore::open(&path).await.is_err());
    assert_integrity(&path, 21).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v21_all_rows(&path).await, all_rows);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, structures);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let sentinel: String =
        sqlx::query_scalar("SELECT sentinel FROM frozen_content_reconciliations")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(sentinel, "preserve-me");
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('frozen_content_reconciliations') ORDER BY cid",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(columns, vec!["sentinel"]);
    db.close().await.unwrap();
    let mut recovery = notryx_client_store::RecoveryStore::open(&path)
        .await
        .unwrap();
    assert!(!recovery.drafts(None).await.unwrap().items.is_empty());
    assert!(!recovery.operations(None).await.unwrap().items.is_empty());
}
