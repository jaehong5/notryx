use super::*;
use notryx_protocol::NodeState;
use tempfile::TempDir;

async fn check_http_failure_cache(
    status: &'static str,
    body: &'static str,
    inventory: bool,
    expected_error: Option<&str>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (directory, server, runtime, session, document) = sync_bootstrap::tests::setup().await;
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let binding = registration_binding(session.binding());
    let expected = session.inventory(None).await.unwrap();
    let node = expected
        .nodes
        .iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    let (draft_before, outbox_before) = {
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .cache_content(&binding, node, b"move source\n")
                .await
                .unwrap()
        );
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap();
        let outbox = state.store.list_outbox(&binding, 0, 100).await.unwrap();
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state.remote_refresh_candidates.clear();
        search_native::replace_renderer(&mut state, "reloaded-owner".into());
        (draft, outbox)
    };
    let address = server.local_addr();
    server.shutdown().await.unwrap();
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();
    let response = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() <= 16 * 1024);
        }
        assert!(request.starts_with(b"GET /v1/nodes"));
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    if inventory {
        let result = inventory_for_renderer(&runtime, "reloaded-owner", None).await;
        if let Some(error) = expected_error {
            assert_eq!(result.unwrap_err(), error);
        } else {
            let page = result.expect("temporary HTTP failures must preserve cached inventory");
            assert_eq!(page.workspace_epoch, expected.workspace_epoch);
            assert_eq!(page.nodes, expected.nodes);
            assert_eq!(page.next_after_id, expected.next_after_id);
        }
    } else {
        let request = WorkspaceOpenRequest {
            renderer_session_id: "reloaded-owner".into(),
            node: node.clone(),
        };
        let WorkspaceOpenStart::Remote(remote) = begin_workspace_open(&runtime, &request, None)
            .await
            .unwrap()
        else {
            panic!("the cached unedited document must attempt its conditional HTTP read");
        };
        let bytes = remote.session.content(node).await;
        let result = finish_workspace_open(
            &runtime,
            remote,
            binding.clone(),
            node.node_id.to_string(),
            node.content_rev.as_ref().unwrap().as_str().to_owned(),
            bytes,
        )
        .await;
        if let Some(error) = expected_error {
            assert_eq!(result.unwrap_err(), error);
        } else {
            let opened = result.expect("temporary HTTP failures must open verified cached content");
            assert_eq!(
                STANDARD.decode(&opened.bytes_base64).unwrap(),
                b"move source\n"
            );
            assert!(!opened.local_draft);
            assert!(!opened.readonly);
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), response)
        .await
        .expect("the HTTP fault must have been served")
        .unwrap();
    let mut state = runtime.0.lock().await;
    assert!(Arc::ptr_eq(state.session.as_ref().unwrap(), &session));
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap(),
        draft_before
    );
    assert_eq!(
        state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        outbox_before
    );
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
            .read_cached_content(&binding, node.node_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"move source\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("notes/move.md")).unwrap(),
        b"move source\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("notes/example.md")).unwrap(),
        b"server source\n"
    );
}

#[tokio::test]
async fn http_busy_reload_uses_cached_inventory() {
    check_http_failure_cache("429 Too Many Requests", "{}", true, None).await;
}

#[tokio::test]
async fn http_unavailable_reload_uses_cached_inventory() {
    check_http_failure_cache("503 Service Unavailable", "{}", true, None).await;
}

#[tokio::test]
async fn http_busy_opens_verified_cached_content() {
    check_http_failure_cache("429 Too Many Requests", "{}", false, None).await;
}

#[tokio::test]
async fn http_unavailable_opens_verified_cached_content() {
    check_http_failure_cache("503 Service Unavailable", "{}", false, None).await;
}

#[tokio::test]
async fn http_forbidden_does_not_fall_back_to_cached_inventory_or_content() {
    for inventory in [true, false] {
        check_http_failure_cache(
            "403 Forbidden",
            r#"{"code":"forbidden","message":"synthetic","request_id":"01900000-0000-7000-8000-000000000001"}"#,
            inventory,
            Some("Forbidden"),
        )
        .await;
    }
}

#[tokio::test]
async fn offline_reload_with_live_session_reads_inventory_without_changing_drafts() {
    let (_directory, server, runtime, session, document) = sync_bootstrap::tests::setup().await;
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let binding = registration_binding(session.binding());
    let expected = session.inventory(None).await.unwrap();
    let (draft_before, outbox_before) = {
        let mut state = runtime.0.lock().await;
        let node = expected
            .nodes
            .iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        assert!(
            state
                .store
                .cache_content(&binding, node, b"move source\n")
                .await
                .unwrap()
        );
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap();
        let outbox = state.store.list_outbox(&binding, 0, 100).await.unwrap();
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state.remote_refresh_candidates.clear();
        search_native::replace_renderer(&mut state, "reloaded-owner".into());
        (draft, outbox)
    };
    server.shutdown().await.unwrap();

    let page = inventory_for_renderer(&runtime, "reloaded-owner", None)
        .await
        .expect("a renderer reload must keep the cached inventory available offline");
    assert_eq!(page.workspace_epoch, expected.workspace_epoch);
    assert_eq!(page.nodes, expected.nodes);
    assert_eq!(page.next_after_id, expected.next_after_id);
    let tail = inventory_for_renderer(&runtime, "reloaded-owner", Some(page.nodes[0].node_id))
        .await
        .unwrap();
    assert_eq!(tail.nodes, expected.nodes[1..]);
    assert!(tail.next_after_id.is_none());
    assert_eq!(
        inventory_for_renderer(&runtime, "owner", None)
            .await
            .unwrap_err(),
        "stale_renderer_session"
    );
    let mut state = runtime.0.lock().await;
    assert!(Arc::ptr_eq(state.session.as_ref().unwrap(), &session));
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap(),
        draft_before
    );
    assert_eq!(
        state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        outbox_before
    );
    let cached_node = expected
        .nodes
        .iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    assert_eq!(
        state
            .store
            .read_cached_content(&binding, cached_node.node_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"move source\n"
    );
}

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
