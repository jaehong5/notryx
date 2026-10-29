use crate::{
    Runtime, RuntimeState, frozen_content_native::inspect_frozen_content_for_renderer,
    registration_binding, sync_bootstrap,
};
use notryx_client_runtime::{ContentMutation, Session};
use notryx_client_store::{ClientStore, OutboxOperation, OutboxStatus, RegistrationBinding};
use notryx_protocol::OperationReceipt;
use notryx_server::{
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

const UNIMPLEMENTED: &str = "frozen_content_inspection_unimplemented";
const BASE: &[u8] = b"server source\n";
const DRAFT: &[u8] = b"synthetic unacknowledged edit\n";
const PEER: &[u8] = b"synthetic later remote edit\n";

type MetadataSnapshot = (Option<(String, String)>, Vec<(String, String, i64)>);

struct Rig {
    _directory: tempfile::TempDir,
    server: RunningServer,
    runtime: Runtime,
    old: RegistrationBinding,
    current: RegistrationBinding,
    third: Arc<Session>,
    peer: Session,
    document: String,
    node: notryx_protocol::NodeState,
    operation: OutboxOperation,
    before_draft: notryx_client_store::PersistedDraft,
    before_outbox: notryx_client_store::OutboxPage,
    before_current_metadata: MetadataSnapshot,
    original_file: Vec<u8>,
    mode: Arc<AtomicU8>,
    observed: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    proxy: tokio::task::JoinHandle<()>,
}

async fn rig(pause_content: bool, old_scope: Option<&str>) -> Rig {
    let (directory, server, _unused_runtime, _unused_session, _) =
        sync_bootstrap::tests::setup().await;
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
    let points = if pause_content {
        vec!["GET /v1/nodes/"]
    } else {
        vec![]
    };
    let (address, observed, release, proxy) =
        sync_bootstrap::tests::test_proxy(server.local_addr(), points, false, Some(mode.clone()))
            .await;
    let old_pair = sync_bootstrap::server_test_support::pairing_request(&config)
        .await
        .0;
    let old_session = Arc::new(
        Session::pair(
            &format!("http://{address}"),
            old_pair.workspace_id,
            &old_pair.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let node = old_session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "example.md")
        .unwrap();
    let document = node.node_id.to_string();
    let mut old = registration_binding(old_session.binding());
    match old_scope {
        Some("workspace") => old.workspace_id = Uuid::new_v4().to_string(),
        Some("epoch") => old.workspace_epoch = Uuid::new_v4().to_string(),
        None => {}
        _ => panic!("unexpected synthetic binding scope"),
    }
    let mut store = ClientStore::open(directory.path().join("frozen-content-race.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&old).await.unwrap();
    let edit = store
        .open_registered_document(
            &old,
            &document,
            BASE,
            node.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, &document, &edit.edit_session_id, 1, DRAFT)
        .await
        .unwrap();
    store
        .close_registered_document(&old, &document, &edit.edit_session_id)
        .await
        .unwrap();
    store.queue_checkpoint(&old, &document, 1).await.unwrap();
    let sent = store.prepare_outbox(&old).await.unwrap().unwrap();
    assert_eq!(sent.status, OutboxStatus::Sent);
    let pair = sync_bootstrap::server_test_support::pairing_request(&config)
        .await
        .0;
    let current_session = Arc::new(
        Session::pair(
            &format!("http://{address}"),
            pair.workspace_id,
            &pair.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let current = registration_binding(current_session.binding());
    let pair = sync_bootstrap::server_test_support::pairing_request(&config)
        .await
        .0;
    let third = Arc::new(
        Session::pair(
            &format!("http://{address}"),
            pair.workspace_id,
            &pair.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let pair = sync_bootstrap::server_test_support::pairing_request(&config)
        .await
        .0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        pair.workspace_id,
        &pair.pairing_secret,
    )
    .await
    .unwrap();
    let mut state = RuntimeState::new(store, "owner".into());
    state.session = Some(old_session);
    let pair = state.gate.begin_pair("owner").unwrap();
    state.store.activate_registration(&current).await.unwrap();
    state.gate.commit_pair(&pair).unwrap();
    state.session = Some(current_session.clone());
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &current_session)
        .await
        .unwrap();
    let (operation, before_draft, before_outbox) = {
        let mut state = runtime.0.lock().await;
        let operation = state
            .store
            .read_outbox_operation(&old, &sent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(operation.frozen);
        assert_eq!(operation.status, OutboxStatus::Sent);
        let draft = state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap()
            .unwrap();
        let outbox = state.store.list_outbox(&old, 0, 100).await.unwrap();
        (operation, draft, outbox)
    };
    let original_file = std::fs::read(directory.path().join("notes/example.md")).unwrap();
    let before_current_metadata = stored_metadata(directory.path(), &current).await;
    Rig {
        _directory: directory,
        server,
        runtime,
        old,
        current,
        third,
        peer,
        document,
        node,
        operation,
        before_draft,
        before_outbox,
        before_current_metadata,
        original_file,
        mode,
        observed,
        release,
        proxy,
    }
}

async fn stored_metadata(
    directory: &std::path::Path,
    binding: &RegistrationBinding,
) -> (Option<(String, String)>, Vec<(String, String, i64)>) {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(directory.join("frozen-content-race.sqlite"))
            .read_only(true),
    )
    .await
    .unwrap();
    let registration: i64 = sqlx::query_scalar("SELECT id FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?")
        .bind(&binding.server_origin)
        .bind(&binding.workspace_id)
        .bind(&binding.workspace_epoch)
        .bind(&binding.device_id)
        .bind(&binding.device_stream_id)
        .fetch_one(&mut db)
        .await
        .unwrap();
    let index = sqlx::query_as("SELECT root_id,cursor FROM remote_index WHERE registration_id=?")
        .bind(registration)
        .fetch_optional(&mut db)
        .await
        .unwrap();
    let nodes = sqlx::query_as("SELECT node_id,state,content_ready FROM remote_nodes WHERE registration_id=? ORDER BY node_id")
        .bind(registration)
        .fetch_all(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    (index, nodes)
}

async fn assert_old_unchanged(rig: &Rig) {
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap()
            == rig.before_draft
    );
    assert!(state.store.list_outbox(&rig.old, 0, 100).await.unwrap() == rig.before_outbox);
    assert!(
        state
            .store
            .read_outbox_operation(&rig.old, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap()
            == rig.operation
    );
}

async fn close(rig: Rig) {
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_inspection_rejects_delayed_owner_or_registration_replacement() {
    for transition in ["owner", "registration"] {
        let mut rig = rig(true, None).await;
        let task = tokio::spawn({
            let runtime = Runtime(rig.runtime.0.clone());
            let current = rig.current.clone();
            let old = rig.old.clone();
            let operation = rig.operation.operation_id.clone();
            async move {
                inspect_frozen_content_for_renderer(&runtime, "owner", &current, &old, &operation)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), rig.observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = rig.runtime.0.lock().await;
            if transition == "owner" {
                state.gate.replace_owner("new-owner".into());
            } else {
                let pair = state.gate.begin_pair("owner").unwrap();
                let third = registration_binding(rig.third.binding());
                state.store.activate_registration(&third).await.unwrap();
                state.gate.commit_pair(&pair).unwrap();
                state.session = Some(rig.third.clone());
            }
        }
        rig.release.send(()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(ref error) if error != UNIMPLEMENTED));
        assert_old_unchanged(&rig).await;
        assert_eq!(
            std::fs::read(rig._directory.path().join("notes/example.md")).unwrap(),
            rig.original_file
        );
        let mut state = rig.runtime.0.lock().await;
        let active = state.store.current_registration().await.unwrap().unwrap();
        assert!(
            state
                .store
                .read_registered_checkpoint(&rig.current, &rig.document)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .store
                .read_registered_checkpoint(&active, &rig.document)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .store
                .list_outbox(&active, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        drop(state);
        assert!(
            stored_metadata(rig._directory.path(), &rig.current).await
                == rig.before_current_metadata
        );
        if transition == "registration" {
            assert_eq!(
                stored_metadata(rig._directory.path(), &active).await,
                (None, vec![])
            );
        }
        close(rig).await;
    }
}

#[tokio::test]
async fn frozen_content_inspection_rejects_download_digest_and_length_mismatch() {
    let baseline = rig(false, None).await;
    let valid = inspect_frozen_content_for_renderer(
        &baseline.runtime,
        "owner",
        &baseline.current,
        &baseline.old,
        &baseline.operation.operation_id,
    )
    .await
    .unwrap();
    assert_eq!(valid.remote_bytes, BASE);
    assert!(valid.operation == baseline.operation);
    assert_old_unchanged(&baseline).await;
    close(baseline).await;
    for corruption in [5, 6] {
        let mut rig = rig(true, None).await;
        rig.mode.store(corruption, Ordering::SeqCst);
        let task = tokio::spawn({
            let runtime = Runtime(rig.runtime.0.clone());
            let current = rig.current.clone();
            let old = rig.old.clone();
            let operation = rig.operation.operation_id.clone();
            async move {
                inspect_frozen_content_for_renderer(&runtime, "owner", &current, &old, &operation)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), rig.observed.recv())
            .await
            .unwrap()
            .unwrap();
        rig.release.send(()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(ref error) if error != UNIMPLEMENTED));
        assert_old_unchanged(&rig).await;
        assert_eq!(
            std::fs::read(rig._directory.path().join("notes/example.md")).unwrap(),
            rig.original_file
        );
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .list_outbox(&rig.current, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        drop(state);
        close(rig).await;
    }
}

#[tokio::test]
async fn frozen_content_inspection_does_not_present_stale_download_as_latest() {
    let mut rig = rig(true, None).await;
    let task = tokio::spawn({
        let runtime = Runtime(rig.runtime.0.clone());
        let current = rig.current.clone();
        let old = rig.old.clone();
        let operation = rig.operation.operation_id.clone();
        async move {
            inspect_frozen_content_for_renderer(&runtime, "owner", &current, &old, &operation).await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), rig.observed.recv())
        .await
        .unwrap()
        .unwrap();
    let receipt = rig
        .peer
        .put_content(&ContentMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: rig.node.node_id,
            expected_revision: rig.node.content_rev.clone().unwrap(),
            bytes: PEER.to_vec(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed { content_rev, .. } = receipt else {
        panic!("peer mutation was not committed")
    };
    rig.release.send(()).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    match result {
        Ok(snapshot) => {
            assert_eq!(snapshot.remote_revision, content_rev.as_str());
            assert_eq!(snapshot.remote_bytes, PEER);
            assert!(snapshot.operation == rig.operation);
            assert_eq!(snapshot.local_bytes, DRAFT);
            assert_eq!(snapshot.base_bytes, BASE);
        }
        Err(error) => assert_ne!(error, UNIMPLEMENTED),
    }
    assert_old_unchanged(&rig).await;
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/example.md")).unwrap(),
        PEER
    );
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .list_outbox(&rig.current, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    drop(state);
    close(rig).await;
}

#[tokio::test]
async fn frozen_content_inspection_keeps_cross_workspace_and_epoch_data_isolated() {
    for scope in ["workspace", "epoch"] {
        let rig = rig(false, Some(scope)).await;
        let result = inspect_frozen_content_for_renderer(
            &rig.runtime,
            "owner",
            &rig.current,
            &rig.old,
            &rig.operation.operation_id,
        )
        .await;
        if scope == "workspace" {
            assert!(matches!(result, Err(ref error) if error != UNIMPLEMENTED));
        } else {
            match result {
                Ok(snapshot) => {
                    assert!(snapshot.operation == rig.operation);
                    assert_eq!(snapshot.local_bytes, DRAFT);
                    assert_eq!(snapshot.base_bytes, BASE);
                    assert_eq!(snapshot.remote_bytes, BASE);
                }
                Err(error) => assert_ne!(error, UNIMPLEMENTED),
            }
        }
        assert_old_unchanged(&rig).await;
        assert_eq!(
            std::fs::read(rig._directory.path().join("notes/example.md")).unwrap(),
            rig.original_file
        );
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .list_outbox(&rig.current, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        drop(state);
        close(rig).await;
    }
}
