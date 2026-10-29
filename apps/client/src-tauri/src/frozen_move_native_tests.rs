use super::*;
use crate::{registration_binding, sync_bootstrap};
use notryx_client_store::PendingOutboxOperation;
use notryx_protocol::MoveNodeRequest;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

#[tokio::test]
async fn frozen_move_requires_current_renderer_confirmation_and_keeps_original() {
    let (directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let node = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    let root = node.parent_id.unwrap();
    let request = MoveNodeRequest {
        expected_parent_id: root,
        parent_id: root,
        name: "renamed.md".into(),
    };
    let old = registration_binding(session.binding());
    let mut current = old.clone();
    current.device_id = Uuid::new_v4().to_string();
    current.device_stream_id = Uuid::new_v4().to_string();
    let item = {
        let mut state = runtime.0.lock().await;
        let item = state
            .store
            .queue_move(&old, node.node_id, &node.meta_etag, &request)
            .await
            .unwrap();
        state.store.activate_registration(&current).await.unwrap();
        state.session = None;
        item
    };
    let path = directory.path().join("client.sqlite");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
    let original: (String, Vec<u8>, String) = sqlx::query_as("SELECT COALESCE(expected_revision,''),bytes,status FROM client_outbox WHERE operation_id=?").bind(&item.operation_id).fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();

    assert!(
        list_for_renderer(&runtime.0, "wrong-owner", &current, None, 10)
            .await
            .is_err()
    );
    assert!(
        list_for_renderer(&runtime.0, "owner", &old, None, 10)
            .await
            .is_err()
    );
    let page = list_for_renderer(&runtime.0, "owner", &current, None, 10)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation_id, item.operation_id);
    assert_eq!(page.entries[0].request, request);
    assert_eq!(page.entries[0].expected_meta_etag, node.meta_etag);
    assert_eq!(
        page.entries[0].current_node.as_ref().unwrap().meta_etag,
        node.meta_etag
    );
    assert_eq!(
        page.entries[0]
            .current_destination
            .as_ref()
            .unwrap()
            .node_id,
        root
    );
    let choice = WorkspaceFrozenMoveChoice::Apply {
        expected_meta_etag: node.meta_etag.clone(),
        request: request.clone(),
    };
    assert!(
        decide_for_renderer(
            &runtime.0,
            "wrong-owner",
            &current,
            &old,
            &item.operation_id,
            &choice
        )
        .await
        .is_err()
    );
    let stale = WorkspaceFrozenMoveChoice::Apply {
        expected_meta_etag: "\"stale\"".into(),
        request: request.clone(),
    };
    assert!(
        decide_for_renderer(
            &runtime.0,
            "owner",
            &current,
            &old,
            &item.operation_id,
            &stale
        )
        .await
        .is_err()
    );
    assert!(
        runtime
            .0
            .lock()
            .await
            .store
            .next_pending_outbox(&current)
            .await
            .unwrap()
            .is_none()
    );
    decide_for_renderer(
        &runtime.0,
        "owner",
        &current,
        &old,
        &item.operation_id,
        &choice,
    )
    .await
    .unwrap();
    decide_for_renderer(
        &runtime.0,
        "owner",
        &current,
        &old,
        &item.operation_id,
        &choice,
    )
    .await
    .unwrap();
    let mut state = runtime.0.lock().await;
    let Some(PendingOutboxOperation::Move(new)) =
        state.store.next_pending_outbox(&current).await.unwrap()
    else {
        panic!("expected a new move")
    };
    assert_ne!(new.operation_id, item.operation_id);
    assert_eq!(new.request, request);
    assert_eq!(new.expected_meta_etag, node.meta_etag);
    drop(state);
    assert!(
        list_for_renderer(&runtime.0, "owner", &current, None, 10)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let after: (String, Vec<u8>, String) = sqlx::query_as("SELECT COALESCE(expected_revision,''),bytes,status FROM client_outbox WHERE operation_id=?").bind(&item.operation_id).fetch_one(&mut db).await.unwrap();
    assert_eq!(after, original);
    let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM move_reconciliations")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(decisions, 1);
    db.close().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_move_rejects_decision_after_registration_replacement() {
    let (directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let node = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    let root = node.parent_id.unwrap();
    let request = MoveNodeRequest {
        expected_parent_id: root,
        parent_id: root,
        name: "renamed.md".into(),
    };
    let old = registration_binding(session.binding());
    let mut current = old.clone();
    current.device_id = Uuid::new_v4().to_string();
    current.device_stream_id = Uuid::new_v4().to_string();
    let item = {
        let mut state = runtime.0.lock().await;
        let item = state
            .store
            .queue_move(&old, node.node_id, &node.meta_etag, &request)
            .await
            .unwrap();
        state.store.activate_registration(&current).await.unwrap();
        state.session = None;
        item
    };
    let path = directory.path().join("client.sqlite");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert_eq!(
        list_for_renderer(&runtime.0, "owner", &current, None, 10)
            .await
            .unwrap()
            .entries[0]
            .operation_id,
        item.operation_id
    );
    let mut replacement = current.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    runtime
        .0
        .lock()
        .await
        .store
        .activate_registration(&replacement)
        .await
        .unwrap();
    assert!(
        decide_for_renderer(
            &runtime.0,
            "owner",
            &current,
            &old,
            &item.operation_id,
            &WorkspaceFrozenMoveChoice::Dismiss
        )
        .await
        .is_err()
    );
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM move_reconciliations")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(decisions, 0);
    let original: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM client_outbox WHERE operation_id=? AND registration_id=1",
    )
    .bind(&item.operation_id)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(original, 1);
    db.close().await.unwrap();
    server.shutdown().await.unwrap();
}
