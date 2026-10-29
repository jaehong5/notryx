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

async fn fixture(path: &Path, registration: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(registration).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(path).await;
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
    db.close().await.unwrap();
}

#[tokio::test]
async fn delete_status_lists_pending_abandoning_and_terminal_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let item = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    let page = store
        .list_delete_operations(&registration, 0, 10)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation.operation_id, item.operation_id);
    assert_eq!(
        page.entries[0].operation.status,
        notryx_client_store::OutboxStatus::Queued
    );
    assert!(page.entries[0].receipt.is_none());
    let pending = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let notryx_client_store::PendingOutboxOperation::Delete(sent) = pending else {
        panic!("delete")
    };
    assert!(
        store
            .request_delete_abandon_matching(&registration, &sent)
            .await
            .unwrap()
    );
    let page = store
        .list_delete_operations(&registration, 0, 10)
        .await
        .unwrap();
    assert_eq!(
        page.entries[0].operation.status,
        notryx_client_store::OutboxStatus::Abandoning
    );
    let pending = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    store
        .prepare_pending_outbox_matching(&registration, &pending)
        .await
        .unwrap();
    let receipt = serde_json::from_value::<notryx_protocol::OperationReceipt>(json!({"status":"abandoned","operation_id":item.operation_id,"operation_seq":"1","workspace_epoch":registration.workspace_epoch})).unwrap();
    store
        .apply_delete_receipt(&registration, &receipt)
        .await
        .unwrap();
    let page = store
        .list_delete_operations(&registration, 0, 10)
        .await
        .unwrap();
    assert_eq!(
        page.entries[0].operation.status,
        notryx_client_store::OutboxStatus::Abandoned
    );
    assert_eq!(page.entries[0].operation.expected_node_etag, ETAG);
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&receipt));
    store.close().await.unwrap();
}
