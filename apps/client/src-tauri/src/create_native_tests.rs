use super::*;
use crate::create_native::{
    create_parents_for_renderer, create_status_for_renderer, queue_create_for_renderer,
};
use notryx_client_store::OutboxStatus;
use notryx_protocol::NodeState;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::TempDir;

const OWNER: &str = "owner";
const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FOLDER: &str = "019a1000-0000-7000-8000-000000000002";
const FILE: &str = "019a1000-0000-7000-8000-000000000003";
const DELETED: &str = "019a1000-0000-7000-8000-000000000004";
const MISSING: &str = "019a1000-0000-7000-8000-000000000005";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn node(id: &str, parent: Option<&str>, name: &str, kind: &str, deleted: bool) -> NodeState {
    serde_json::from_value(serde_json::json!({
        "node_id":id,"parent_id":parent,"name":name,"kind":kind,"deleted":deleted,
        "content_rev":if kind=="file" {Some("content")} else {None},
        "content_etag":if kind=="file" {Some("\"content\"")} else {None},
        "meta_rev":"meta","meta_etag":"\"meta\"","node_etag":"\"node\"",
        "content_hash":if kind=="file" {Some(format!("sha256:{}", "a".repeat(64)))} else {None},
        "size":if kind=="file" {Some("0")} else {None},
        "children_generation":"0","last_change_seq":"7"
    }))
    .unwrap()
}
async fn fixture() -> (TempDir, Arc<Mutex<RuntimeState>>, RegistrationBinding) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    let binding = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'7')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for node in [
        node(ROOT, None, "", "folder", false),
        node(FOLDER, Some(ROOT), "folder", "folder", false),
        node(FILE, Some(ROOT), "file.md", "file", false),
        node(DELETED, Some(ROOT), "deleted", "folder", true),
    ] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let store = ClientStore::open(&path).await.unwrap();
    let runtime = Arc::new(Mutex::new(RuntimeState::new(store, OWNER.into())));
    assert!(runtime.lock().await.session.is_none());
    (dir, runtime, binding)
}

#[tokio::test]
async fn offline_create_parent_page_uses_the_same_live_local_tree_as_queue() {
    let (_dir, runtime, binding) = fixture().await;
    let page = create_parents_for_renderer(&runtime, OWNER, &binding, None)
        .await
        .unwrap();
    assert_eq!(page.workspace_epoch.to_string(), binding.workspace_epoch);
    assert_eq!(
        page.nodes
            .iter()
            .map(|node| node.node_id.to_string())
            .collect::<Vec<_>>(),
        vec![ROOT, FOLDER]
    );
    assert!(page.next_after_id.is_none());
    let tail = create_parents_for_renderer(&runtime, OWNER, &binding, Some(ROOT.parse().unwrap()))
        .await
        .unwrap();
    assert_eq!(tail.nodes, page.nodes[1..]);
    let operation = queue_create_for_renderer(
        &runtime,
        OWNER,
        &binding,
        FOLDER.parse().unwrap(),
        "offline folder",
    )
    .await
    .unwrap();
    assert_eq!(operation.request.parent_id.to_string(), FOLDER);
    assert_eq!(operation.status, OutboxStatus::Queued);
    let after = create_parents_for_renderer(&runtime, OWNER, &binding, None)
        .await
        .unwrap();
    assert_eq!(after.nodes, page.nodes);
    assert!(
        !after
            .nodes
            .iter()
            .any(|node| node.node_id == operation.request.node_id)
    );
}

