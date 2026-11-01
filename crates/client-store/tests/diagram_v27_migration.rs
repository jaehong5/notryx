use notryx_client_store::{
    ClientStore, DiagramDraftCheckpoint, RecoveryStore, RegistrationBinding, StoreError,
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const NODE: &str = "019a1000-0000-7000-8000-000000000002";

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

async fn v26_fixture(path: &Path, collision: bool) -> (RegistrationBinding, String) {
    let binding = RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    };
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    store
        .save_diagram_draft(
            &binding,
            NODE,
            &DiagramDraftCheckpoint {
                expected_generation: 0,
                xml: "<mxfile>pending</mxfile>".into(),
                base_xml: "<mxfile>base</mxfile>".into(),
                base_revision: "original-rev".into(),
            },
        )
        .await
        .unwrap();
    let operation = store
        .queue_diagram_png(&binding, NODE, 1, b"\x89PNG\r\n\x1a\npreserved")
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(path).await;
    sqlx::raw_sql("DROP TABLE diagram_conflict_copies; PRAGMA user_version=26;")
        .execute(&mut db)
        .await
        .unwrap();
    if collision {
        sqlx::raw_sql("CREATE TABLE diagram_conflict_copies(sentinel TEXT); INSERT INTO diagram_conflict_copies VALUES('preserve-me');").execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    (binding, operation.operation_id)
}

#[tokio::test]
async fn v27_preserves_pending_png_and_xml_and_adds_empty_copy_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (binding, operation_id) = v26_fixture(&path, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let operation = store
        .read_outbox_operation(&binding, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.bytes, b"\x89PNG\r\n\x1a\npreserved");
    assert_eq!(
        store
            .read_diagram_operation_xml(&binding, &operation_id)
            .await
            .unwrap()
            .as_deref(),
        Some("<mxfile>pending</mxfile>")
    );
    let draft = store
        .read_diagram_draft(&binding, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 1);
    assert_eq!(draft.base_xml, "<mxfile>base</mxfile>");
    assert_eq!(draft.base_revision, "original-rev");
    store.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(
        recovery.operation_bytes(&operation_id).await.unwrap(),
        b"\x89PNG\r\n\x1a\npreserved"
    );
    drop(recovery);
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        27
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM diagram_conflict_copies")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut db)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "ok"
    );
}

#[tokio::test]
async fn v27_collision_rolls_back_without_changing_schema_or_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (_, operation_id) = v26_fixture(&path, true).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        26
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sentinel FROM diagram_conflict_copies")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "preserve-me"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT xml FROM diagram_drafts")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "<mxfile>pending</mxfile>"
    );
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>("SELECT bytes FROM client_outbox WHERE operation_id=?")
            .bind(&operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap(),
        b"\x89PNG\r\n\x1a\npreserved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT xml FROM diagram_save_snapshots WHERE operation_id=?"
        )
        .bind(&operation_id)
        .fetch_one(&mut db)
        .await
        .unwrap(),
        "<mxfile>pending</mxfile>"
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut db)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn v27_index_collision_rolls_back_the_new_table_and_preserves_pending_payload() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (_, operation_id) = v26_fixture(&path, false).await;
    let mut db = connect(&path).await;
    sqlx::raw_sql("CREATE TABLE diagram_conflict_completed(sentinel TEXT); INSERT INTO diagram_conflict_completed VALUES('preserve-me');")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        26
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sentinel FROM diagram_conflict_completed")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "preserve-me"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='diagram_conflict_copies'"
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>("SELECT bytes FROM client_outbox WHERE operation_id=?")
            .bind(operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap(),
        b"\x89PNG\r\n\x1a\npreserved"
    );
}
