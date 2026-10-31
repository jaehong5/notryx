use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState,
    conflict_snapshot_native::prepare_conflict_snapshot,
    registration_binding,
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
};
use notryx_client_runtime::{ContentMutation, DeleteMutation, Session};
use notryx_client_store::{ClientStore, OutboxConflict, OutboxOperation, RegistrationBinding};
use notryx_protocol::{NodeState, OperationReceipt};
use notryx_server::{
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
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
const BASE: &[u8] = b"server source\n";
const LOCAL: &[u8] = b"local draft\n";
const REMOTE: &[u8] = b"peer first\n";

async fn content_proxy(
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
    let armed = Arc::new(AtomicBool::new(false));
    let captured = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task = tokio::spawn({
        let (armed, captured, release) = (armed.clone(), captured.clone(), release.clone());
        async move {
            loop {
                let (mut incoming, _) = listener.accept().await.unwrap();
                let (armed, captured, release) = (armed.clone(), captured.clone(), release.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let end = loop {
                        let size = incoming.read(&mut buffer).await.unwrap();
                        if size == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..size]);
                        if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n")
                        {
                            break index;
                        }
                        assert!(request.len() < 65536);
                    };
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let gate = headers.starts_with("GET /v1/nodes/")
                        && headers.lines().next().unwrap().contains("/content ")
                        && armed.swap(false, Ordering::SeqCst);
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
                    if gate {
                        let mut response = Vec::new();
                        server.read_to_end(&mut response).await.unwrap();
                        captured.notify_one();
                        release.notified().await;
                        let _ = incoming.write_all(&response).await;
                    } else {
                        let _ = tokio::io::copy_bidirectional(&mut incoming, &mut server).await;
                    }
                });
            }
        }
    });
    (address, armed, captured, release, task)
}

struct Rig {
    _directory: tempfile::TempDir,
    server: RunningServer,
    runtime: Runtime,
    session: Arc<Session>,
    peer: Session,
    binding: RegistrationBinding,
    document: String,
    edit_session: String,
    initial: NodeState,
    first_peer: NodeState,
    operation: OutboxOperation,
    armed: Arc<AtomicBool>,
    captured: Arc<Notify>,
    release: Arc<Notify>,
    proxy: tokio::task::JoinHandle<()>,
    config: Config,
}

async fn rig() -> Rig {
    rig_with_followup(false).await
}

async fn rig_with_followup(followup: bool) -> Rig {
    let (directory, server, _unused_runtime, original, document) = setup().await;
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
    let initial = original
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == document)
        .unwrap();
    let (address, armed, captured, release, proxy) = content_proxy(server.local_addr()).await;
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
    let pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let binding = registration_binding(session.binding());
    let mut store = ClientStore::open(directory.path().join("conflict-snapshot.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&binding).await.unwrap();
    let opened = store
        .open_registered_document(
            &binding,
            &document,
            BASE,
            initial.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 1, LOCAL)
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
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let peer_result = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: initial.node_id,
            expected_revision: initial.content_rev.clone().unwrap(),
            bytes: REMOTE.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(peer_result, OperationReceipt::Committed { .. }));
    let operation = runtime
        .0
        .lock()
        .await
        .store
        .prepare_outbox(&binding)
        .await
        .unwrap()
        .unwrap();
    if followup {
        let mut state = runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &binding,
                &document,
                &opened.edit_session_id,
                2,
                b"newer local\n",
            )
            .await
            .unwrap();
        state
            .store
            .queue_checkpoint(&binding, &document, 2)
            .await
            .unwrap();
    }
    let receipt = session
        .put_content(&ContentMutation {
            binding: session.binding().clone(),
            operation_id: operation.operation_id.parse().unwrap(),
            operation_seq: operation.operation_seq,
            node_id: initial.node_id,
            expected_revision: notryx_domain::ContentRevision::new(
                operation.expected_revision.clone().unwrap(),
            )
            .unwrap(),
            bytes: operation.bytes.clone(),
        })
        .await
        .unwrap();
    let OperationReceipt::PreconditionFailed {
        current_content_rev,
        current_content_etag,
        ..
    } = receipt
    else {
        panic!("expected 412")
    };
    runtime
        .0
        .lock()
        .await
        .store
        .apply_outbox_conflict(
            &binding,
            &OutboxConflict {
                operation_id: operation.operation_id.clone(),
                operation_seq: operation.operation_seq,
                workspace_epoch: binding.workspace_epoch.clone(),
                current_revision: current_content_rev.as_str().into(),
                current_etag: current_content_etag,
            },
        )
        .await
        .unwrap();
    let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    crate::sync_catchup::catch_up_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let first_peer = runtime
        .0
        .lock()
        .await
        .store
        .current_remote_file(&binding, initial.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first_peer.content_rev, initial.content_rev);
    Rig {
        _directory: directory,
        server,
        runtime,
        session,
        peer,
        binding,
        document,
        edit_session: opened.edit_session_id,
        initial,
        first_peer,
        operation,
        armed,
        captured,
        release,
        proxy,
        config,
    }
}

