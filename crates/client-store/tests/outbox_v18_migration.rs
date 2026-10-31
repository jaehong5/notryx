#[path = "support/migration_fixtures.rs"]
#[allow(dead_code)]
mod migration_fixtures;
use migration_fixtures::{
    COMPLETED_OP, CONFLICT_NODE, CONFLICT_OP, FOLLOWUP_OP, V16_BASE, V16_LOCAL, V16_REMOTE,
    V16_SENT, assert_integrity, completed_markers, connect, decisions, fixture_v17,
    frozen_binding_v16, hash, move_decisions, snapshot, v16_intents,
};
use notryx_client_store::{ClientStore, StoreError};
use sqlx::Connection;

#[tokio::test]
async fn v17_to_v18_preserves_existing_data_and_creates_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v17(&path, false).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    let create_delete = decisions(&path).await;
    let moves = move_decisions(&path).await;
    assert_eq!(
        markers,
        vec![
            (CONFLICT_OP.to_owned(), None, None),
            (COMPLETED_OP.to_owned(), Some(2), Some(1))
        ]
    );

    let mut store = ClientStore::open(&path).await.unwrap();
    let preserved = store
        .read_content_conflict_intent(&frozen_binding_v16(), CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preserved.base_bytes, V16_BASE);
    assert_eq!(preserved.local_bytes, V16_LOCAL);
    assert_eq!(preserved.remote_bytes, V16_REMOTE);
    assert_eq!(preserved.remote_hash, hash(V16_REMOTE));
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
    assert_eq!(
        store
            .read_outbox_operation(&frozen_binding_v16(), FOLLOWUP_OP)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        V16_LOCAL
    );
    store.close().await.unwrap();

    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, create_delete);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let history_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM content_conflict_intent_history")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(history_count, 0);
    db.close().await.unwrap();
    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn failed_v18_migration_rolls_back_version_and_preserves_obstruction_and_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v17(&path, true).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    let create_delete = decisions(&path).await;
    let moves = move_decisions(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_integrity(&path, 17).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, create_delete);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let sentinel: String =
        sqlx::query_scalar("SELECT sentinel FROM content_conflict_intent_history")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(sentinel, "preserve-me");
    sqlx::query("DROP TABLE content_conflict_intent_history")
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
    assert_eq!(decisions(&path).await, create_delete);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let history_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM content_conflict_intent_history")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(history_count, 0);
    db.close().await.unwrap();
}