#[tokio::test]
async fn offline_create_queue_and_status_survive_reopening_sqlite() {
    let (dir, runtime, binding) = fixture().await;
    let first = queue_create_for_renderer(&runtime, OWNER, &binding, ROOT.parse().unwrap(), "one")
        .await
        .unwrap();
    let second =
        queue_create_for_renderer(&runtime, OWNER, &binding, FOLDER.parse().unwrap(), "two")
            .await
            .unwrap();
    assert_eq!((first.operation_seq, second.operation_seq), (1, 2));
    assert_ne!(first.request.node_id, second.request.node_id);
    let status = create_status_for_renderer(&runtime, OWNER, &binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(status.entries.len(), 2);
    assert_eq!(status.entries[0].operation, first);
    assert_eq!(status.entries[1].operation, second);
    assert!(
        status
            .entries
            .iter()
            .all(|entry| entry.receipt.is_none() && !entry.frozen)
    );
    let state = Arc::try_unwrap(runtime).ok().unwrap().into_inner();
    state.store.close().await.unwrap();
    let store = ClientStore::open(dir.path().join("client.sqlite"))
        .await
        .unwrap();
    let runtime = Arc::new(Mutex::new(RuntimeState::new(store, OWNER.into())));
    assert!(runtime.lock().await.session.is_none());
    let restored = create_status_for_renderer(&runtime, OWNER, &binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(restored, status);
    let page = create_parents_for_renderer(&runtime, OWNER, &binding, None)
        .await
        .unwrap();
    assert!(
        page.nodes
            .iter()
            .any(|node| node.node_id.to_string() == FOLDER)
    );
    assert!(!page.nodes.iter().any(
        |node| node.node_id == first.request.node_id || node.node_id == second.request.node_id
    ));
}

#[tokio::test]
async fn offline_create_rejects_stale_binding_owner_and_invalid_parent_without_enqueuing() {
    let (_dir, runtime, binding) = fixture().await;
    let wrong = RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..binding.clone()
    };
    assert!(
        create_parents_for_renderer(&runtime, OWNER, &wrong, None)
            .await
            .is_err()
    );
    assert!(
        create_status_for_renderer(&runtime, OWNER, &wrong, 0, 100)
            .await
            .is_err()
    );
    assert!(
        queue_create_for_renderer(
            &runtime,
            OWNER,
            &wrong,
            ROOT.parse().unwrap(),
            "wrong binding"
        )
        .await
        .is_err()
    );
    assert!(
        create_parents_for_renderer(&runtime, "old-owner", &binding, None)
            .await
            .is_err()
    );
    assert!(
        create_status_for_renderer(&runtime, "old-owner", &binding, 0, 100)
            .await
            .is_err()
    );
    assert!(
        queue_create_for_renderer(
            &runtime,
            "old-owner",
            &binding,
            ROOT.parse().unwrap(),
            "wrong owner"
        )
        .await
        .is_err()
    );
    for parent in [FILE, DELETED, MISSING] {
        assert!(
            queue_create_for_renderer(
                &runtime,
                OWNER,
                &binding,
                parent.parse().unwrap(),
                "invalid parent"
            )
            .await
            .is_err()
        );
    }
    let status = create_status_for_renderer(&runtime, OWNER, &binding, 0, 100)
        .await
        .unwrap();
    assert!(status.entries.is_empty());
    let page = create_parents_for_renderer(&runtime, OWNER, &binding, None)
        .await
        .unwrap();
    assert!(
        !page
            .nodes
            .iter()
            .any(|node| node.node_id.to_string() == DELETED)
    );
    assert!(runtime.lock().await.session.is_none());
}

#[tokio::test]
async fn selected_parent_deleted_after_listing_is_rechecked_before_queue() {
    let (dir, runtime, binding) = fixture().await;
    let before = create_parents_for_renderer(&runtime, OWNER, &binding, None)
        .await
        .unwrap();
    assert!(
        before
            .nodes
            .iter()
            .any(|node| node.node_id.to_string() == FOLDER)
    );
    let deleted = node(FOLDER, Some(ROOT), "folder", "folder", true);
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(dir.path().join("client.sqlite"))
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE remote_nodes SET state=? WHERE registration_id=1 AND node_id=?")
        .bind(serde_json::to_string(&deleted).unwrap())
        .bind(FOLDER)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(
        queue_create_for_renderer(&runtime, OWNER, &binding, FOLDER.parse().unwrap(), "late")
            .await
            .is_err()
    );
    assert!(
        create_status_for_renderer(&runtime, OWNER, &binding, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let after = create_parents_for_renderer(&runtime, OWNER, &binding, None)
        .await
        .unwrap();
    assert!(
        !after
            .nodes
            .iter()
            .any(|node| node.node_id.to_string() == FOLDER)
    );
}

// Append to apps/client/src-tauri/src/create_native_tests.rs.
// Expose create_native::request_create_abandon_for_renderer as pub(super) for this sibling module.
#[tokio::test]
async fn offline_queued_create_cancel_is_durable_and_retains_fifo_head() {
    use notryx_client_store::{OutboxAbandonState, PendingOutboxOperation};
    let (dir, runtime, binding) = fixture().await;
    let first =
        queue_create_for_renderer(&runtime, OWNER, &binding, ROOT.parse().unwrap(), "first")
            .await
            .unwrap();
    let second =
        queue_create_for_renderer(&runtime, OWNER, &binding, ROOT.parse().unwrap(), "second")
            .await
            .unwrap();
    assert_eq!((first.operation_seq, second.operation_seq), (1, 2));
    assert!(runtime.lock().await.session.is_none());
    assert!(
        crate::create_native::request_create_abandon_for_renderer(
            &runtime,
            OWNER,
            &binding,
            &second.operation_id,
            None
        )
        .await
        .is_err()
    );
    let wrong = RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..binding.clone()
    };
    assert!(
        crate::create_native::request_create_abandon_for_renderer(
            &runtime,
            OWNER,
            &wrong,
            &first.operation_id,
            None
        )
        .await
        .is_err()
    );
    assert!(
        crate::create_native::request_create_abandon_for_renderer(
            &runtime,
            "old-owner",
            &binding,
            &first.operation_id,
            None
        )
        .await
        .is_err()
    );
    crate::create_native::request_create_abandon_for_renderer(
        &runtime,
        OWNER,
        &binding,
        &first.operation_id,
        None,
    )
    .await
    .unwrap();
    let before = create_status_for_renderer(&runtime, OWNER, &binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(before.entries.len(), 2);
    assert_eq!(before.entries[0].operation.operation_id, first.operation_id);
    assert_eq!(before.entries[0].operation.request, first.request);
    assert_eq!(before.entries[0].operation.status, OutboxStatus::Abandoning);
    assert_eq!(
        before.entries[0].operation.abandon_state,
        OutboxAbandonState::Requested
    );
    assert_eq!(before.entries[1].operation, second);
    assert!(before.entries.iter().all(|entry| entry.receipt.is_none()));
    let state = Arc::try_unwrap(runtime).ok().unwrap().into_inner();
    state.store.close().await.unwrap();
    let store = ClientStore::open(dir.path().join("client.sqlite"))
        .await
        .unwrap();
    let runtime = Arc::new(Mutex::new(RuntimeState::new(store, OWNER.into())));
    assert!(runtime.lock().await.session.is_none());
    let after = create_status_for_renderer(&runtime, OWNER, &binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(after, before);
    let mut state = runtime.lock().await;
    let PendingOutboxOperation::Create(head) = state
        .store
        .next_pending_outbox(&binding)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("cancel intent must remain at FIFO head")
    };
    assert_eq!(head.operation_id, first.operation_id);
    assert_eq!(head.operation_seq, 1);
    assert_eq!(head.status, OutboxStatus::Abandoning);
    assert_eq!(head.abandon_state, OutboxAbandonState::Requested);
    assert_eq!(state.store.receipt_ack_through(&binding).await.unwrap(), 0);
}

#[tokio::test]
async fn offline_sent_create_cancel_does_not_guess_missing_server_receipt() {
    use notryx_client_store::PendingOutboxOperation;
    let (_dir, runtime, binding) = fixture().await;
    let first = queue_create_for_renderer(
        &runtime,
        OWNER,
        &binding,
        ROOT.parse().unwrap(),
        "uncertain",
    )
    .await
    .unwrap();
    let sent = {
        let mut state = runtime.lock().await;
        let PendingOutboxOperation::Create(sent) = state
            .store
            .prepare_pending_outbox(&binding)
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected create")
        };
        sent
    };
    assert_eq!(sent.operation_id, first.operation_id);
    assert_eq!(sent.status, OutboxStatus::Sent);
    assert!(
        crate::create_native::request_create_abandon_for_renderer(
            &runtime,
            OWNER,
            &binding,
            &first.operation_id,
            None
        )
        .await
        .is_err()
    );
    let mut state = runtime.lock().await;
    assert_eq!(
        state.store.next_pending_outbox(&binding).await.unwrap(),
        Some(PendingOutboxOperation::Create(sent))
    );
    assert_eq!(state.store.receipt_ack_through(&binding).await.unwrap(), 0);
}

#[tokio::test]
async fn offline_file_create_native_survives_restart_and_does_not_create_fake_remote_document() {
    let (dir, runtime, binding) = fixture().await;
    let operation = crate::create_native::queue_node_for_renderer(
        &runtime,
        OWNER,
        &binding,
        ROOT.parse().unwrap(),
        "empty.md",
        notryx_protocol::NodeKind::File,
    )
    .await
    .unwrap();
    assert_eq!(operation.request.kind, notryx_protocol::NodeKind::File);
    assert_eq!(operation.request.upload_id, None);
    assert_eq!(operation.status, OutboxStatus::Queued);
    let node_id = operation.request.node_id;
    drop(runtime);
    let mut reopened = ClientStore::open(&dir.path().join("client.sqlite"))
        .await
        .unwrap();
    let page = reopened
        .list_create_operations(&binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].operation.request.kind,
        notryx_protocol::NodeKind::File
    );
    reopened.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(dir.path().join("client.sqlite")),
    )
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM remote_nodes WHERE node_id = ?")
        .bind(node_id.to_string())
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn file_create_without_markdown_extension_does_not_consume_fifo_sequence() {
    let (_dir, runtime, binding) = fixture().await;
    let result = crate::create_native::queue_node_for_renderer(
        &runtime,
        OWNER,
        &binding,
        ROOT.parse().unwrap(),
        "no extension",
        notryx_protocol::NodeKind::File,
    )
    .await;
    assert!(result.is_err());
    let operation = crate::create_native::queue_node_for_renderer(
        &runtime,
        OWNER,
        &binding,
        ROOT.parse().unwrap(),
        "UPPER.MD",
        notryx_protocol::NodeKind::File,
    )
    .await
    .unwrap();
    assert_eq!(operation.operation_seq, 1);
    assert_eq!(operation.request.name, "UPPER.MD");
}
