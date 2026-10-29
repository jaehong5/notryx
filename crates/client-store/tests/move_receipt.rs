use notryx_client_store::{
    ClientStore, OutboxAbandonState, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::{MoveNodeRequest, OperationReceipt};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const DESTINATION: &str = "019a1000-0000-7000-8000-000000000003";
const OTHER: &str = "019a1000-0000-7000-8000-000000000004";
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

fn move_request() -> MoveNodeRequest {
    MoveNodeRequest {
        expected_parent_id: ROOT.parse().unwrap(),
        parent_id: DESTINATION.parse().unwrap(),
        name: "renamed.md".into(),
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

fn moved_receipt(registration: &RegistrationBinding, operation_id: &str) -> serde_json::Value {
    json!({"status":"moved", "operation_id":operation_id, "operation_seq":"1", "workspace_epoch":registration.workspace_epoch,
        "node":{"node_id":TARGET,"parent_id":DESTINATION,"name":"renamed.md","kind":"file","deleted":false,"content_rev":"content-2","content_etag":"\"content-2\"","meta_rev":"meta-2","meta_etag":"\"meta-2\"","node_etag":"\"node-2\"","content_hash":"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac","size":"5","children_generation":"0","last_change_seq":"9"},"commit_seq":"9"})
}

#[tokio::test]
async fn move_receipt_validates_identity_and_node_then_preserves_newer_draft_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let tree = remote_snapshot(&path).await;
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
            &move_request(),
        )
        .await
        .unwrap();
    let sent = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&sent, PendingOutboxOperation::Move(item) if item.status == OutboxStatus::Sent)
    );
    let content = store
        .queue_checkpoint(&registration, TARGET, 1)
        .await
        .unwrap();
    assert_eq!(content.operation_seq, 2);
    let good = moved_receipt(&registration, &item.operation_id);
    let mut invalid = Vec::new();
    for (key, value) in [
        ("operation_id", json!(Uuid::now_v7().to_string())),
        ("operation_seq", json!("2")),
        ("workspace_epoch", json!(Uuid::new_v4().to_string())),
        ("commit_seq", json!("10")),
    ] {
        let mut value_copy = good.clone();
        value_copy[key] = value;
        invalid.push(value_copy);
    }
    for (key, value) in [
        ("node_id", json!(OTHER)),
        ("parent_id", json!(ROOT)),
        ("name", json!("unexpected.md")),
        ("meta_etag", json!("\"different\"")),
    ] {
        let mut value_copy = good.clone();
        value_copy["node"][key] = value;
        invalid.push(value_copy);
    }
    let mut wrong_kind = good.clone();
    wrong_kind["node"]["kind"] = json!("folder");
    for key in ["content_rev", "content_etag", "content_hash", "size"] {
        wrong_kind["node"][key] = serde_json::Value::Null;
    }
    invalid.push(wrong_kind);
    for value in invalid {
        let receipt: OperationReceipt = serde_json::from_value(value).unwrap();
        assert!(matches!(
            store.apply_move_receipt(&registration, &receipt).await,
            Err(StoreError::InvalidReceipt)
        ));
        assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
        assert_eq!(
            store.next_pending_outbox(&registration).await.unwrap(),
            Some(sent.clone())
        );
    }
    let receipt: OperationReceipt = serde_json::from_value(good).unwrap();
    store
        .apply_move_receipt(&registration, &receipt)
        .await
        .unwrap();
    store
        .apply_move_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert!(
        matches!(store.next_pending_outbox(&registration).await.unwrap(), Some(PendingOutboxOperation::Content(ref item)) if item.operation_id == content.operation_id && item.bytes == b"local draft\n")
    );
    let draft = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    assert_eq!(draft.base_revision, "content-1");
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, tree);
    let mut db = database(&path).await;
    let (status, saved): (String, String) =
        sqlx::query_as("SELECT status,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(&item.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(status, "committed");
    assert_eq!(
        serde_json::from_str::<OperationReceipt>(&saved).unwrap(),
        receipt
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn move_conflict_receipts_advance_ack_without_changing_draft_or_original_request() {
    for kind in [
        "metadata_precondition_failed",
        "destination_exists",
        "recovery_conflict",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let registration = binding();
        fixture(&path, &registration).await;
        let before = remote_snapshot(&path).await;
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
                &move_request(),
            )
            .await
            .unwrap();
        store.prepare_pending_outbox(&registration).await.unwrap();
        let mut value = json!({"status":kind,"operation_id":item.operation_id,"operation_seq":"1","workspace_epoch":registration.workspace_epoch});
        if kind == "metadata_precondition_failed" {
            value["current_meta_rev"] = json!("meta-2");
            value["current_meta_etag"] = json!("\"meta-2\"");
            let mut invalid = value.clone();
            invalid["current_meta_etag"] = json!(META_ETAG);
            let malformed: OperationReceipt = serde_json::from_value(invalid).unwrap();
            assert!(matches!(
                store.apply_move_receipt(&registration, &malformed).await,
                Err(StoreError::InvalidReceipt)
            ));
            assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
        }
        let receipt: OperationReceipt = serde_json::from_value(value).unwrap();
        store
            .apply_move_receipt(&registration, &receipt)
            .await
            .unwrap();
        assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
        assert!(
            store
                .next_pending_outbox(&registration)
                .await
                .unwrap()
                .is_none()
        );
        let draft = store
            .read_registered_checkpoint(&registration, TARGET)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft\n");
        assert_eq!(draft.base_bytes, b"base\n");
        store
            .close_registered_document(&registration, TARGET, &edit.edit_session_id)
            .await
            .unwrap();
        store.close().await.unwrap();
        assert_eq!(remote_snapshot(&path).await, before);
        let mut db = database(&path).await;
        let (status, terminal, etag, saved): (String, String, String, String) = sqlx::query_as("SELECT status,terminal_kind,expected_revision,receipt_json FROM client_outbox WHERE operation_id=?").bind(item.operation_id).fetch_one(&mut db).await.unwrap();
        assert_eq!(status, "conflict");
        assert_eq!(terminal, kind);
        assert_eq!(etag, META_ETAG);
        assert_eq!(
            serde_json::from_str::<OperationReceipt>(&saved).unwrap(),
            receipt
        );
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn move_abandon_intent_survives_restart_and_acknowledges_without_replacing_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let before = remote_snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_move(
            &registration,
            TARGET.parse().unwrap(),
            META_ETAG,
            &move_request(),
        )
        .await
        .unwrap();
    let PendingOutboxOperation::Move(sent) = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("move")
    };
    assert!(
        !store
            .request_move_abandon_matching(&registration, &queued)
            .await
            .unwrap()
    );
    assert!(
        store
            .request_move_abandon_matching(&registration, &sent)
            .await
            .unwrap()
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let pending = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&pending, PendingOutboxOperation::Move(item) if item.status == OutboxStatus::Abandoning && item.abandon_state == OutboxAbandonState::Requested && item.request == move_request() && item.expected_meta_etag == META_ETAG)
    );
    let prepared = store
        .prepare_pending_outbox_matching(&registration, &pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&prepared, PendingOutboxOperation::Move(item) if item.abandon_state == OutboxAbandonState::Sent)
    );
    let receipt: OperationReceipt = serde_json::from_value(json!({"status":"abandoned","operation_id":sent.operation_id,"operation_seq":"1","workspace_epoch":registration.workspace_epoch})).unwrap();
    store
        .apply_move_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert!(
        store
            .next_pending_outbox(&registration)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, before);
}

#[tokio::test]
async fn move_receipt_database_failure_keeps_fifo_and_draft_without_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let before = remote_snapshot(&path).await;
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
            &move_request(),
        )
        .await
        .unwrap();
    let pending = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let mut db = database(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_move_receipt BEFORE UPDATE OF receipt_json ON client_outbox WHEN OLD.operation_kind='move' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let receipt: OperationReceipt =
        serde_json::from_value(moved_receipt(&registration, &item.operation_id)).unwrap();
    assert!(matches!(
        store.apply_move_receipt(&registration, &receipt).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    assert_eq!(
        store.next_pending_outbox(&registration).await.unwrap(),
        Some(pending)
    );
    let draft = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, before);
}
