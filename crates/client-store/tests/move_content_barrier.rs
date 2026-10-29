use notryx_client_store::{
    ClientStore, OutboxCommit, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
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

#[tokio::test]
async fn content_move_content_keeps_fifo_payloads_and_newer_draft_after_late_ack() {
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
            b"first edit\n",
        )
        .await
        .unwrap();
    let first = store
        .queue_checkpoint(&registration, TARGET, 1)
        .await
        .unwrap();
    let movement = store
        .queue_move(
            &registration,
            TARGET.parse().unwrap(),
            META_ETAG,
            &move_request(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            TARGET,
            &edit.edit_session_id,
            2,
            b"second edit\n",
        )
        .await
        .unwrap();
    let second = store
        .queue_checkpoint(&registration, TARGET, 2)
        .await
        .unwrap();
    assert_eq!(
        (
            first.operation_seq,
            movement.operation_seq,
            second.operation_seq
        ),
        (1, 2, 3)
    );
    assert_ne!(first.operation_id, second.operation_id);
    store
        .save_registered_checkpoint(
            &registration,
            TARGET,
            &edit.edit_session_id,
            3,
            b"newest draft\n",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Content(sent) = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("first content")
    };
    assert_eq!(sent.operation_id, first.operation_id);
    assert_eq!(sent.generation, 1);
    assert_eq!(sent.bytes, b"first edit\n");
    store
        .apply_outbox_commit(
            &registration,
            &OutboxCommit {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                document_id: TARGET.into(),
                content_revision: "content-2".into(),
                content_hash: sent.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    let PendingOutboxOperation::Move(sent_move) = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("move follows first content")
    };
    assert_eq!(sent_move.operation_id, movement.operation_id);
    assert_eq!(sent_move.expected_meta_etag, META_ETAG);
    assert_eq!(sent_move.status, OutboxStatus::Sent);
    let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(json!({"status":"moved", "operation_id":sent_move.operation_id, "operation_seq":"2", "workspace_epoch":registration.workspace_epoch,
        "node":{"node_id":TARGET,"parent_id":DESTINATION,"name":"renamed.md","kind":"file","deleted":false,"content_rev":"content-2","content_etag":"\"content-2\"","meta_rev":"meta-2","meta_etag":"\"meta-2\"","node_etag":"\"node-2\"","content_hash":sent.payload_hash,"size":sent.bytes.len().to_string(),"children_generation":"0","last_change_seq":"9"},"commit_seq":"9"})).unwrap();
    store
        .apply_move_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 2);
    let Some(PendingOutboxOperation::Content(next)) =
        store.next_pending_outbox(&registration).await.unwrap()
    else {
        panic!("second content follows move")
    };
    assert_eq!(next.operation_id, second.operation_id);
    assert_eq!(next.operation_seq, 3);
    assert_eq!(next.generation, 2);
    assert_eq!(next.bytes, b"second edit\n");
    assert_eq!(next.expected_revision.as_deref(), Some("content-2"));
    let draft = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 3);
    assert_eq!(draft.bytes, b"newest draft\n");
    assert_eq!(draft.base_bytes, b"first edit\n");
    assert_eq!(draft.base_revision, "content-2");
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, tree);
}
