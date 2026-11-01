use notryx_client_store::{ClientStore, DiagramDraftCheckpoint, RegistrationBinding};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const NODE: &str = "019a1000-0000-7000-8000-000000000002";

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

async fn v25_fixture(path: &Path, collision: bool) -> (RegistrationBinding, String) {
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
                base_xml: "<mxfile/>".into(),
                base_revision: "original-rev".into(),
            },
        )
        .await
        .unwrap();
    let operation = store
        .queue_diagram_checkpoint(&binding, NODE, 1)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(path).await;
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut db)
        .await
        .unwrap();
    let ddl: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema WHERE name='client_outbox' AND type='table'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    let indexes: Vec<String> = sqlx::query_scalar("SELECT sql FROM sqlite_schema WHERE tbl_name='client_outbox' AND type='index' AND sql IS NOT NULL").fetch_all(&mut db).await.unwrap();
    let old = ddl
        .replace("client_outbox", "client_outbox_v25")
        .replace("26214400", "5242880");
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(old))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO client_outbox_v25 SELECT * FROM client_outbox; DROP TABLE client_outbox; ALTER TABLE client_outbox_v25 RENAME TO client_outbox; DROP TABLE IF EXISTS diagram_conflict_copies; DROP TABLE IF EXISTS diagram_save_snapshots; PRAGMA user_version=25;").execute(&mut *tx).await.unwrap();
    for index in indexes {
        sqlx::raw_sql(sqlx::AssertSqlSafe(index))
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::raw_sql("CREATE TABLE receipt_references(operation_id TEXT NOT NULL REFERENCES client_outbox(operation_id)); CREATE TABLE migration_audit(count INTEGER); INSERT INTO migration_audit VALUES(0); CREATE INDEX custom_outbox_name ON client_outbox(document_id); CREATE TRIGGER custom_outbox_update AFTER UPDATE ON client_outbox BEGIN UPDATE migration_audit SET count=count+1; END;").execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO receipt_references VALUES (?)")
        .bind(&operation.operation_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    if collision {
        sqlx::raw_sql("CREATE TABLE diagram_save_snapshots(sentinel TEXT); INSERT INTO diagram_save_snapshots VALUES('preserve-me');").execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut db)
            .await
            .unwrap()
            .is_none()
    );
    db.close().await.unwrap();
    (binding, operation.operation_id)
}

#[tokio::test]
async fn v26_migration_preserves_xml_requests_foreign_keys_indexes_and_triggers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (binding, operation_id) = v25_fixture(&path, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let operation = store
        .read_outbox_operation(&binding, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.bytes, b"<mxfile>pending</mxfile>");
    assert_eq!(
        store
            .read_diagram_operation_xml(&binding, &operation_id)
            .await
            .unwrap()
            .as_deref(),
        Some("<mxfile>pending</mxfile>")
    );
    assert_eq!(
        store
            .read_diagram_draft(&binding, NODE)
            .await
            .unwrap()
            .unwrap()
            .base_revision,
        "original-rev"
    );
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        27
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut db)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('custom_outbox_name','custom_outbox_update')").fetch_one(&mut db).await.unwrap(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count FROM migration_audit")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
    sqlx::query("UPDATE client_outbox SET status=status WHERE operation_id=?")
        .bind(&operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count FROM migration_audit")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1
    );
    let ddl: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_schema WHERE name='client_outbox'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(ddl.contains("26214400"));
    assert!(!ddl.contains("5242880"));
}

#[tokio::test]
async fn v26_migration_failure_rolls_back_rebuild_and_preserves_original_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (_, operation_id) = v25_fixture(&path, true).await;
    assert!(ClientStore::open(&path).await.is_err());
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        25
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sentinel FROM diagram_save_snapshots")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "preserve-me"
    );
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>("SELECT bytes FROM client_outbox WHERE operation_id=?")
            .bind(operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap(),
        b"<mxfile>pending</mxfile>"
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('custom_outbox_name','custom_outbox_update')").fetch_one(&mut db).await.unwrap(), 2);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut db)
            .await
            .unwrap()
            .is_none()
    );
    let ddl: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_schema WHERE name='client_outbox'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(ddl.contains("5242880"));
    assert!(!ddl.contains("26214400"));
}