async fn assert_preserved(rig: &Rig) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        draft.base_revision,
        rig.initial.content_rev.as_ref().unwrap().as_str()
    );
    let page = state.store.list_outbox(&rig.binding, 0, 100).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation_id, rig.operation.operation_id);
    assert_eq!(
        page.entries[0].status,
        notryx_client_store::OutboxStatus::Conflict
    );
    let saved = state
        .store
        .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bytes, rig.operation.bytes);
    assert_eq!(saved.payload_hash, rig.operation.payload_hash);
    assert_eq!(saved.expected_revision, rig.operation.expected_revision);
    assert_eq!(saved.operation_seq, rig.operation.operation_seq);
}

async fn prepare(rig: &Rig) -> Result<crate::conflict_snapshot_native::ConflictSnapshot, String> {
    prepare_conflict_snapshot(
        &rig.runtime,
        OWNER,
        &rig.document,
        &rig.edit_session,
        1,
        &rig.operation.operation_id,
    )
    .await
}

#[tokio::test]
async fn conflict_snapshot_reads_real_412_base_local_and_latest_peer_bytes_without_mutation() {
    let rig = rig().await;
    let result = prepare(&rig).await.unwrap();
    assert_eq!(result.binding, rig.binding);
    assert_eq!(result.node_id, rig.initial.node_id);
    assert_eq!(result.operation_id, rig.operation.operation_id);
    assert_eq!(result.generation, 1);
    assert_eq!(
        result.base_revision,
        rig.initial.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(result.base_bytes, BASE);
    assert_eq!(result.local_bytes, LOCAL);
    assert_eq!(
        result.remote_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(result.remote_bytes, REMOTE);
    assert!(
        prepare_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.document,
            &rig.edit_session,
            1,
            &Uuid::now_v7().to_string()
        )
        .await
        .is_err()
    );
    assert!(
        prepare_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.document,
            &rig.edit_session,
            2,
            &rig.operation.operation_id
        )
        .await
        .is_err()
    );
    assert_preserved(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_rejects_old_response_after_second_remote_write_or_delete() {
    for delete in [false, true] {
        let rig = rig().await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = tokio::spawn({
            let (runtime, document, edit, operation) = (
                Runtime(rig.runtime.0.clone()),
                rig.document.clone(),
                rig.edit_session.clone(),
                rig.operation.operation_id.clone(),
            );
            async move {
                prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
            .await
            .unwrap();
        if delete {
            let result = rig
                .peer
                .delete(&DeleteMutation {
                    binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                    operation_seq: 2,
                    node_id: rig.initial.node_id,
                    expected_node_etag: rig.first_peer.node_etag.clone(),
                })
                .await
                .unwrap();
            assert!(matches!(result, OperationReceipt::Deleted { .. }));
        } else {
            let result = rig
                .peer
                .put_content(&ContentMutation {
                    binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                    operation_seq: 2,
                    node_id: rig.initial.node_id,
                    expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                    bytes: b"peer second\n".to_vec(),
                })
                .await
                .unwrap();
            assert!(matches!(result, OperationReceipt::Committed { .. }));
        }
        let gate = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session)
            .await
            .unwrap();
        rig.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "an older captured response cannot describe the current document"
        );
        assert_preserved(&rig).await;
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn conflict_snapshot_rejects_delayed_response_after_registration_replacement() {
    let rig = rig().await;
    rig.armed.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let (runtime, document, edit, operation) = (
            Runtime(rig.runtime.0.clone()),
            rig.document.clone(),
            rig.edit_session.clone(),
            rig.operation.operation_id.clone(),
        );
        async move { prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
        .await
        .unwrap();
    let pairing = server_test_support::pairing_request(&rig.config).await.0;
    let replacement = Arc::new(
        Session::pair(
            &format!("http://{}", rig.server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let new_binding = registration_binding(replacement.binding());
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state
            .store
            .activate_registration(&new_binding)
            .await
            .unwrap();
        state.session = Some(replacement);
    }
    rig.release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_preserved(&rig).await;
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .list_outbox(&new_binding, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

async fn write_external_and_observe(rig: &Rig, bytes: &[u8]) -> NodeState {
    let path = rig.config.workspace.notes_root.join("example.md");
    std::fs::write(&path, bytes).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let inventory = rig.peer.inventory(None).await.unwrap();
            if let Some(node) = inventory.nodes.into_iter().find(|node| {
                node.node_id == rig.initial.node_id
                    && node.content_rev != rig.first_peer.content_rev
                    && node.size.as_deref() == Some(bytes.len().to_string().as_str())
            }) {
                return node;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("watcher did not publish the external synthetic document")
}

async fn prepare_after_external_change(
    rig: &Rig,
) -> Result<crate::conflict_snapshot_native::ConflictSnapshot, String> {
    // Inventory can publish the new revision before a concurrent object recheck finishes.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match prepare(rig).await {
                Err(error) if error == "server_not_ready" => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                result => return result,
            }
        }
    })
    .await
    .expect("server did not settle after the external synthetic change")
}

#[tokio::test]
async fn conflict_snapshot_rejects_non_utf8_peer_document_without_changing_data() {
    let rig = rig().await;
    let bytes = b"peer invalid \xff\xfe\n";
    let remote = write_external_and_observe(&rig, bytes).await;
    assert_ne!(remote.content_rev, rig.first_peer.content_rev);
    assert_eq!(
        prepare_after_external_change(&rig).await.err().as_deref(),
        Some("unsupported_content")
    );
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        bytes
    );
    assert_eq!(rig.peer.content(&remote).await.unwrap(), bytes);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_rejects_over_5_mib_peer_document_without_changing_data() {
    let rig = rig().await;
    let bytes = vec![b'x'; notryx_client_store::MAX_DOCUMENT_BYTES + 1];
    let remote = write_external_and_observe(&rig, &bytes).await;
    assert_ne!(remote.content_rev, rig.first_peer.content_rev);
    assert_eq!(
        prepare_after_external_change(&rig).await.err().as_deref(),
        Some("unsupported_content")
    );
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        bytes
    );
    assert_eq!(rig.peer.content(&remote).await.unwrap(), bytes);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_reports_newer_remote_revision_not_old_412_revision() {
    let rig = rig().await;
    let current = b"peer second version\n";
    let receipt = rig
        .peer
        .put_content(&ContentMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: rig.initial.node_id,
            expected_revision: rig.first_peer.content_rev.clone().unwrap(),
            bytes: current.to_vec(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed { content_rev, .. } = receipt else {
        panic!("peer change must commit")
    };
    let snapshot = prepare(&rig).await.unwrap();
    assert_eq!(snapshot.remote_revision, content_rev.as_str());
    assert_eq!(snapshot.remote_bytes, current);
    assert_ne!(
        snapshot.remote_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(snapshot.local_bytes, LOCAL);
    assert_eq!(snapshot.base_bytes, BASE);
    let saved = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .inspect_content_conflict(&rig.binding, &rig.document, &rig.operation.operation_id)
        .await
        .unwrap();
    assert_eq!(
        saved.conflict_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(
        saved.conflict_etag,
        rig.first_peer.content_etag.as_deref().unwrap()
    );
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        current
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflict_snapshot_rejects_old_response_after_newer_local_checkpoint() {
    let rig = rig().await;
    rig.armed.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let (runtime, document, edit, operation) = (
            Runtime(rig.runtime.0.clone()),
            rig.document.clone(),
            rig.edit_session.clone(),
            rig.operation.operation_id.clone(),
        );
        async move { prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
        .await
        .unwrap();
    let latest = b"newer local checkpoint\n";
    rig.runtime
        .0
        .lock()
        .await
        .store
        .save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 2, latest)
        .await
        .unwrap();
    rig.release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, latest);
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        draft.base_revision,
        rig.initial.content_rev.as_ref().unwrap().as_str()
    );
    let operation = state
        .store
        .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.bytes, rig.operation.bytes);
    assert_eq!(operation.payload_hash, rig.operation.payload_hash);
    assert_eq!(
        operation.status,
        notryx_client_store::OutboxStatus::Conflict
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

async fn cache_observation(rig: &Rig) -> (i64, i64) {
    use sqlx::Connection;
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true),
    )
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cached_content WHERE registration_id=1 AND node_id=?",
    )
    .bind(&rig.document)
    .fetch_one(&mut db)
    .await
    .unwrap();
    let ready: i64 = sqlx::query_scalar(
        "SELECT content_ready FROM remote_nodes WHERE registration_id=1 AND node_id=?",
    )
    .bind(&rig.document)
    .fetch_one(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    (count, ready)
}

#[tokio::test]
async fn conflict_snapshot_old_renderer_response_cannot_commit_cache_or_ready_state() {
    let rig = rig().await;
    let before = cache_observation(&rig).await;
    assert_eq!(before, (0, 0));
    rig.armed.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let (runtime, document, edit, operation) = (
            Runtime(rig.runtime.0.clone()),
            rig.document.clone(),
            rig.edit_session.clone(),
            rig.operation.operation_id.clone(),
        );
        async move { prepare_conflict_snapshot(&runtime, OWNER, &document, &edit, 1, &operation).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
        .await
        .unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state.remote_refresh_candidates.clear();
        crate::search_native::replace_renderer(&mut state, "new-renderer".into());
    }
    rig.release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(cache_observation(&rig).await, before);
    assert_preserved(&rig).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
