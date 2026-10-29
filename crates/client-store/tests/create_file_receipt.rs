use notryx_client_store::{
    ClientStore, CreateOutboxOperation, PendingOutboxOperation, RegistrationBinding, StoreError,
};
use notryx_protocol::{CreateNodeRequest, OperationReceipt};
use serde_json::{Value, json};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FOLDER: &str = "019a1000-0000-7000-8000-000000000002";
const OTHER: &str = "019a1000-0000-7000-8000-000000000003";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn request() -> CreateNodeRequest {
    serde_json::from_value(json!({"node_id":FOLDER,"parent_id":ROOT,"name":"new.md","kind":"file"}))
        .unwrap()
}
fn created(b: &RegistrationBinding, op: &CreateOutboxOperation) -> Value {
    json!({"status":"created","operation_id":op.operation_id,"operation_seq":op.operation_seq.to_string(),"workspace_epoch":b.workspace_epoch,
        "node":{"node_id":FOLDER,"parent_id":ROOT,"name":"new.md","kind":"file","deleted":false,
            "content_rev":"content-9","content_etag":"\"content-9\"","meta_rev":"meta-9","meta_etag":"\"meta-9\"","node_etag":"\"node-9\"",
            "content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"9"},"commit_seq":"9"})
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
async fn sent(store: &mut ClientStore, b: &RegistrationBinding) -> CreateOutboxOperation {
    let queued = store.queue_create(b, &request()).await.unwrap();
    let PendingOutboxOperation::Create(sent) =
        store.prepare_pending_outbox(b).await.unwrap().unwrap()
    else {
        panic!("create must be first in FIFO")
    };
    assert_eq!(queued.operation_id, sent.operation_id);
    assert_eq!(sent.operation_seq, 1);
    sent
}
#[derive(Debug, PartialEq, Eq)]
struct Persisted {
    bytes: Vec<u8>,
    hash: String,
    status: String,
    terminal: Option<String>,
    receipt: Option<String>,
    ack: i64,
    confirmed: i64,
    cursor: String,
}
async fn persisted(path: &Path, id: &str) -> Persisted {
    let mut conn = db(path).await;
    let (bytes,hash,status,terminal,receipt,ack,confirmed,cursor):ReceiptSnapshot=sqlx::query_as("SELECT o.bytes,o.payload_hash,o.status,o.terminal_kind,o.receipt_json,s.ack_through,s.confirmed_through,r.cursor FROM client_outbox o JOIN outbox_streams s ON s.registration_id=o.registration_id JOIN remote_index r ON r.registration_id=o.registration_id WHERE o.operation_id=?")
        .bind(id).fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    Persisted {
        bytes,
        hash,
        status,
        terminal,
        receipt,
        ack,
        confirmed,
        cursor,
    }
}

type ReceiptSnapshot = (
    Vec<u8>,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    String,
);

#[tokio::test]
async fn create_success_is_durable_idempotent_and_does_not_advance_metadata_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let op = sent(&mut store, &b).await;
    let before = persisted(&path, &op.operation_id).await;
    assert_eq!(before.status, "sent");
    assert_eq!(before.ack, 0);
    assert_eq!(before.cursor, "7");
    let receipt: OperationReceipt = serde_json::from_value(created(&b, &op)).unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    let after = persisted(&path, &op.operation_id).await;
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.hash, before.hash);
    assert_eq!(after.status, "committed");
    assert_eq!(after.terminal, None);
    assert_eq!(after.ack, 1);
    assert_eq!(after.confirmed, 0);
    assert_eq!(after.cursor, "7");
    assert_eq!(
        serde_json::from_str::<OperationReceipt>(after.receipt.as_ref().unwrap()).unwrap(),
        receipt
    );
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    assert_eq!(persisted(&path, &op.operation_id).await, after);
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    assert_eq!(persisted(&path, &op.operation_id).await, after);
    assert!(store.next_pending_outbox(&b).await.unwrap().is_none());
    store.close().await.unwrap();
}

#[tokio::test]
async fn mismatched_create_receipts_are_rejected_without_ack_or_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let op = sent(&mut store, &b).await;
    let before = persisted(&path, &op.operation_id).await;
    let mut wrong_node = created(&b, &op);
    wrong_node["node"]["node_id"] = json!(OTHER);
    let mut wrong_parent = created(&b, &op);
    wrong_parent["node"]["parent_id"] = json!(OTHER);
    let mut wrong_name = created(&b, &op);
    wrong_name["node"]["name"] = json!("different.md");
    let mut wrong_epoch = created(&b, &op);
    wrong_epoch["workspace_epoch"] = json!(Uuid::new_v4().to_string());
    let mut wrong_seq = created(&b, &op);
    wrong_seq["operation_seq"] = json!("2");
    let mut wrong_commit = created(&b, &op);
    wrong_commit["commit_seq"] = json!("10");
    for value in [
        wrong_node,
        wrong_parent,
        wrong_name,
        wrong_epoch,
        wrong_seq,
        wrong_commit,
    ] {
        let receipt: OperationReceipt = serde_json::from_value(value).unwrap();
        assert!(store.apply_create_receipt(&b, &receipt).await.is_err());
        assert_eq!(persisted(&path, &op.operation_id).await, before);
    }
    let good: OperationReceipt = serde_json::from_value(created(&b, &op)).unwrap();
    store.apply_create_receipt(&b, &good).await.unwrap();
    let committed = persisted(&path, &op.operation_id).await;
    assert!(
        store
            .apply_create_receipt(&b, &conflict(&b, &op))
            .await
            .is_err()
    );
    assert_eq!(persisted(&path, &op.operation_id).await, committed);
    store.close().await.unwrap();
}

#[tokio::test]
async fn name_conflict_receipt_is_saved_before_ack_and_keeps_original_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let op = sent(&mut store, &b).await;
    let before = persisted(&path, &op.operation_id).await;
    let receipt = conflict(&b, &op);
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    let after = persisted(&path, &op.operation_id).await;
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.hash, before.hash);
    assert_eq!(
        serde_json::from_slice::<CreateNodeRequest>(&after.bytes).unwrap(),
        request()
    );
    assert_eq!(after.status, "conflict");
    assert_eq!(after.terminal.as_deref(), Some("destination_exists"));
    assert_eq!(after.ack, 1);
    assert_eq!(after.confirmed, 0);
    assert_eq!(after.cursor, "7");
    assert_eq!(
        serde_json::from_str::<OperationReceipt>(after.receipt.as_ref().unwrap()).unwrap(),
        receipt
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    assert_eq!(persisted(&path, &op.operation_id).await, after);
    assert!(store.next_pending_outbox(&b).await.unwrap().is_none());
    store.close().await.unwrap();
}

