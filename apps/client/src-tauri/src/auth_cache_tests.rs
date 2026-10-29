use super::*;
use notryx_client_runtime::Error as NetworkError;
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceSendStatus};
use notryx_server::admin;
use sqlx::Connection;

async fn check_unauthorized_cache(expired: bool) {
    let (directory, server, runtime, session, other_document) =
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
    let (draft_before, outbox_before) = {
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .cache_content(&binding, &node, b"move source\n")
                .await
                .unwrap()
        );
        state.sync_state = Some(WorkspaceSyncState {
            binding: to_contract(binding.clone()),
            status: WorkspaceSyncStatus::Ready,
        });
        (
            state
                .store
                .read_registered_checkpoint(&binding, &other_document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        )
    };
    let request = WorkspaceOpenRequest {
        renderer_session_id: "owner".into(),
        node: node.clone(),
    };
    let WorkspaceOpenStart::Remote(remote) = begin_workspace_open(&runtime, &request, None)
        .await
        .unwrap()
    else {
        panic!("remote content expected");
    };
    if expired {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut db = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("server-state/state.db")),
        )
        .await
        .unwrap();
        let changed =
            sqlx::query("UPDATE devices SET issued_at=?,idle_expires_at=? WHERE device_id=?")
                .bind(now - 2)
                .bind(now - 1)
                .bind(binding.device_id.clone())
                .execute(&mut db)
                .await
                .unwrap();
        assert_eq!(changed.rows_affected(), 1);
        db.close().await.unwrap();
    } else {
        let result = admin::request(
            &directory.path().join("admin.sock"),
            &AdminRequest::RevokeDevice {
                device_id: binding.device_id.clone(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            AdminResponse::Ok {
                result: AdminResult::Done {}
            }
        ));
    }
    let bytes = remote.session.content(&node).await;
    assert_eq!(bytes, Err(NetworkError::Unauthorized));
    let opened = finish_workspace_open(
        &runtime,
        remote,
        binding.clone(),
        node.node_id.to_string(),
        node.content_rev.as_ref().unwrap().as_str().to_owned(),
        bytes,
    )
    .await
    .expect("the verified local cache must remain available after device authentication fails");
    assert_eq!(
        STANDARD.decode(&opened.bytes_base64).unwrap(),
        b"move source\n"
    );
    assert!(!opened.has_session);
    let status = sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
        .await
        .unwrap();
    assert_eq!(status.status, WorkspaceSyncStatus::AuthRequired);
    let outcome = send::send_one(runtime.0.clone(), "owner").await.unwrap();
    assert_eq!(outcome.status, WorkspaceSendStatus::AuthRequired);
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &other_document)
            .await
            .unwrap(),
        draft_before
    );
    assert_eq!(
        state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        outbox_before
    );
    assert_eq!(
        std::fs::read(directory.path().join("notes/example.md")).unwrap(),
        b"server source\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("notes/move.md")).unwrap(),
        b"move source\n"
    );
    drop(state);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn auth_cache_revoked_device_keeps_verified_cache_and_blocks_send() {
    check_unauthorized_cache(false).await;
}

#[tokio::test]
async fn auth_cache_expired_device_keeps_verified_cache_and_blocks_send() {
    check_unauthorized_cache(true).await;
}

#[tokio::test]
async fn auth_cache_first_unauthorized_inventory_uses_local_index_and_blocks_send() {
    let (directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let binding = registration_binding(session.binding());
    {
        let mut state = runtime.0.lock().await;
        state.sync_state = Some(WorkspaceSyncState {
            binding: to_contract(binding.clone()),
            status: WorkspaceSyncStatus::Ready,
        });
    }
    let result = admin::request(
        &directory.path().join("admin.sock"),
        &AdminRequest::RevokeDevice {
            device_id: binding.device_id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        AdminResponse::Ok {
            result: AdminResult::Done {}
        }
    ));
    let page = inventory_for_renderer(&runtime, "owner", None)
        .await
        .expect("the published local index must remain available after authorization fails");
    assert!(page.nodes.iter().any(|node| node.name == "move.md"));
    assert_eq!(
        sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
            .await
            .unwrap()
            .status,
        WorkspaceSyncStatus::AuthRequired
    );
    assert_eq!(
        send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::AuthRequired
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn auth_cache_late_response_is_rejected_and_protocol_block_wins() {
    let (_directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
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
    {
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .cache_content(&binding, &node, b"move source\n")
                .await
                .unwrap()
        );
    }
    let request = WorkspaceOpenRequest {
        renderer_session_id: "owner".into(),
        node: node.clone(),
    };
    let WorkspaceOpenStart::Remote(remote) = begin_workspace_open(&runtime, &request, None)
        .await
        .unwrap()
    else {
        panic!("remote content expected");
    };
    {
        let mut state = runtime.0.lock().await;
        state
            .transmission_gate
            .block(&binding, send::TransmissionBlock::AuthRequired);
        assert!(
            !state
                .transmission_gate
                .response_trusted(&binding, remote.generation)
        );
    }
    let forbidden = finish_workspace_open(
        &runtime,
        remote.clone(),
        binding.clone(),
        node.node_id.to_string(),
        node.content_rev.as_ref().unwrap().as_str().into(),
        Err(NetworkError::Forbidden),
    )
    .await;
    assert!(matches!(forbidden, Err(error) if error == "Forbidden"));
    let opened = finish_workspace_open(
        &runtime,
        remote.clone(),
        binding.clone(),
        node.node_id.to_string(),
        node.content_rev.as_ref().unwrap().as_str().into(),
        Ok(b"stale network response".to_vec()),
    )
    .await
    .unwrap();
    assert_eq!(
        STANDARD.decode(&opened.bytes_base64).unwrap(),
        b"move source\n"
    );
    assert!(!opened.has_session);
    {
        let mut state = runtime.0.lock().await;
        state
            .transmission_gate
            .block(&binding, send::TransmissionBlock::ProtocolMismatch);
        assert_eq!(
            state.transmission_gate.blocked_status(&binding),
            WorkspaceSendStatus::NeedsReview
        );
    }
    let result = finish_workspace_open(
        &runtime,
        remote,
        binding.clone(),
        node.node_id.to_string(),
        node.content_rev.as_ref().unwrap().as_str().into(),
        Err(NetworkError::Unauthorized),
    )
    .await;
    assert!(matches!(result, Err(error) if error == "stale_sync"));
    server.shutdown().await.unwrap();
}
