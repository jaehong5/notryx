#[path = "support/migration_fixtures.rs"]
#[allow(dead_code)]
mod migration_fixtures;
use migration_fixtures::{
    assert_integrity, completed_markers, connect, fixture_v18, snapshot, v16_intents,
};
use notryx_client_store::{ClientStore, StoreError};
use sqlx::Connection;

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
