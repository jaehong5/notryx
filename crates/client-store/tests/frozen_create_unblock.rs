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
use notryx_protocol::{CreateNodeRequest, RestoreNodeRequest};
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
fn apply(name: &str) -> FrozenCreateChoice {
    FrozenCreateChoice::Apply {
        parent_id: PARENT.parse().unwrap(),
        name: name.into(),
    }
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
async fn setup(
    path: &Path,
    sent: bool,
) -> (
    ClientStore,
    RegistrationBinding,
    RegistrationBinding,
    String,
) {
    let old = binding();
    let current = replacement(&old);
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    seed_nodes(path, &old).await;
    let op = store.queue_create(&old, &old_request()).await.unwrap();
    let saved = old_row(path, &op.operation_id).await;
    assert_eq!(saved.7, None); // Normal create has no parent-create dependency.
    if sent {
        let PendingOutboxOperation::Create(item) =
            store.prepare_pending_outbox(&old).await.unwrap().unwrap()
        else {
            panic!("expected create")
        };
        assert_eq!(item.status, OutboxStatus::Sent);
    }
    store.activate_registration(&current).await.unwrap();
    seed_nodes(path, &current).await;
    (store, old, current, op.operation_id)
}

fn restore_request() -> RestoreNodeRequest {
    serde_json::from_value(
        json!({"expected_meta_rev":"old-meta","parent_id":ROOT,"name":"restored"}),
    )
    .unwrap()
}
async fn decision(path: &Path, id: &str) -> (String, Option<String>) {
    let mut conn = db(path).await;
    let value = sqlx::query_as(
        "SELECT decision,new_operation_id FROM create_reconciliations WHERE old_operation_id=?",
    )
    .bind(id)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    value
}

#[tokio::test]
async fn reviewed_frozen_create_unblocks_existing_node_operations_without_reusing_node_for_create()
{
    for sent in [false, true] {
        for choice in [apply("new folder"), FrozenCreateChoice::Dismiss] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("store.db");
            let (mut store, old, current, id) = setup(&path, sent).await;
            let before = old_row(&path, &id).await;
            store
                .resolve_frozen_create(&old, &current, &id, &choice)
                .await
                .unwrap();
            let selected = decision(&path, &id).await;
            assert_eq!(
                selected.0,
                if matches!(choice, FrozenCreateChoice::Dismiss) {
                    "dismissed"
                } else {
                    "applied"
                }
            );
            assert_eq!(old_row(&path, &id).await, before);
            let restored = store
                .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
                .await
                .unwrap();
            assert_eq!(restored.node_id.to_string(), OLD_NODE);
            let opened = store
                .open_registered_document(&current, OLD_NODE, b"base", "rev-a")
                .await
                .unwrap();
            store
                .save_registered_checkpoint(
                    &current,
                    OLD_NODE,
                    &opened.edit_session_id,
                    1,
                    b"local draft",
                )
                .await
                .unwrap();
            let edit = store.queue_checkpoint(&current, OLD_NODE, 1).await.unwrap();
            assert_eq!(edit.bytes, b"local draft");
            assert!(store.queue_create(&current, &old_request()).await.is_err());
            assert_eq!(old_row(&path, &id).await, before);
            assert_eq!(decision(&path, &id).await, selected);
            store.close().await.unwrap();
            let mut store = ClientStore::open(&path).await.unwrap();
            assert_eq!(old_row(&path, &id).await, before);
            assert_eq!(decision(&path, &id).await, selected);
            let restore = store
                .list_restore_operations(&current, 0, 100)
                .await
                .unwrap();
            assert_eq!(restore.entries.len(), 1);
            assert_eq!(restore.entries[0].operation, restored);
            let pending = store
                .read_outbox_operation(&current, &edit.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pending.bytes, b"local draft");
            assert!(store.queue_create(&current, &old_request()).await.is_err());
            store.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn reviewing_create_does_not_resolve_separate_frozen_content_or_restore() {
    for frozen_kind in ["content", "restore"] {
        for choice in [apply("new folder"), FrozenCreateChoice::Dismiss] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("store.db");
            let old = binding();
            let current = replacement(&old);
            let mut store = ClientStore::open(&path).await.unwrap();
            store.activate_registration(&old).await.unwrap();
            seed_nodes(&path, &old).await;
            let create = store.queue_create(&old, &old_request()).await.unwrap();
            let PendingOutboxOperation::Create(sent) =
                store.prepare_pending_outbox(&old).await.unwrap().unwrap()
            else {
                panic!("expected create")
            };
            assert_eq!(sent.operation_id, create.operation_id);
            let other_id = if frozen_kind == "content" {
                let opened = store
                    .open_registered_document(&old, OLD_NODE, b"old base", "rev-a")
                    .await
                    .unwrap();
                store
                    .save_registered_checkpoint(
                        &old,
                        OLD_NODE,
                        &opened.edit_session_id,
                        1,
                        b"old independent draft",
                    )
                    .await
                    .unwrap();
                let edit = store.queue_checkpoint(&old, OLD_NODE, 1).await.unwrap();
                store
                    .close_registered_document(&old, OLD_NODE, &opened.edit_session_id)
                    .await
                    .unwrap();
                edit.operation_id
            } else {
                store
                    .queue_restore(&old, OLD_NODE.parse().unwrap(), &restore_request())
                    .await
                    .unwrap()
                    .operation_id
            };
            let old_create = old_row(&path, &create.operation_id).await;
            let other_before = old_row(&path, &other_id).await;
            store.activate_registration(&current).await.unwrap();
            seed_nodes(&path, &current).await;
            store
                .resolve_frozen_create(&old, &current, &create.operation_id, &choice)
                .await
                .unwrap();
            let selected = decision(&path, &create.operation_id).await;
            assert!(matches!(
                store
                    .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
                    .await,
                Err(StoreError::ReconciliationRequired)
            ));
            let opened = store
                .open_registered_document(&current, OLD_NODE, b"current base", "rev-b")
                .await
                .unwrap();
            store
                .save_registered_checkpoint(
                    &current,
                    OLD_NODE,
                    &opened.edit_session_id,
                    1,
                    b"current local draft",
                )
                .await
                .unwrap();
            assert!(matches!(
                store.queue_checkpoint(&current, OLD_NODE, 1).await,
                Err(StoreError::ReconciliationRequired)
            ));
            assert_eq!(old_row(&path, &create.operation_id).await, old_create);
            assert_eq!(old_row(&path, &other_id).await, other_before);
            assert_eq!(decision(&path, &create.operation_id).await, selected);
            store.close().await.unwrap();
            let mut store = ClientStore::open(&path).await.unwrap();
            assert!(matches!(
                store
                    .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
                    .await,
                Err(StoreError::ReconciliationRequired)
            ));
            assert_eq!(old_row(&path, &create.operation_id).await, old_create);
            assert_eq!(old_row(&path, &other_id).await, other_before);
            assert_eq!(decision(&path, &create.operation_id).await, selected);
            store.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn same_workspace_other_origin_requires_visible_review_before_existing_node_unblocks() {
    for choice in [apply("new folder"), FrozenCreateChoice::Dismiss] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let old = binding();
        let mut current = replacement(&old);
        current.server_origin = "https://other.example".into();
        assert_eq!(old.workspace_id, current.workspace_id);
        assert_ne!(old.server_origin, current.server_origin);
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&old).await.unwrap();
        seed_nodes(&path, &old).await;
        let create = store.queue_create(&old, &old_request()).await.unwrap();
        let PendingOutboxOperation::Create(sent) =
            store.prepare_pending_outbox(&old).await.unwrap().unwrap()
        else {
            panic!("expected create")
        };
        assert_eq!(sent.operation_id, create.operation_id);
        let before = old_row(&path, &create.operation_id).await;
        store.activate_registration(&current).await.unwrap();
        seed_nodes(&path, &current).await;
        let page = store
            .list_frozen_creates(&current, None, 100)
            .await
            .unwrap();
        let entry = page
            .entries
            .iter()
            .find(|item| item.operation.operation_id == create.operation_id)
            .expect("old origin must be visible for explicit review");
        assert_eq!(entry.operation.binding, old);
        assert!(entry.frozen);
        assert!(matches!(
            store
                .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
                .await,
            Err(StoreError::ReconciliationRequired)
        ));
        let editor = store
            .open_registered_document(&current, OLD_NODE, b"base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &current,
                OLD_NODE,
                &editor.edit_session_id,
                1,
                b"current draft",
            )
            .await
            .unwrap();
        assert!(matches!(
            store.queue_checkpoint(&current, OLD_NODE, 1).await,
            Err(StoreError::ReconciliationRequired)
        ));
        store
            .resolve_frozen_create(&old, &current, &create.operation_id, &choice)
            .await
            .unwrap();
        let selected = decision(&path, &create.operation_id).await;
        let restored = store
            .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
            .await
            .unwrap();
        let edit = store.queue_checkpoint(&current, OLD_NODE, 1).await.unwrap();
        assert_eq!(edit.bytes, b"current draft");
        assert_eq!(old_row(&path, &create.operation_id).await, before);
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .list_restore_operations(&current, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .operation,
            restored
        );
        assert_eq!(
            store
                .read_outbox_operation(&current, &edit.operation_id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"current draft"
        );
        assert_eq!(decision(&path, &create.operation_id).await, selected);
        assert_eq!(old_row(&path, &create.operation_id).await, before);
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn different_workspace_remains_independent_of_unresolved_frozen_create() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let old = binding();
    let current = binding();
    assert_ne!(old.workspace_id, current.workspace_id);
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    seed_nodes(&path, &old).await;
    let create = store.queue_create(&old, &old_request()).await.unwrap();
    let before = old_row(&path, &create.operation_id).await;
    store.activate_registration(&current).await.unwrap();
    seed_nodes(&path, &current).await;
    assert!(
        store
            .list_frozen_creates(&current, None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let restored = store
        .queue_restore(&current, OLD_NODE.parse().unwrap(), &restore_request())
        .await
        .unwrap();
    assert_eq!(restored.operation_seq, 1);
    let editor = store
        .open_registered_document(&current, OLD_NODE, b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &current,
            OLD_NODE,
            &editor.edit_session_id,
            1,
            b"independent draft",
        )
        .await
        .unwrap();
    let edit = store.queue_checkpoint(&current, OLD_NODE, 1).await.unwrap();
    assert_eq!(edit.operation_seq, 2);
    assert_eq!(old_row(&path, &create.operation_id).await, before);
    store.close().await.unwrap();
}
