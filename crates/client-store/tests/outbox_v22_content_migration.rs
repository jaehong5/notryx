#[path = "support/migration_fixtures.rs"]
#[allow(dead_code)]
mod migration_fixtures;
use migration_fixtures::{
    assert_integrity, completed_markers, connect, decisions, fixture_v21_content, move_decisions,
    snapshot, v16_intents,
};
use notryx_client_store::ClientStore;
use sqlx::Connection;
use std::path::Path;

async fn v21_all_rows(path: &Path) -> Vec<(String, Vec<String>)> {
    let mut db = connect(path).await;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT IN ('frozen_content_reconciliations', 'frozen_content_dismissals', 'credential_expiry', 'diagram_drafts', 'diagram_save_snapshots', 'diagram_conflict_copies') ORDER BY name",
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
    assert_integrity(&path, 27).await;
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
    let diagram_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM diagram_drafts")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(diagram_count, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM diagram_save_snapshots")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
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
