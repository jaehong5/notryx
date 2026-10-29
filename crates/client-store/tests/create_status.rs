use notryx_client_store::{
    ClientStore, CreateOutboxOperation, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::{CreateNodeRequest, OperationReceipt};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const NODES: [&str; 4] = [
    "019a1000-0000-7000-8000-000000000002",
    "019a1000-0000-7000-8000-000000000003",
    "019a1000-0000-7000-8000-000000000004",
    "019a1000-0000-7000-8000-000000000005",
];
fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn request(index: usize) -> CreateNodeRequest {
    serde_json::from_value(json!({"node_id":NODES[index],"parent_id":ROOT,"name":format!("folder-{index}"),"kind":"folder"})).unwrap()
}
fn success(b: &RegistrationBinding, op: &CreateOutboxOperation) -> OperationReceipt {
    serde_json::from_value(json!({"status":"created","operation_id":op.operation_id,"operation_seq":op.operation_seq.to_string(),"workspace_epoch":b.workspace_epoch,
        "node":{"node_id":op.request.node_id.to_string(),"parent_id":ROOT,"name":op.request.name,"kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"meta-9","meta_etag":"\"meta-9\"","node_etag":"\"node-9\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"9"},"commit_seq":"9"})).unwrap()
}
fn conflict(b: &RegistrationBinding, op: &CreateOutboxOperation) -> OperationReceipt {
    serde_json::from_value(json!({"status":"destination_exists","operation_id":op.operation_id,"operation_seq":op.operation_seq.to_string(),"workspace_epoch":b.workspace_epoch})).unwrap()
}
async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}
async fn fixture(path: &Path, b: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(b).await.unwrap();
    store.close().await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"7","last_change_seq":"7"});
    let mut conn = db(path).await;
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'7')")
        .bind(ROOT)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,1)",
    )
    .bind(ROOT)
    .bind(root.to_string())
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}
async fn tree(path: &Path) -> (Vec<(String, String, i64)>, String) {
    let mut conn = db(path).await;
    let nodes=sqlx::query_as("SELECT node_id,state,content_ready FROM remote_nodes WHERE registration_id=1 ORDER BY node_id").fetch_all(&mut conn).await.unwrap();
    let cursor = sqlx::query_scalar("SELECT cursor FROM remote_index WHERE registration_id=1")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    (nodes, cursor)
}
async fn send(store: &mut ClientStore, b: &RegistrationBinding) -> CreateOutboxOperation {
    let PendingOutboxOperation::Create(op) =
        store.prepare_pending_outbox(b).await.unwrap().unwrap()
    else {
        panic!("create must be selected in FIFO")
    };
    op
}

#[tokio::test]
async fn create_list_preserves_status_request_receipt_and_frozen_binding_without_mutating_remote_tree()
 {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let before = tree(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let q0 = store.queue_create(&b, &request(0)).await.unwrap();
    let s0 = send(&mut store, &b).await;
    assert_eq!(q0.operation_id, s0.operation_id);
    let r0 = success(&b, &s0);
    store.apply_create_receipt(&b, &r0).await.unwrap();
    let q1 = store.queue_create(&b, &request(1)).await.unwrap();
    let s1 = send(&mut store, &b).await;
    assert_eq!(q1.operation_id, s1.operation_id);
    let r1 = conflict(&b, &s1);
    store.apply_create_receipt(&b, &r1).await.unwrap();
    let q2 = store.queue_create(&b, &request(2)).await.unwrap();
    let s2 = send(&mut store, &b).await;
    assert_eq!(q2.operation_id, s2.operation_id);
    let q3 = store.queue_create(&b, &request(3)).await.unwrap();
    let page = store.list_create_operations(&b, 0, 100).await.unwrap();
    assert_eq!(page.entries.len(), 4);
    assert_eq!(page.next_after_seq, None);
    for (index, entry) in page.entries.iter().enumerate() {
        assert_eq!(entry.operation.request, request(index));
        assert_eq!(entry.operation.operation_seq, index as u64 + 1);
        assert_eq!(entry.operation.binding, b);
        assert!(!entry.frozen);
    }
    assert_eq!(page.entries[0].operation.status, OutboxStatus::Committed);
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&r0));
    assert_eq!(page.entries[1].operation.status, OutboxStatus::Failed);
    assert_eq!(page.entries[1].receipt.as_ref(), Some(&r1));
    assert_eq!(page.entries[2].operation.status, OutboxStatus::Sent);
    assert!(page.entries[2].receipt.is_none());
    assert_eq!(page.entries[3].operation.status, OutboxStatus::Queued);
    assert!(page.entries[3].receipt.is_none());
    assert_eq!(page.entries[3].operation.operation_id, q3.operation_id);
    assert_eq!(tree(&path).await, before);
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let restored = store.list_create_operations(&b, 0, 100).await.unwrap();
    assert_eq!(restored, page);
    let mut replacement = b.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    let frozen = store.list_create_operations(&b, 0, 100).await.unwrap();
    assert!(frozen.entries.iter().all(|entry| entry.frozen));
    for (old, new) in page.entries.iter().zip(&frozen.entries) {
        assert_eq!(old.operation, new.operation);
        assert_eq!(old.receipt, new.receipt)
    }
    assert!(
        store
            .list_create_operations(&replacement, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(tree(&path).await, before);
    store.close().await.unwrap();
}

#[tokio::test]
async fn create_list_uses_exclusive_bounded_sequence_keysets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let before = tree(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    for index in 0..4 {
        store.queue_create(&b, &request(index)).await.unwrap();
    }
    for limit in [0, 101] {
        assert!(matches!(
            store.list_create_operations(&b, 0, limit).await,
            Err(StoreError::InvalidRequest)
        ));
    }
    assert!(matches!(
        store.list_create_operations(&b, u64::MAX, 1).await,
        Err(StoreError::InvalidRequest)
    ));
    let p1 = store.list_create_operations(&b, 0, 2).await.unwrap();
    assert_eq!(
        p1.entries
            .iter()
            .map(|x| x.operation.operation_seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(p1.next_after_seq, Some(2));
    let p2 = store
        .list_create_operations(&b, p1.next_after_seq.unwrap(), 2)
        .await
        .unwrap();
    assert_eq!(
        p2.entries
            .iter()
            .map(|x| x.operation.operation_seq)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(p2.next_after_seq, None);
    assert!(
        store
            .list_create_operations(&b, 4, 1)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(tree(&path).await, before);
    store.close().await.unwrap();
}

#[tokio::test]
async fn corrupted_create_receipt_is_rejected_and_metadata_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let before = tree(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let op = store.queue_create(&b, &request(0)).await.unwrap();
    let sent = send(&mut store, &b).await;
    let receipt = success(&b, &sent);
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    store.close().await.unwrap();
    let mut corrupt = serde_json::to_value(receipt).unwrap();
    corrupt["node"]["parent_id"] = json!(NODES[1]);
    for raw in ["{}".to_string(), corrupt.to_string()] {
        let mut conn = db(&path).await;
        sqlx::query("UPDATE client_outbox SET receipt_json=? WHERE operation_id=?")
            .bind(raw)
            .bind(&op.operation_id)
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert!(matches!(
            store.list_create_operations(&b, 0, 100).await,
            Err(StoreError::UnsupportedFormat)
        ));
        assert_eq!(store.receipt_ack_through(&b).await.unwrap(), 1);
        assert_eq!(tree(&path).await, before);
        store.close().await.unwrap();
    }
}
