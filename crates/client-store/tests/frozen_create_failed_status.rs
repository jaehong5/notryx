// Proposed store API:
//   enum FrozenCreateChoice { Apply { parent_id: NodeId, name: String }, Dismiss }
//   ClientStore::resolve_frozen_create(&old_binding, &current_binding, old_operation_id, &choice)
//     -> Result<..., StoreError>
// The return value is deliberately not assumed; the observable contract is read
// from SQLite and the existing list_create_operations/next_pending_outbox APIs.
// Proposed decision table name: create_reconciliations (old_operation_id PRIMARY KEY).
use notryx_client_store::{
    ClientStore, FrozenCreateChoice, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::{CreateNodeRequest, OperationReceipt, RestoreNodeRequest};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const PARENT: &str = "019a1000-0000-7000-8000-000000000002";
const OLD_NODE: &str = "019a1000-0000-7000-8000-000000000003";
fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn replacement(old: &RegistrationBinding) -> RegistrationBinding {
    RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..old.clone()
    }
}
fn old_request() -> CreateNodeRequest {
    serde_json::from_value(
        json!({"node_id":OLD_NODE,"parent_id":ROOT,"name":"original name","kind":"folder"}),
    )
    .unwrap()
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
async fn seed_nodes(path: &Path, b: &RegistrationBinding) {
    let mut conn = db(path).await;
    let id:i64=sqlx::query_scalar("SELECT id FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?").bind(&b.server_origin).bind(&b.workspace_id).bind(&b.workspace_epoch).bind(&b.device_id).bind(&b.device_stream_id).fetch_one(&mut conn).await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"1","last_change_seq":"1"});
    let parent = json!({"node_id":PARENT,"parent_id":ROOT,"name":"target","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"parent-meta","meta_etag":"\"parent-meta\"","node_etag":"\"parent-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    sqlx::query("INSERT INTO remote_index VALUES (?,?,'1')")
        .bind(id)
        .bind(ROOT)
        .execute(&mut conn)
        .await
        .unwrap();
    for (node, state) in [(ROOT, root), (PARENT, parent)] {
        sqlx::query("INSERT INTO remote_nodes VALUES (?,?,?,0)")
            .bind(id)
            .bind(node)
            .bind(state.to_string())
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn.close().await.unwrap();
}
async fn old_row(
    path: &Path,
    op: &str,
) -> (
    String,
    i64,
    String,
    String,
    Vec<u8>,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
) {
    let mut conn = db(path).await;
    let row=sqlx::query_as("SELECT r.workspace_id,o.operation_seq,r.device_id,r.device_stream_id,o.bytes,o.payload_hash,o.receipt_json,o.dependency,o.status,o.abandon_state,o.terminal_kind FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id=?").bind(op).fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    row
}
async fn counts(path: &Path, b: &RegistrationBinding) -> (i64, i64, Option<i64>) {
    let mut conn = db(path).await;
    let old_decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM create_reconciliations")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    let (items,next):(i64,Option<i64>)=sqlx::query_as("SELECT (SELECT COUNT(*) FROM client_outbox o WHERE o.registration_id=r.id),(SELECT s.next_seq FROM outbox_streams s WHERE s.registration_id=r.id) FROM client_registrations r WHERE r.device_id=? AND r.device_stream_id=?").bind(&b.device_id).bind(&b.device_stream_id).fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    (old_decisions, items, next)
}
const OTHER_NODE: &str = "019a1000-0000-7000-8000-000000000009";
fn restore_request() -> RestoreNodeRequest {
    serde_json::from_value(
        json!({"expected_meta_rev":"old-meta","parent_id":ROOT,"name":"restored"}),
    )
    .unwrap()
}
fn other_request() -> CreateNodeRequest {
    serde_json::from_value(
        json!({"node_id":OTHER_NODE,"parent_id":ROOT,"name":"other","kind":"folder"}),
    )
    .unwrap()
}
fn failed_receipt(old: &RegistrationBinding, id: &str, status: &str) -> OperationReceipt {
    serde_json::from_value(json!({"status":status,"operation_id":id,"operation_seq":"1","workspace_epoch":old.workspace_epoch})).unwrap()
}
async fn failed_fixture(
    path: &Path,
    status: &str,
) -> (
    ClientStore,
    RegistrationBinding,
    RegistrationBinding,
    String,
    OperationReceipt,
    String,
) {
    let old = binding();
    let current = replacement(&old);
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    seed_nodes(path, &old).await;
    let queued = store.queue_create(&old, &old_request()).await.unwrap();
    let PendingOutboxOperation::Create(sent) =
        store.prepare_pending_outbox(&old).await.unwrap().unwrap()
    else {
        panic!("expected create")
    };
    assert_eq!(sent.operation_id, queued.operation_id);
    let receipt = failed_receipt(&old, &queued.operation_id, status);
    store.apply_create_receipt(&old, &receipt).await.unwrap();
    let unknown = store.queue_create(&old, &other_request()).await.unwrap();
    assert_eq!(unknown.operation_seq, 2);
    store.activate_registration(&current).await.unwrap();
    seed_nodes(path, &current).await;
    (
        store,
        old,
        current,
        queued.operation_id,
        receipt,
        unknown.operation_id,
    )
}

#[tokio::test]
async fn confirmed_create_failure_is_listed_as_failure_and_dismissal_unblocks_restore_without_reuse()
 {
    for status in ["destination_exists", "node_id_conflict"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, old, current, id, receipt, unknown_id) =
            failed_fixture(&path, status).await;
        let before = old_row(&path, &id).await;
        let unknown_before = old_row(&path, &unknown_id).await;
        let page = store
            .list_frozen_creates(&current, None, 100)
            .await
            .unwrap();
        let failed = page
            .entries
            .iter()
            .find(|entry| entry.operation.operation_id == id)
            .expect("confirmed failure must be reviewable");
        assert!(failed.frozen);
        assert_eq!(failed.operation.binding, old);
        assert_eq!(failed.operation.request, old_request());
        assert_eq!(failed.operation.status, OutboxStatus::Failed);
        assert_eq!(failed.receipt.as_ref(), Some(&receipt));
        let unknown = page
            .entries
            .iter()
            .find(|entry| entry.operation.operation_id == unknown_id)
            .expect("queued create must remain reviewable");
        assert_eq!(unknown.operation.status, OutboxStatus::Queued);
        assert!(unknown.receipt.is_none());
        assert!(matches!(
            store
                .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
                .await,
            Err(StoreError::ReconciliationRequired)
        ));
        store
            .resolve_frozen_create(&old, &current, &id, &FrozenCreateChoice::Dismiss)
            .await
            .unwrap();
        assert_eq!(old_row(&path, &id).await, before);
        assert_eq!(old_row(&path, &unknown_id).await, unknown_before);
        let after = store
            .list_frozen_creates(&current, None, 100)
            .await
            .unwrap();
        assert!(
            !after
                .entries
                .iter()
                .any(|entry| entry.operation.operation_id == id)
        );
        assert!(
            after
                .entries
                .iter()
                .any(|entry| entry.operation.operation_id == unknown_id)
        );
        let restore = store
            .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
            .await
            .unwrap();
        assert_eq!(restore.node_id.to_string(), OLD_NODE);
        assert!(store.queue_create(&current, &old_request()).await.is_err());
        let mut conn = db(&path).await;
        let selected: (String, Option<String>) = sqlx::query_as(
            "SELECT decision,new_operation_id FROM create_reconciliations WHERE old_operation_id=?",
        )
        .bind(&id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        assert_eq!(selected, ("dismissed".into(), None));
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert_eq!(old_row(&path, &id).await, before);
        assert_eq!(old_row(&path, &unknown_id).await, unknown_before);
        assert_eq!(
            store
                .list_restore_operations(&current, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .operation,
            restore
        );
        assert!(
            store
                .list_frozen_creates(&current, None, 100)
                .await
                .unwrap()
                .entries
                .iter()
                .any(|entry| entry.operation.operation_id == unknown_id)
        );
        assert!(store.queue_create(&current, &old_request()).await.is_err());
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn corrupted_terminal_receipt_is_rejected_and_preserved() {
    for variant in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, _old, current, id, receipt, _unknown) =
            failed_fixture(&path, "destination_exists").await;
        let mut altered = serde_json::to_value(receipt).unwrap();
        let raw = match variant {
            0 => None,
            1 => Some("{}".to_string()),
            2 => {
                altered["operation_id"] = json!("019a1000-0000-7000-8000-000000000099");
                Some(altered.to_string())
            }
            _ => {
                altered["status"] = json!("node_id_conflict");
                Some(altered.to_string())
            }
        };
        let mut conn = db(&path).await;
        sqlx::query("UPDATE client_outbox SET receipt_json=? WHERE operation_id=?")
            .bind(raw)
            .bind(&id)
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let before = old_row(&path, &id).await;
        assert!(matches!(
            store.list_frozen_creates(&current, None, 100).await,
            Err(StoreError::UnsupportedFormat)
        ));
        assert_eq!(old_row(&path, &id).await, before);
        assert_eq!(counts(&path, &current).await, (0, 0, None));
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn confirmed_failure_in_other_workspace_is_not_listed_or_used_to_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let (mut store, old, _same_workspace, id, _receipt, _unknown) =
        failed_fixture(&path, "destination_exists").await;
    let before = old_row(&path, &id).await;
    let other = binding();
    assert_ne!(old.workspace_id, other.workspace_id);
    store.activate_registration(&other).await.unwrap();
    seed_nodes(&path, &other).await;
    assert!(
        store
            .list_frozen_creates(&other, None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        store
            .resolve_frozen_create(&old, &other, &id, &FrozenCreateChoice::Dismiss)
            .await
            .is_err()
    );
    let restore = store
        .queue_restore(&other, OLD_NODE.parse().unwrap(), &restore_request())
        .await
        .unwrap();
    assert_eq!(restore.operation_seq, 1);
    assert_eq!(old_row(&path, &id).await, before);
    store.close().await.unwrap();
}