#[tokio::test]
async fn late_receipt_cannot_mutate_frozen_or_replacement_registration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let op = sent(&mut store, &b).await;
    let receipt: OperationReceipt = serde_json::from_value(created(&b, &op)).unwrap();
    let mut replacement = b.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    let frozen = persisted(&path, &op.operation_id).await;
    assert!(matches!(
        store.apply_create_receipt(&b, &receipt).await,
        Err(StoreError::StaleRegistration)
    ));
    assert!(
        store
            .apply_create_receipt(&replacement, &receipt)
            .await
            .is_err()
    );
    assert_eq!(persisted(&path, &op.operation_id).await, frozen);
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn ack_write_failure_rolls_back_terminal_receipt_and_preserves_sent_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let op = sent(&mut store, &b).await;
    let before = persisted(&path, &op.operation_id).await;
    let mut conn = db(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_create_ack BEFORE UPDATE OF ack_through ON outbox_streams BEGIN SELECT RAISE(ABORT,'synthetic ack failure'); END;").execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    let receipt = conflict(&b, &op);
    assert!(store.apply_create_receipt(&b, &receipt).await.is_err());
    assert_eq!(persisted(&path, &op.operation_id).await, before);
    let mut conn = db(&path).await;
    sqlx::raw_sql("DROP TRIGGER reject_create_ack")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    let after = persisted(&path, &op.operation_id).await;
    assert_eq!(after.ack, 1);
    assert_eq!(after.terminal.as_deref(), Some("destination_exists"));
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.cursor, "7");
    store.close().await.unwrap();
}

#[tokio::test]
async fn pending_file_create_survives_restart_without_creating_remote_or_draft_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store.queue_create(&registration, &request()).await.unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let pending = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending, PendingOutboxOperation::Create(queued));
    store.close().await.unwrap();
    let mut conn = db(&path).await;
    let remote: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM remote_nodes WHERE node_id=?")
        .bind(FOLDER)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    let drafts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM draft_checkpoints")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!((remote, drafts), (0, 0));
    conn.close().await.unwrap();
}

#[tokio::test]
async fn empty_file_create_rejects_nonempty_or_inconsistent_receipts_without_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let op = sent(&mut store, &registration).await;
    let before = persisted(&path, &op.operation_id).await;
    let mut wrong_size = created(&registration, &op);
    wrong_size["node"]["size"] = json!("1");
    let mut wrong_hash = created(&registration, &op);
    wrong_hash["node"]["content_hash"] = json!(format!("sha256:{}", "a".repeat(64)));
    let mut wrong_etag = created(&registration, &op);
    wrong_etag["node"]["content_etag"] = json!("\"other\"");
    let mut folder = created(&registration, &op);
    folder["node"]["kind"] = json!("folder");
    for field in ["content_rev", "content_etag", "content_hash", "size"] {
        folder["node"][field] = Value::Null;
    }
    for value in [wrong_size, wrong_hash, wrong_etag, folder] {
        let receipt: OperationReceipt = serde_json::from_value(value).unwrap();
        assert!(
            store
                .apply_create_receipt(&registration, &receipt)
                .await
                .is_err()
        );
        assert_eq!(persisted(&path, &op.operation_id).await, before);
    }
    let receipt: OperationReceipt = serde_json::from_value(created(&registration, &op)).unwrap();
    store
        .apply_create_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    store.close().await.unwrap();
}

#[tokio::test]
async fn empty_file_receipt_accepts_valid_noncanonical_decimal_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let binding = binding();
    fixture(&path, &binding).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let operation = sent(&mut store, &binding).await;
    let mut value = created(&binding, &operation);
    value["node"]["size"] = json!("00");
    let receipt: OperationReceipt = serde_json::from_value(value).unwrap();
    store
        .apply_create_receipt(&binding, &receipt)
        .await
        .unwrap();
    let page = store
        .list_create_operations(&binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&receipt));
}
