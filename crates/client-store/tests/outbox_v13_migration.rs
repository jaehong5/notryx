use notryx_client_store::{ClientStore, RegistrationBinding};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const ETAG: &str = "\"node-1\"";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

async fn database(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn v12_fixture(path: &Path, obstruct: bool) -> RegistrationBinding {
    let registration = binding();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("fixtures/client_store_v7.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("fixtures/client_store_v12.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO client_registrations VALUES (1,?,?,?,?,?,'active')")
        .bind(&registration.server_origin)
        .bind(&registration.workspace_id)
        .bind(&registration.workspace_epoch)
        .bind(&registration.device_id)
        .bind(&registration.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO registered_documents VALUES (1,'other','fixture-storage'); INSERT INTO draft_checkpoints VALUES ('fixture-storage',1,1,X'6472616674'); INSERT INTO draft_bases VALUES ('fixture-storage',X'62617365','rev-a'); INSERT INTO outbox_streams VALUES (1,2,0,0);").execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,1,1,'delete',?,?,X'','sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','queued',0)")
        .bind(Uuid::now_v7().to_string()).bind(TARGET).bind(ETAG).execute(&mut db).await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    let target = json!({"node_id":TARGET,"parent_id":ROOT,"name":"target.md","kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":"meta-1","meta_etag":"\"meta-1\"","node_etag":ETAG,"content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"1"});
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for (id, state) in [(ROOT, root), (TARGET, target)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(id).bind(state.to_string()).execute(&mut db).await.unwrap();
    }
    if obstruct {
        sqlx::query("CREATE TABLE delete_reconciliations (obstruction TEXT)")
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
    registration
}

async fn snapshot(path: &Path) -> Vec<String> {
    let mut db = database(path).await;
    let rows = sqlx::query_scalar("SELECT 'outbox:'||quote(operation_id)||':'||operation_seq||':'||quote(expected_revision)||':'||hex(bytes)||':'||status FROM client_outbox UNION ALL SELECT 'draft:'||quote(document_id)||':'||generation||':'||hex(bytes) FROM draft_checkpoints UNION ALL SELECT 'base:'||quote(document_id)||':'||hex(bytes)||':'||quote(revision) FROM draft_bases UNION ALL SELECT 'stream:'||registration_id||':'||next_seq||':'||ack_through||':'||confirmed_through FROM outbox_streams ORDER BY 1").fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

#[tokio::test]
async fn v12_to_v13_preserves_delete_identity_draft_and_stream() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = v12_fixture(&path, false).await;
    let before = snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let page = store
        .list_delete_operations(&registration, 0, 10)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation.expected_node_etag, ETAG);
    let draft = store
        .read_registered_checkpoint(&registration, "other")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
    store.close().await.unwrap();
    assert_eq!(snapshot(&path).await, before);
    let mut db = database(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 23);
    let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(errors, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn failed_v13_migration_preserves_v12_data_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    v12_fixture(&path, true).await;
    let before = snapshot(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(notryx_client_store::StoreError::Storage)
    ));
    assert_eq!(snapshot(&path).await, before);
    let mut db = database(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 12);
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('delete_reconciliations')")
            .fetch_all(&mut db)
            .await
            .unwrap();
    assert_eq!(columns, vec!["obstruction"]);
    db.close().await.unwrap();
}
