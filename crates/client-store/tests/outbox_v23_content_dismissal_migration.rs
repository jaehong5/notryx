#[path = "support/migration_fixtures.rs"]
#[allow(dead_code)]
mod migration_fixtures;
use migration_fixtures::{assert_integrity, connect, fixture_v21_content};
use notryx_client_store::ClientStore;
use sqlx::Connection;
use std::path::Path;

// The v22 schema is fixed here independently of the migration under test.
async fn fixture_v22_dismissal(path: &Path, obstruct: bool) {
    fixture_v21_content(path, false).await;
    let mut db = connect(path).await;
    sqlx::raw_sql(
        r#"
        CREATE TABLE frozen_content_reconciliations (
            old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
            current_registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            new_operation_id TEXT NOT NULL UNIQUE REFERENCES client_outbox(operation_id),
            approval_hash TEXT NOT NULL,
            history_hash TEXT NOT NULL,
            view_fingerprint TEXT NOT NULL,
            new_operation_seq INTEGER NOT NULL CHECK(new_operation_seq > 0),
            generation INTEGER NOT NULL CHECK(generation > 0),
            draft_bytes BLOB NOT NULL,
            base_bytes BLOB NOT NULL,
            base_revision TEXT NOT NULL,
            old_payload BLOB NOT NULL,
            old_payload_hash TEXT NOT NULL,
            old_status TEXT NOT NULL,
            old_generation INTEGER NOT NULL,
            old_seq INTEGER NOT NULL,
            old_expected_revision TEXT,
            old_dependency TEXT,
            old_payload_len INTEGER NOT NULL,
            old_payload_retired INTEGER NOT NULL,
            new_payload_hash TEXT NOT NULL,
            new_expected_revision TEXT NOT NULL
        );
        INSERT INTO frozen_content_reconciliations VALUES (
            'op-2',1,'op-1','synthetic-approval','synthetic-history','synthetic-view',1,3,
            X'6e65776572206472616674',X'62617365','rev-a',X'696e20666c69676874',
            'synthetic-old-hash','sent',2,2,'rev-a',NULL,9,0,'synthetic-new-hash','rev-a'
        );
        PRAGMA user_version=22;
    "#,
    )
    .execute(&mut db)
    .await
    .unwrap();
    if obstruct {
        sqlx::raw_sql("CREATE TABLE frozen_content_dismissals (sentinel TEXT NOT NULL); INSERT INTO frozen_content_dismissals VALUES ('preserve-me');")
            .execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn v22_all_rows(path: &Path) -> Vec<(String, Vec<String>)> {
    let mut db = connect(path).await;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT IN ('frozen_content_dismissals', 'credential_expiry') ORDER BY name",
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
            .map(|column| format!("quote(\"{}\")", column.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" || ':' || ");
        let query = format!(
            "SELECT {} FROM \"{}\" ORDER BY 1",
            expression,
            table.replace('"', "\"\"")
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
async fn v23_dismissal_migration_preserves_all_v22_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v22_dismissal(&path, false).await;
    assert_integrity(&path, 22).await;
    let before = v22_all_rows(&path).await;
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(v22_all_rows(&path).await, before);
    let mut db = connect(&path).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM frozen_content_dismissals")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.close().await.unwrap();
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(v22_all_rows(&path).await, before);
    let mut recovery = notryx_client_store::RecoveryStore::open(&path)
        .await
        .unwrap();
    assert!(!recovery.drafts(None).await.unwrap().items.is_empty());
    assert!(!recovery.operations(None).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn v23_dismissal_collision_rolls_back_and_preserves_v22_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v22_dismissal(&path, true).await;
    assert_integrity(&path, 22).await;
    let before = v22_all_rows(&path).await;
    assert!(ClientStore::open(&path).await.is_err());
    assert_integrity(&path, 22).await;
    assert_eq!(v22_all_rows(&path).await, before);
    let mut db = connect(&path).await;
    let sentinel: String = sqlx::query_scalar("SELECT sentinel FROM frozen_content_dismissals")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(sentinel, "preserve-me");
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('frozen_content_dismissals') ORDER BY cid",
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
