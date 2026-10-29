use super::*;
use notryx_protocol::NodeState;
use tempfile::TempDir;

async fn restart_offline() -> (TempDir, Runtime, RegistrationBinding, NodeState, String) {
    let (directory, server, runtime, session, draft_document) =
        sync_bootstrap::tests::setup().await;
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
    let binding = registration_binding(session.binding());
    assert!(
        runtime
            .0
            .lock()
            .await
            .store
            .cache_content(&binding, &node, b"move source\n")
            .await
            .unwrap()
    );
    server.shutdown().await.unwrap();
    let state = Arc::try_unwrap(runtime.0).ok().unwrap().into_inner();
    state.store.close().await.unwrap();
    drop(session);
    let store = ClientStore::open(directory.path().join("client.sqlite"))
        .await
        .unwrap();
    let runtime = Runtime(Arc::new(Mutex::new(RuntimeState::new(
        store,
        "owner".into(),
    ))));
    (directory, runtime, binding, node, draft_document)
}

#[tokio::test]
async fn offline_restart_exposes_current_registration_as_offline() {
    let (_directory, runtime, binding, node, _) = restart_offline().await;
    let status = sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
        .await
        .expect("the saved registration must remain visible without a network session");
    assert_eq!(status.binding, to_contract(binding.clone()));
    assert_eq!(status.status, WorkspaceSyncStatus::Offline);
    let page = inventory_for_renderer(&runtime, "owner", None)
        .await
        .unwrap();
    assert_eq!(page.workspace_epoch.to_string(), binding.workspace_epoch);
    assert!(
        page.nodes
            .iter()
            .any(|current| current.node_id == node.node_id)
    );
    assert!(page.next_after_id.is_none());
    let first = page.nodes.first().unwrap().node_id;
    let tail = inventory_for_renderer(&runtime, "owner", Some(first))
        .await
        .unwrap();
    assert_eq!(tail.nodes, page.nodes[1..]);
    assert!(
        matches!(inventory_for_renderer(&runtime, "old-owner", None).await, Err(error) if error == "stale_renderer_session")
    );
    let state = &mut *runtime.0.lock().await;
    let (_, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
    assert!(nodes.iter().any(|current| current.node_id == node.node_id));
    assert!(state.session.is_none());
}

#[tokio::test]
async fn offline_restart_opens_verified_cache_and_preserves_existing_outbox() {
    let (directory, runtime, binding, node, draft_document) = restart_offline().await;
    let (draft_before, outbox_before) = {
        let mut state = runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&binding, &draft_document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        )
    };
    let request = WorkspaceOpenRequest {
        renderer_session_id: "owner".into(),
        node: node.clone(),
    };
    let WorkspaceOpenStart::Draft(opened) = begin_workspace_open(&runtime, &request, None)
        .await
        .expect("verified cached content must open after an offline restart")
    else {
        panic!("opening cached content must not depend on a network session");
    };
    assert!(!opened.local_draft);
    assert!(!opened.has_session);
    assert_eq!(
        STANDARD.decode(&opened.bytes_base64).unwrap(),
        b"move source\n"
    );
    let mut state = runtime.0.lock().await;
    state
        .store
        .save_registered_checkpoint(
            &binding,
            &node.node_id.to_string(),
            &opened.edit_session_id,
            1,
            b"offline edit\n",
        )
        .await
        .unwrap();
    let draft = state
        .store
        .read_registered_checkpoint(&binding, &node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"offline edit\n");
    assert_eq!(draft.base_bytes, b"move source\n");
    assert_eq!(draft.base_revision, node.content_rev.unwrap().as_str());
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &draft_document)
            .await
            .unwrap(),
        draft_before
    );
    assert_eq!(
        state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        outbox_before
    );
    assert_eq!(
        std::fs::read(directory.path().join("notes/move.md")).unwrap(),
        b"move source\n"
    );
}

#[tokio::test]
async fn offline_restart_opens_existing_draft_without_creating_or_sending_an_operation() {
    let (_directory, runtime, binding, _, document) = restart_offline().await;
    let (node, outbox_before) = {
        let mut state = runtime.0.lock().await;
        let (_, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        let node = nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        (
            node,
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        )
    };
    let request = WorkspaceOpenRequest {
        renderer_session_id: "owner".into(),
        node,
    };
    let WorkspaceOpenStart::Draft(opened) = begin_workspace_open(&runtime, &request, None)
        .await
        .expect("the existing draft must open after an offline restart")
    else {
        panic!("the durable draft must take precedence");
    };
    assert!(opened.local_draft);
    assert!(!opened.has_session);
    assert_eq!(
        STANDARD.decode(&opened.bytes_base64).unwrap(),
        b"local draft"
    );
    assert_eq!(
        runtime
            .0
            .lock()
            .await
            .store
            .list_outbox(&binding, 0, 100)
            .await
            .unwrap(),
        outbox_before
    );
}

#[tokio::test]
async fn offline_restart_rejects_corrupt_cache_without_creating_a_draft() {
    use sqlx::Connection;
    let (directory, runtime, binding, node, document) = restart_offline().await;
    let outbox_before = runtime
        .0
        .lock()
        .await
        .store
        .list_outbox(&binding, 0, 100)
        .await
        .unwrap();
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(directory.path().join("client.sqlite")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE cached_content SET bytes=? WHERE node_id=?")
        .bind(b"corrupt".as_slice())
        .bind(node.node_id.to_string())
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let request = WorkspaceOpenRequest {
        renderer_session_id: "owner".into(),
        node: node.clone(),
    };
    assert!(
        begin_workspace_open(&runtime, &request, None)
            .await
            .is_err()
    );
    let mut state = runtime.0.lock().await;
    assert!(
        state
            .store
            .read_registered_checkpoint(&binding, &node.node_id.to_string())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"local draft"
    );
    assert_eq!(
        state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        outbox_before
    );
    assert_eq!(
        std::fs::read(directory.path().join("notes/move.md")).unwrap(),
        b"move source\n"
    );
}

#[tokio::test]
async fn offline_restart_registration_replacement_keeps_old_draft_and_freezes_outbox() {
    let (_directory, runtime, binding, node, document) = restart_offline().await;
    let mut state = runtime.0.lock().await;
    let previous = state.store.list_outbox(&binding, 0, 100).await.unwrap();
    let replacement = RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..binding.clone()
    };
    state
        .store
        .activate_registration(&replacement)
        .await
        .unwrap();
    let old_outbox = state.store.list_outbox(&binding, 0, 100).await.unwrap();
    assert_eq!(old_outbox.entries.len(), previous.entries.len());
    assert_eq!(
        old_outbox.entries[0].operation_id,
        previous.entries[0].operation_id
    );
    assert!(old_outbox.entries[0].frozen);
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"local draft"
    );
    let recovered = state.open_local_draft(&binding, &document).await.unwrap();
    assert!(recovered.local_draft);
    assert!(!recovered.has_session);
    assert_eq!(
        STANDARD.decode(recovered.bytes_base64).unwrap(),
        b"local draft"
    );
    drop(state);
    let request = WorkspaceOpenRequest {
        renderer_session_id: "owner".into(),
        node,
    };
    assert!(
        matches!(begin_workspace_open(&runtime, &request, None).await, Err(error) if error == "not_connected")
    );
}
