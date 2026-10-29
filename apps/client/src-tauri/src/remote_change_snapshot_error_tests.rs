use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    remote_change_dirty::inspect_dirty_remote_notifying,
    sync_bootstrap::{
        bootstrap_metadata, server_test_support,
        tests::{setup, test_proxy},
    },
};
use notryx_client_runtime::{ContentMutation, Session};
use notryx_client_store::ClientStore;
use notryx_protocol::{OperationReceipt, WorkspaceDirtyRemoteRequest};
use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
use sqlx::Connection;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[tokio::test]
async fn dirty_snapshot_error_still_notifies_committed_metadata() {
    let (directory, server, _original_runtime, original, document) = setup().await;
    let original_node = original
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == document)
        .unwrap();
    let other = original
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
    let (address, mut observed, release, proxy) =
        test_proxy(server.local_addr(), vec!["GET /v1/changes?"], false, None).await;
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
    let database = directory.path().join("snapshot-error-client.sqlite");
    let store = ClientStore::open(&database).await.unwrap();
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
    let receipt = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: other.node_id,
            expected_revision: other.content_rev.unwrap(),
            bytes: b"other peer revision\n".to_vec(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed {
        content_rev: expected,
        ..
    } = receipt
    else {
        panic!("peer mutation was not committed")
    };
    let head = peer.server_info().await.unwrap().head_seq;
    assert!(head > initial);
    let mut checking = tokio::spawn({
        let runtime = crate::Runtime(runtime.0.clone());
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
    let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
        .await
        .unwrap();
    sqlx::query("ALTER TABLE draft_checkpoints RENAME TO injected_checkpoint_error")
        .execute(&mut db)
        .await
        .unwrap();
    let release_result = release.send(()).await;
    let checked =
        match tokio::time::timeout(std::time::Duration::from_secs(10), &mut checking).await {
            Ok(value) => Some(value),
            Err(_) => {
                checking.abort();
                let _ = checking.await;
                None
            }
        };
    let restored = sqlx::query("ALTER TABLE injected_checkpoint_error RENAME TO draft_checkpoints")
        .execute(&mut db)
        .await;
    let closed = db.close().await;
    assert!(
        restored.is_ok(),
        "injected checkpoint schema was not restored"
    );
    assert!(closed.is_ok(), "fault-injection connection did not close");
    assert!(release_result.is_ok(), "test proxy was not released");
    let (result, events) = checked
        .expect("dirty inspection timed out")
        .expect("dirty inspection task failed");
    let (cursor, actual, after) = {
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
    assert!(result.is_err(), "injected SQLite error was not reported");
    assert!(after == before, "checkpoint, base or outbox changed");
    assert_eq!(cursor, head);
    assert_eq!(actual.as_ref(), Some(&expected));
    let count = events
        .iter()
        .filter(|event| event.as_str() == "workspace-metadata-changed")
        .count();
    proxy.abort();
    server.shutdown().await.unwrap();
    assert_eq!(
        count, 1,
        "committed metadata must be notified even when the draft snapshot fails"
    );
}
