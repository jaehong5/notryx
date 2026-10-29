use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    remote_change_dirty::{DirtyRemoteState, inspect_dirty_remote},
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
};
use notryx_client_runtime::{ContentMutation, Session};
use notryx_client_store::ClientStore;
use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Notify},
};
use uuid::Uuid;

const OWNER: &str = "owner";

async fn gated_proxy(
    target: std::net::SocketAddr,
) -> (
    std::net::SocketAddr,
    Arc<AtomicBool>,
    Arc<Notify>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let active = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task = tokio::spawn({
        let (active, observed, release) = (active.clone(), observed.clone(), release.clone());
        async move {
            loop {
                let (mut incoming, _) = listener.accept().await.unwrap();
                let (active, observed, release) =
                    (active.clone(), observed.clone(), release.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let end = loop {
                        let size = incoming.read(&mut buffer).await.unwrap();
                        assert_ne!(size, 0);
                        request.extend_from_slice(&buffer[..size]);
                        if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n")
                        {
                            break index;
                        }
                        assert!(request.len() < 65536);
                    };
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    if headers.starts_with("GET /v1/server-info ")
                        && active.swap(false, Ordering::SeqCst)
                    {
                        observed.notify_one();
                        release.notified().await;
                    }
                    let mut forwarded = headers
                        .lines()
                        .filter(|line| !line.to_ascii_lowercase().starts_with("connection:"))
                        .map(|line| format!("{line}\r\n"))
                        .collect::<String>()
                        .into_bytes();
                    forwarded.extend_from_slice(b"Connection: close\r\n\r\n");
                    forwarded.extend_from_slice(&request[end + 4..]);
                    let mut server = TcpStream::connect(target).await.unwrap();
                    server.write_all(&forwarded).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut incoming, &mut server).await;
                });
            }
        }
    });
    (address, active, observed, release, task)
}

#[tokio::test]
async fn dirty_notice_resolves_when_another_catchup_applies_the_same_change() {
    let (directory, server, _original_runtime, original, document) = setup().await;
    let original_node = original
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == document)
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
    let (address, active, observed, release, proxy) = gated_proxy(server.local_addr()).await;
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
    let mut store = ClientStore::open(directory.path().join("dirty-race-extra.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&binding).await.unwrap();
    let edit = store
        .open_registered_document(
            &binding,
            &document,
            b"server source\n",
            original_node.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            &document,
            &edit.edit_session_id,
            1,
            b"local draft\n",
        )
        .await
        .unwrap();
    store
        .queue_checkpoint(&binding, &document, 1)
        .await
        .unwrap();
    let mut state = RuntimeState::new(store, OWNER.into());
    state.session = Some(session.clone());
    state
        .gate
        .open_session(edit.edit_session_id.clone())
        .unwrap();
    state.product_sessions.insert(
        edit.edit_session_id.clone(),
        ProductSession {
            document_id: document.clone(),
            binding: binding.clone(),
            kind: ProductSessionKind::Active,
        },
    );
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    let cursor = bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let before = {
        let mut state = runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        )
    };
    let peer_pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        peer_pairing.workspace_id,
        &peer_pairing.pairing_secret,
    )
    .await
    .unwrap();
    let receipt = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: original_node.node_id,
            expected_revision: original_node.content_rev.unwrap(),
            bytes: b"peer change\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(
        receipt,
        notryx_protocol::OperationReceipt::Committed { .. }
    ));
    let head = peer.server_info().await.unwrap().head_seq;
    let page = peer.changes(cursor, Some(head)).await.unwrap();
    active.store(true, Ordering::SeqCst);
    let checking = tokio::spawn({
        let (runtime, document, edit_session) = (
            Runtime(runtime.0.clone()),
            document.clone(),
            edit.edit_session_id.clone(),
        );
        async move { inspect_dirty_remote(&runtime, OWNER, &document, &edit_session, 1).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), observed.notified())
        .await
        .unwrap();
    runtime
        .0
        .lock()
        .await
        .store
        .apply_changes(&binding, &page, head)
        .await
        .unwrap();
    release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), checking)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let after = {
        let mut state = runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        )
    };
    assert!(after == before, "checkpoint, base or outbox changed");
    proxy.abort();
    server.shutdown().await.unwrap();
    assert_eq!(
        result,
        DirtyRemoteState::Changed,
        "concurrent catch-up must not leave a known peer change indefinitely pending"
    );
}
