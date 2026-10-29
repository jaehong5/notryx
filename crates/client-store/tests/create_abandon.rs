use notryx_client_store::{
    ClientStore, CreateOutboxOperation, OutboxAbandonState, OutboxStatus, PendingOutboxOperation,
    RegistrationBinding, StoreError,
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
async fn send(store: &mut ClientStore, b: &RegistrationBinding) -> CreateOutboxOperation {
    let PendingOutboxOperation::Create(op) =
        store.prepare_pending_outbox(b).await.unwrap().unwrap()
    else {
        panic!("create must be selected in FIFO")
    };
    op
}

#[tokio::test]
async fn create_abandon_matching_is_durable_and_does_not_skip_fifo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let first = store.queue_create(&b, &request(0)).await.unwrap();
    let second = store.queue_create(&b, &request(1)).await.unwrap();
    let sent = send(&mut store, &b).await;
    assert_eq!(sent.operation_id, first.operation_id);
    assert_eq!(sent.status, OutboxStatus::Sent);
    assert!(
        !store
            .request_create_abandon_matching(&b, &first)
            .await
            .unwrap()
    );
    assert!(
        !store
            .request_create_abandon_matching(&b, &second)
            .await
            .unwrap()
    );
    let mut wrong = sent.clone();
    wrong.request.name = "different".into();
    assert!(
        !store
            .request_create_abandon_matching(&b, &wrong)
            .await
            .unwrap()
    );
    assert_eq!(
        store.next_pending_outbox(&b).await.unwrap(),
        Some(PendingOutboxOperation::Create(sent.clone()))
    );
    assert!(
        store
            .request_create_abandon_matching(&b, &sent)
            .await
            .unwrap()
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Create(intent) =
        store.next_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("first create must remain at FIFO head")
    };
    assert_eq!(intent.operation_id, sent.operation_id);
    assert_eq!(intent.request, sent.request);
    assert_eq!(intent.status, OutboxStatus::Abandoning);
    assert_eq!(intent.abandon_state, OutboxAbandonState::Requested);
    assert!(
        !store
            .request_create_abandon_matching(&b, &second)
            .await
            .unwrap()
    );
    let PendingOutboxOperation::Create(prepared) =
        store.prepare_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("abandon must remain at FIFO head")
    };
    assert_eq!(prepared.operation_id, first.operation_id);
    assert_eq!(prepared.abandon_state, OutboxAbandonState::Sent);
    assert_eq!(store.receipt_ack_through(&b).await.unwrap(), 0);
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Create(retry) =
        store.prepare_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("uncertain abandon must remain at FIFO head")
    };
    assert_eq!(retry, prepared);
    let receipt:OperationReceipt=serde_json::from_value(json!({"status":"abandoned","operation_id":prepared.operation_id,"operation_seq":"1","workspace_epoch":b.workspace_epoch})).unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    assert_eq!(store.receipt_ack_through(&b).await.unwrap(), 1);
    let PendingOutboxOperation::Create(next) =
        store.next_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("second create should become FIFO head after terminal receipt")
    };
    assert_eq!(next.operation_id, second.operation_id);
    assert_eq!(next.request, second.request);
    store.close().await.unwrap();
}

#[tokio::test]
async fn frozen_create_cannot_be_abandoned_by_old_or_replacement_binding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    store.queue_create(&b, &request(0)).await.unwrap();
    let sent = send(&mut store, &b).await;
    assert!(
        store
            .request_create_abandon_matching(&b, &sent)
            .await
            .unwrap()
    );
    let PendingOutboxOperation::Create(requested) =
        store.next_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("missing requested operation")
    };
    let mut replacement = b.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    assert!(matches!(
        store.request_create_abandon_matching(&b, &requested).await,
        Err(StoreError::StaleRegistration)
    ));
    assert!(
        !store
            .request_create_abandon_matching(&replacement, &requested)
            .await
            .unwrap()
    );
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    let mut conn = db(&path).await;
    let (status, abandon, bytes, seq): (String, String, Vec<u8>, i64) = sqlx::query_as(
        "SELECT status,abandon_state,bytes,operation_seq FROM client_outbox WHERE operation_id=?",
    )
    .bind(&sent.operation_id)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    assert_eq!(status, "sent");
    assert_eq!(abandon, "requested");
    assert_eq!(seq, 1);
    assert_eq!(
        serde_json::from_slice::<CreateNodeRequest>(&bytes).unwrap(),
        sent.request
    );
}

#[tokio::test]
async fn abandon_write_failure_keeps_payload_and_fifo_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    store.queue_create(&b, &request(0)).await.unwrap();
    let sent = send(&mut store, &b).await;
    let mut conn = db(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_create_abandon BEFORE UPDATE OF abandon_state ON client_outbox BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;").execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    assert!(matches!(
        store.request_create_abandon_matching(&b, &sent).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(
        store.next_pending_outbox(&b).await.unwrap(),
        Some(PendingOutboxOperation::Create(sent.clone()))
    );
    assert_eq!(store.receipt_ack_through(&b).await.unwrap(), 0);
    let mut conn = db(&path).await;
    sqlx::raw_sql("DROP TRIGGER reject_create_abandon")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    assert!(
        store
            .request_create_abandon_matching(&b, &sent)
            .await
            .unwrap()
    );
    store.close().await.unwrap();
}
