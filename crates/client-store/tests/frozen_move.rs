use notryx_client_store::{
    ClientStore, FrozenMoveChoice, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::MoveNodeRequest;
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

async fn frozen_fixture(
    path: &Path,
    sent: bool,
) -> (RegistrationBinding, RegistrationBinding, String) {
    let old = binding();
    fixture(path, &old).await;
    let mut store = ClientStore::open(path).await.unwrap();
    let edit = store
        .open_registered_document(&old, OTHER, b"other base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, OTHER, &edit.edit_session_id, 1, b"other draft")
        .await
        .unwrap();
    store
        .close_registered_document(&old, OTHER, &edit.edit_session_id)
        .await
        .unwrap();
    let movement = store
        .queue_move(&old, TARGET.parse().unwrap(), META_ETAG, &move_request())
        .await
        .unwrap();
    if sent {
        store.prepare_pending_outbox(&old).await.unwrap();
    }
    let mut current = binding();
    current.workspace_id = old.workspace_id.clone();
    current.workspace_epoch = old.workspace_epoch.clone();
    store.activate_registration(&current).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(path).await;
    sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    (old, current, movement.operation_id)
}

fn apply_choice(etag: &str) -> FrozenMoveChoice {
    FrozenMoveChoice::Apply {
        expected_meta_etag: etag.into(),
        request: move_request(),
    }
}

#[tokio::test]
async fn frozen_move_requires_explicit_current_confirmation_before_new_operation() {
    for sent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let (old, current, id) = frozen_fixture(&path, sent).await;
        let mut db = database(&path).await;
        let before: (i64, String, Vec<u8>, String, String) = sqlx::query_as("SELECT operation_seq,COALESCE(expected_revision,''),bytes,payload_hash,status FROM client_outbox WHERE operation_id=?").bind(&id).fetch_one(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        let page = store.list_frozen_moves(&current, None, 100).await.unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation.operation_id, id);
        assert_eq!(page.entries[0].operation.binding, old);
        assert_eq!(page.entries[0].operation.expected_meta_etag, META_ETAG);
        assert_eq!(page.entries[0].operation.request, move_request());
        assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
        assert!(
            store
                .resolve_frozen_move(&old, &current, &id, &apply_choice("\"stale\""))
                .await
                .is_err()
        );
        assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
        let choice = apply_choice(META_ETAG);
        store
            .resolve_frozen_move(&old, &current, &id, &choice)
            .await
            .unwrap();
        let Some(PendingOutboxOperation::Move(new)) =
            store.next_pending_outbox(&current).await.unwrap()
        else {
            panic!("new move")
        };
        assert_ne!(new.operation_id, id);
        assert_eq!(new.operation_seq, 1);
        assert_eq!(new.node_id.to_string(), TARGET);
        assert_eq!(new.expected_meta_etag, META_ETAG);
        assert_eq!(new.request, move_request());
        assert_eq!(new.status, OutboxStatus::Queued);
        store
            .resolve_frozen_move(&old, &current, &id, &choice)
            .await
            .unwrap();
        assert!(
            matches!(store.next_pending_outbox(&current).await.unwrap(), Some(PendingOutboxOperation::Move(item)) if item.operation_id == new.operation_id)
        );
        assert!(
            store
                .list_frozen_moves(&current, None, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        let draft = store
            .read_registered_checkpoint(&old, OTHER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"other draft");
        assert_eq!(draft.base_bytes, b"other base");
        store.close().await.unwrap();
        let mut db = database(&path).await;
        let after: (i64, String, Vec<u8>, String, String) = sqlx::query_as("SELECT operation_seq,COALESCE(expected_revision,''),bytes,payload_hash,status FROM client_outbox WHERE operation_id=?").bind(&id).fetch_one(&mut db).await.unwrap();
        assert_eq!(after, before);
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn frozen_move_dismiss_does_not_send_or_discard_original_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (old, current, id) = frozen_fixture(&path, true).await;
    let tree = remote_snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .resolve_frozen_move(&old, &current, &id, &FrozenMoveChoice::Dismiss)
        .await
        .unwrap();
    store
        .resolve_frozen_move(&old, &current, &id, &FrozenMoveChoice::Dismiss)
        .await
        .unwrap();
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    assert!(
        store
            .list_frozen_moves(&current, None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        store
            .resolve_frozen_move(&old, &current, &id, &apply_choice(META_ETAG))
            .await
            .is_err()
    );
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, tree);
    let mut db = database(&path).await;
    let status: String =
        sqlx::query_scalar("SELECT status FROM client_outbox WHERE operation_id=?")
            .bind(id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(status, "sent");
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_move_keeps_later_same_document_content_and_draft_for_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let old = binding();
    fixture(&path, &old).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&old, TARGET, b"base\n", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, TARGET, &edit.edit_session_id, 1, b"first edit\n")
        .await
        .unwrap();
    let first = store.queue_checkpoint(&old, TARGET, 1).await.unwrap();
    let movement = store
        .queue_move(&old, TARGET.parse().unwrap(), META_ETAG, &move_request())
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, TARGET, &edit.edit_session_id, 2, b"later draft\n")
        .await
        .unwrap();
    let later = store.queue_checkpoint(&old, TARGET, 2).await.unwrap();
    assert_eq!(
        (
            first.operation_seq,
            movement.operation_seq,
            later.operation_seq
        ),
        (1, 2, 3)
    );
    store
        .close_registered_document(&old, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    let mut current = binding();
    current.workspace_id = old.workspace_id.clone();
    current.workspace_epoch = old.workspace_epoch.clone();
    store.activate_registration(&current).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(&path).await;
    sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    for choice in [FrozenMoveChoice::Dismiss, apply_choice(META_ETAG)] {
        assert!(matches!(
            store
                .resolve_frozen_move(&old, &current, &movement.operation_id, &choice)
                .await,
            Err(StoreError::ReconciliationRequired)
        ));
    }
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    let draft = store
        .read_registered_checkpoint(&old, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    let later_saved = store
        .read_outbox_operation(&old, &later.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(later_saved.bytes, b"later draft\n");
    assert_eq!(later_saved.operation_seq, 3);
    store.close().await.unwrap();
}
