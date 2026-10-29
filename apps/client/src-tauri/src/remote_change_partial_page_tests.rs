use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    remote_change_dirty::{DirtyRemoteState, inspect_dirty_remote_notifying},
    sync_bootstrap::{
        bootstrap_metadata, server_test_support,
        tests::{setup, test_proxy},
    },
};
use notryx_client_runtime::{ContentMutation, Session};
use notryx_client_store::ClientStore;
use notryx_protocol::{OperationReceipt, WorkspaceDirtyRemoteRequest, WorkspaceSyncStatus};
use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[tokio::test]
async fn dirty_partial_catchup_failure_notifies_committed_metadata() {
    let (directory, server, _original_runtime, original, document) = setup().await;
    let inventory = original.inventory(None).await.unwrap();
    let original_node = inventory
        .nodes
        .iter()
        .find(|node| node.node_id.to_string() == document)
        .unwrap();
    let other = inventory
        .nodes
        .iter()
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
    let mode = Arc::new(AtomicU8::new(0));
    let (address, mut observed, release, proxy) = test_proxy(
        server.local_addr(),
        vec!["GET /v1/changes?"],
        false,
        Some(mode.clone()),
    )
    .await;
    let pairing = server_test_support::pairing_request(&config).await.0;
    let session = Arc::new(
        Session::pair(
            &format!("http://{address}"),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let binding = registration_binding(session.binding());
    let store = ClientStore::open(directory.path().join("partial-page-client.sqlite"))
        .await
        .unwrap();
    let runtime = Runtime(Arc::new(Mutex::new(RuntimeState::new(
        store,
        "owner".into(),
    ))));
    let (snapshot, request, before) = {
        let mut state = runtime.0.lock().await;
        state.store.activate_registration(&binding).await.unwrap();
        state.session = Some(session.clone());
        let opened = state
            .store
            .open_registered_document(
                &binding,
                &document,
                b"server source\n",
                original_node.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(
                &binding,
                &document,
                &opened.edit_session_id,
                1,
                b"local draft\n",
            )
            .await
            .unwrap();
        state
            .store
            .queue_checkpoint(&binding, &document, 1)
            .await
            .unwrap();
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
    let initial = bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let first = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: other.node_id,
            expected_revision: other.content_rev.clone().unwrap(),
            bytes: b"first peer revision\n".to_vec(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed {
        content_rev: first_revision,
        ..
    } = first
    else {
        panic!("first peer mutation was not committed")
    };
    let second = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: other.node_id,
            expected_revision: first_revision.clone(),
            bytes: b"second peer revision\n".to_vec(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed {
        content_rev: second_revision,
        ..
    } = second
    else {
        panic!("second peer mutation was not committed")
    };
    let head = peer.server_info().await.unwrap().head_seq;
    assert_eq!(head.get(), initial.get() + 2);
    mode.store(3, Ordering::SeqCst);
    let checking = tokio::spawn({
        let runtime = Runtime(runtime.0.clone());
        let request = request.clone();
        async move {
            let mut events = Vec::new();
            let result = inspect_dirty_remote_notifying(
                &runtime,
                &request,
                1,
                || Ok(()),
                |event| events.push(event.to_owned()),
            )
            .await;
            (result, events)
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), observed.recv())
        .await
        .unwrap()
        .unwrap();
    mode.store(4, Ordering::SeqCst);
    release.send(()).await.unwrap();
    let (result, mut events) = tokio::time::timeout(std::time::Duration::from_secs(15), checking)
        .await
        .unwrap()
        .unwrap();
    let (cursor, revision, status, after) = {
        let mut state = runtime.0.lock().await;
        let (cursor, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        let revision = nodes
            .into_iter()
            .find(|node| node.node_id == other.node_id)
            .unwrap()
            .content_rev;
        let status = state.sync_state.as_ref().map(|sync| sync.status);
        let after = (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        );
        (cursor, revision, status, after)
    };
    assert_eq!(result.unwrap(), DirtyRemoteState::Pending);
    assert_eq!(cursor.get(), initial.get() + 1);
    assert_eq!(revision.as_ref(), Some(&first_revision));
    assert_eq!(status, Some(WorkspaceSyncStatus::Offline));
    assert!(after == before, "checkpoint, base or outbox changed");
    let first_count = events
        .iter()
        .filter(|event| event.as_str() == "workspace-metadata-changed")
        .count();
    mode.store(0, Ordering::SeqCst);
    inspect_dirty_remote_notifying(
        &runtime,
        &request,
        1,
        || Ok(()),
        |event| events.push(event.to_owned()),
    )
    .await
    .unwrap();
    let (final_cursor, final_revision, final_local) = {
        let mut state = runtime.0.lock().await;
        let (cursor, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        let revision = nodes
            .into_iter()
            .find(|node| node.node_id == other.node_id)
            .unwrap()
            .content_rev;
        let local = (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        );
        (cursor, revision, local)
    };
    assert_eq!(final_cursor, head);
    assert_eq!(final_revision.as_ref(), Some(&second_revision));
    assert!(
        final_local == before,
        "checkpoint, base or outbox changed after retry"
    );
    let second_count = events
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
    let final_count = events
        .iter()
        .filter(|event| event.as_str() == "workspace-metadata-changed")
        .count();
    proxy.abort();
    server.shutdown().await.unwrap();
    assert_eq!(
        first_count, 1,
        "committed first page must notify even when the next page fails"
    );
    assert_eq!(
        second_count, 2,
        "retry must notify the later committed page"
    );
    assert_eq!(
        final_count, 2,
        "unchanged metadata must not trigger another notification"
    );
}
