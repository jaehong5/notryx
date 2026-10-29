// Proposed store API:
//   enum FrozenCreateChoice { Apply { parent_id: NodeId, name: String }, Dismiss }
//   ClientStore::resolve_frozen_create(&old_binding, &current_binding, old_operation_id, &choice)
//     -> Result<..., StoreError>
// The return value is deliberately not assumed; the observable contract is read
// from SQLite and the existing list_create_operations/next_pending_outbox APIs.
// Proposed decision table name: create_reconciliations (old_operation_id PRIMARY KEY).
use notryx_client_store::{
    ClientStore, FrozenCreateChoice, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
};
use notryx_protocol::CreateNodeRequest;
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
        json!({"node_id":OLD_NODE,"parent_id":ROOT,"name":"original.md","kind":"file"}),
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

#[tokio::test]
async fn apply_creates_one_new_durable_operation_and_preserves_original() {
    for sent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, old, current, id) = setup(&path, sent).await;
        let before = old_row(&path, &id).await;
        store
            .resolve_frozen_create(&old, &current, &id, &apply("chosen.md"))
            .await
            .unwrap();
        let PendingOutboxOperation::Create(new) =
            store.next_pending_outbox(&current).await.unwrap().unwrap()
        else {
            panic!("new create required")
        };
        assert_eq!(new.operation_seq, 1);
        assert_ne!(new.operation_id, id);
        assert_ne!(new.request.node_id.to_string(), OLD_NODE);
        assert_eq!(
            Uuid::parse_str(&new.operation_id)
                .unwrap()
                .get_version_num(),
            7
        );
        assert_eq!(
            Uuid::parse_str(&new.request.node_id.to_string())
                .unwrap()
                .get_version_num(),
            7
        );
        assert_eq!(new.request.parent_id.to_string(), PARENT);
        assert_eq!(new.request.kind, notryx_protocol::NodeKind::File);
        assert_eq!(new.request.name, "chosen.md");
        assert_eq!(new.binding, current);
        assert_eq!(old_row(&path, &id).await, before);
        assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
        store
            .resolve_frozen_create(&old, &current, &id, &apply("chosen.md"))
            .await
            .unwrap();
        assert_eq!(
            store.next_pending_outbox(&current).await.unwrap(),
            Some(PendingOutboxOperation::Create(new.clone()))
        );
        assert!(
            store
                .resolve_frozen_create(&old, &current, &id, &apply("different.md"))
                .await
                .is_err()
        );
        assert!(
            store
                .resolve_frozen_create(&old, &current, &id, &FrozenCreateChoice::Dismiss)
                .await
                .is_err()
        );
        assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        store
            .resolve_frozen_create(&old, &current, &id, &apply("chosen.md"))
            .await
            .unwrap();
        assert_eq!(
            store.next_pending_outbox(&current).await.unwrap(),
            Some(PendingOutboxOperation::Create(new))
        );
        assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
        assert_eq!(old_row(&path, &id).await, before);
        store.close().await.unwrap();
    }
}
