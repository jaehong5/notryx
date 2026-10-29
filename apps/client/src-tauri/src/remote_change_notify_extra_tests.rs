use crate::{
    ProductSession, ProductSessionKind, registration_binding,
    remote_change_dirty::inspect_dirty_remote_notifying,
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
};
use notryx_client_runtime::{ContentMutation, Session};
use notryx_protocol::{OperationReceipt, WorkspaceDirtyRemoteRequest};
use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
use uuid::Uuid;

#[tokio::test]
async fn dirty_inspection_notifies_when_it_applies_another_nodes_metadata() {
    let (directory, server, runtime, session, document) = setup().await;
    let binding = registration_binding(session.binding());
    let (snapshot, request, before) = {
        let mut state = runtime.0.lock().await;
        let (opened, active) = state
            .store
            .try_open_registered_draft(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert!(active);
        state
            .gate
            .open_session(opened.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            opened.edit_session_id.clone(),
            ProductSession {
                document_id: document.clone(),
                binding: binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let before = (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        );
        (
            state.gate.snapshot("owner").unwrap(),
            WorkspaceDirtyRemoteRequest {
                renderer_session_id: "owner".into(),
                document_id: document.clone(),
                edit_session_id: opened.edit_session_id,
                generation: "1".into(),
            },
            before,
        )
    };
    let initial_cursor = bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let other = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: server.local_addr(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("server-state"),
            uploads: Default::default(),
        },
    };
    let pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let receipt = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: other.node_id,
            expected_revision: other.content_rev.clone().unwrap(),
            bytes: b"other peer revision\n".to_vec(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed {
        content_rev: expected_revision,
        ..
    } = receipt
    else {
        panic!("peer mutation was not committed")
    };
    let head = peer.server_info().await.unwrap().head_seq;
    assert!(head > initial_cursor);
    let mut events = Vec::new();
    inspect_dirty_remote_notifying(
        &runtime,
        &request,
        1,
        || Ok(()),
        |event| events.push(event.to_owned()),
    )
    .await
    .unwrap();
    let (cursor, actual_revision, after) = {
        let mut state = runtime.0.lock().await;
        let (cursor, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        let actual = nodes
            .into_iter()
            .find(|node| node.node_id == other.node_id)
            .unwrap()
            .content_rev;
        let after = (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        );
        (cursor, actual, after)
    };
    assert!(after == before, "checkpoint, base or outbox changed");
    assert_eq!(cursor, head);
    assert_eq!(actual_revision.as_ref(), Some(&expected_revision));
    let first_metadata_events = events
        .iter()
        .filter(|event| event.as_str() == "workspace-metadata-changed")
        .count();
    inspect_dirty_remote_notifying(
        &runtime,
        &request,
        1,
        || Ok(()),
        |event| events.push(event.to_owned()),
    )
    .await
    .unwrap();
    let total_metadata_events = events
        .iter()
        .filter(|event| event.as_str() == "workspace-metadata-changed")
        .count();
    server.shutdown().await.unwrap();
    assert_eq!(
        first_metadata_events, 1,
        "metadata catch-up did not notify the renderer"
    );
    assert_eq!(
        total_metadata_events, 1,
        "unchanged metadata must not trigger another notification"
    );
}
