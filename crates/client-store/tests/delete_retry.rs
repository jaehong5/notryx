use notryx_client_store::{ClientStore, RegistrationBinding};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const OTHER: &str = "019a1000-0000-7000-8000-000000000003";
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
async fn stale_delete_requires_new_confirmation_and_preserves_prior_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&registration, OTHER, b"other base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            OTHER,
            &edit.edit_session_id,
            1,
            b"other draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, OTHER, &edit.edit_session_id)
        .await
        .unwrap();
    let first = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    store.prepare_pending_outbox(&registration).await.unwrap();
    let receipt = serde_json::from_value::<notryx_protocol::OperationReceipt>(json!({"status":"node_precondition_failed","operation_id":first.operation_id,"operation_seq":"1","workspace_epoch":registration.workspace_epoch,"current_node_etag":"\"node-2\""})).unwrap();
    store
        .apply_delete_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert!(
        store
            .next_pending_outbox(&registration)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let state: String =
        sqlx::query_scalar("SELECT state FROM remote_nodes WHERE registration_id=1 AND node_id=?")
            .bind(TARGET)
            .fetch_one(&mut db)
            .await
            .unwrap();
    let mut node: serde_json::Value = serde_json::from_str(&state).unwrap();
    node["node_etag"] = json!("\"node-2\"");
    node["last_change_seq"] = json!("2");
    node["name"] = json!("renamed.md");
    node["meta_rev"] = json!("meta-2");
    node["meta_etag"] = json!("\"meta-2\"");
    sqlx::query("UPDATE remote_nodes SET state=? WHERE registration_id=1 AND node_id=?")
        .bind(node.to_string())
        .bind(TARGET)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(
        store
            .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
            .await
            .is_err()
    );
    let next = store
        .queue_delete(&registration, TARGET.parse().unwrap(), "\"node-2\"")
        .await
        .unwrap();
    assert_eq!(next.operation_seq, 2);
    assert_ne!(next.operation_id, first.operation_id);
    assert_eq!(next.expected_node_etag, "\"node-2\"");
    let draft = store
        .read_registered_checkpoint(&registration, OTHER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"other draft");
    assert_eq!(draft.base_bytes, b"other base");
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let (status, saved, etag): (String, String, String) = sqlx::query_as(
        "SELECT status,receipt_json,expected_revision FROM client_outbox WHERE operation_id=?",
    )
    .bind(&first.operation_id)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(status, "conflict");
    assert_eq!(etag, ETAG);
    assert_eq!(
        serde_json::from_str::<notryx_protocol::OperationReceipt>(&saved).unwrap(),
        receipt
    );
    db.close().await.unwrap();
}
