use notryx_client_store::{
    ClientStore, OutboxStatus, PendingOutboxOperation, RegistrationBinding, StoreError,
};
use notryx_protocol::MoveNodeRequest;
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const DESTINATION: &str = "019a1000-0000-7000-8000-000000000003";
const META_ETAG: &str = "\"meta-1\"";

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

async fn fixture(path: &Path, registration: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(registration).await.unwrap();
    store.close().await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"2","last_change_seq":"1"});
    let target = json!({"node_id":TARGET,"parent_id":ROOT,"name":"original.md","kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":"meta-1","meta_etag":META_ETAG,"node_etag":"\"node-1\"","content_hash":"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac","size":"5","children_generation":"0","last_change_seq":"1"});
    let destination = json!({"node_id":DESTINATION,"parent_id":ROOT,"name":"destination","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"destination-meta","meta_etag":"\"destination-meta\"","node_etag":"\"destination-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    let mut db = database(path).await;
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for (id, node) in [(ROOT, root), (TARGET, target), (DESTINATION, destination)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(id).bind(node.to_string()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn remote_snapshot(path: &Path) -> (Vec<(String, String, i64)>, String) {
    let mut db = database(path).await;
    let nodes = sqlx::query_as("SELECT node_id,state,content_ready FROM remote_nodes WHERE registration_id=1 ORDER BY node_id").fetch_all(&mut db).await.unwrap();
    let cursor = sqlx::query_scalar("SELECT cursor FROM remote_index WHERE registration_id=1")
        .fetch_one(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    (nodes, cursor)
}

fn same_parent_request(name: &str) -> MoveNodeRequest {
    MoveNodeRequest {
        expected_parent_id: ROOT.parse().unwrap(),
        parent_id: ROOT.parse().unwrap(),
        name: name.into(),
    }
}

fn receipt(
    registration: &RegistrationBinding,
    id: &str,
    name: &str,
    rev: &str,
    seq: &str,
) -> notryx_protocol::OperationReceipt {
    serde_json::from_value(json!({"status":"moved","operation_id":id,"operation_seq":"1","workspace_epoch":registration.workspace_epoch,
        "node":{"node_id":TARGET,"parent_id":ROOT,"name":name,"kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":rev,"meta_etag":format!("\"{rev}\""),"node_etag":"\"node-1\"","content_hash":"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac","size":"5","children_generation":"0","last_change_seq":seq},"commit_seq":seq})).unwrap()
}

#[tokio::test]
async fn move_status_refuses_corrupt_same_parent_rename_with_reused_metadata_etag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&registration, TARGET, b"base\n", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            TARGET,
            &edit.edit_session_id,
            1,
            b"local draft\n",
        )
        .await
        .unwrap();
    let item = store
        .queue_move(
            &registration,
            TARGET.parse().unwrap(),
            META_ETAG,
            &same_parent_request("renamed.md"),
        )
        .await
        .unwrap();
    store.prepare_pending_outbox(&registration).await.unwrap();
    let good = receipt(
        &registration,
        &item.operation_id,
        "renamed.md",
        "meta-2",
        "9",
    );
    store
        .apply_move_receipt(&registration, &good)
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let corrupt = receipt(
        &registration,
        &item.operation_id,
        "renamed.md",
        "meta-1",
        "9",
    );
    let mut db = database(&path).await;
    sqlx::query("UPDATE client_outbox SET receipt_json=? WHERE operation_id=?")
        .bind(serde_json::to_string(&corrupt).unwrap())
        .bind(&item.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let before = remote_snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store.list_move_operations(&registration, 0, 100).await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    let draft = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, before);
    let mut db = database(&path).await;
    let saved: String =
        sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(item.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<notryx_protocol::OperationReceipt>(&saved).unwrap(),
        corrupt
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn move_status_keeps_valid_noop_receipt_after_cache_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let item = store
        .queue_move(
            &registration,
            TARGET.parse().unwrap(),
            META_ETAG,
            &same_parent_request("original.md"),
        )
        .await
        .unwrap();
    assert!(matches!(
        store.prepare_pending_outbox(&registration).await.unwrap(),
        Some(PendingOutboxOperation::Move(_))
    ));
    let noop = receipt(
        &registration,
        &item.operation_id,
        "original.md",
        "meta-1",
        "1",
    );
    store
        .apply_move_receipt(&registration, &noop)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let raw: String =
        sqlx::query_scalar("SELECT state FROM remote_nodes WHERE registration_id=1 AND node_id=?")
            .bind(TARGET)
            .fetch_one(&mut db)
            .await
            .unwrap();
    let mut node: serde_json::Value = serde_json::from_str(&raw).unwrap();
    node["name"] = json!("peer.md");
    node["meta_rev"] = json!("meta-2");
    node["meta_etag"] = json!("\"meta-2\"");
    node["node_etag"] = json!("\"node-2\"");
    node["last_change_seq"] = json!("2");
    sqlx::query("UPDATE remote_nodes SET state=? WHERE registration_id=1 AND node_id=?")
        .bind(node.to_string())
        .bind(TARGET)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let before = remote_snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let page = store
        .list_move_operations(&registration, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation.status, OutboxStatus::Committed);
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&noop));
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, before);
}
