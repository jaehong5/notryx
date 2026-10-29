use crate::{
    Runtime, RuntimeState,
    frozen_content_native::inspect_frozen_content_for_renderer,
    registration_binding,
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
};
use notryx_client_runtime::{ContentMutation, Session};
use notryx_client_store::{ClientStore, OutboxOperation, OutboxStatus, RegistrationBinding};
use notryx_protocol::OperationReceipt;
use notryx_server::{
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::sync::Arc;
use tokio::sync::Mutex;

const BASE: &[u8] = b"move source\n";
const SENT: &[u8] = b"first synthetic edit\n";
const LOCAL: &[u8] = b"later synthetic edit\n";

async fn stream_watermarks(path: &std::path::Path) -> Vec<(i64, i64, i64)> {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let rows = sqlx::query_as(
        "SELECT registration_id, ack_through, confirmed_through FROM outbox_streams ORDER BY registration_id",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    rows
}

struct Rig {
    _directory: tempfile::TempDir,
    server: RunningServer,
    runtime: Runtime,
    session: Arc<Session>,
    old: RegistrationBinding,
    current: RegistrationBinding,
    document: String,
    base_revision: String,
    sent: OutboxOperation,
}

async fn rig() -> Rig {
    let (directory, server, _unused_runtime, original, _) = setup().await;
    let node = original
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "move.md")
        .unwrap();
    assert_eq!(original.content(&node).await.unwrap(), BASE);
    let document = node.node_id.to_string();
    let base_revision = node.content_rev.as_ref().unwrap().as_str().to_owned();
    let old = registration_binding(original.binding());
    let mut store = ClientStore::open(directory.path().join("frozen-content-inspection.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&old).await.unwrap();
    let opened = store
        .open_registered_document(&old, &document, BASE, &base_revision)
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, &document, &opened.edit_session_id, 1, SENT)
        .await
        .unwrap();
    store.queue_checkpoint(&old, &document, 1).await.unwrap();
    let sent = store.prepare_outbox(&old).await.unwrap().unwrap();
    assert_eq!(sent.status, OutboxStatus::Sent);
    let receipt = original
        .put_content(&ContentMutation {
            binding: original.binding().clone(),
            operation_id: sent.operation_id.parse().unwrap(),
            operation_seq: sent.operation_seq,
            node_id: node.node_id,
            expected_revision: node.content_rev.clone().unwrap(),
            bytes: SENT.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Committed { .. }));
    // The server committed the request, but the old local store has not applied its receipt.
    drop(receipt);
    store
        .save_registered_checkpoint(&old, &document, &opened.edit_session_id, 2, LOCAL)
        .await
        .unwrap();
    let successor = store.queue_checkpoint(&old, &document, 2).await.unwrap();
    assert_ne!(successor.operation_id, sent.operation_id);
    store
        .close_registered_document(&old, &document, &opened.edit_session_id)
        .await
        .unwrap();
    let mut state = RuntimeState::new(store, "owner".into());
    state.session = Some(original.clone());
    let runtime = Runtime(Arc::new(Mutex::new(state)));

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
    let session = Arc::new(
        Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let current = registration_binding(session.binding());
    assert_eq!(old.workspace_id, current.workspace_id);
    assert_eq!(old.workspace_epoch, current.workspace_epoch);
    assert_ne!(old.device_stream_id, current.device_stream_id);
    {
        let mut state = runtime.0.lock().await;
        let pair = state.gate.begin_pair("owner").unwrap();
        state.store.activate_registration(&current).await.unwrap();
        state.gate.commit_pair(&pair).unwrap();
        state.session = Some(session.clone());
    }
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    Rig {
        _directory: directory,
        server,
        runtime,
        session,
        old,
        current,
        document,
        base_revision,
        sent,
    }
}

#[tokio::test]
async fn frozen_content_inspection_fetches_real_server_without_resolving_sent_or_changing_drafts() {
    let rig = rig().await;
    let (before_draft, before_old, before_new, before_sent, before_followup) = {
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap();
        let old = state.store.list_outbox(&rig.old, 0, 100).await.unwrap();
        let new = state.store.list_outbox(&rig.current, 0, 100).await.unwrap();
        let sent = state
            .store
            .read_outbox_operation(&rig.old, &rig.sent.operation_id)
            .await
            .unwrap()
            .unwrap();
        let followup = state
            .store
            .read_outbox_operation(&rig.old, &old.entries[1].operation_id)
            .await
            .unwrap()
            .unwrap();
        (draft, old, new, sent, followup)
    };
    assert_eq!(before_draft.generation, 2);
    assert_eq!(before_draft.bytes, LOCAL);
    assert_eq!(before_draft.base_bytes, BASE);
    assert_eq!(before_draft.base_revision, rig.base_revision);
    assert_eq!(before_old.entries.len(), 2);
    assert!(before_old.entries.iter().all(|item| item.frozen));
    assert_eq!(before_old.entries[0].status, OutboxStatus::Sent);
    assert_eq!(before_old.entries[0].operation_id, rig.sent.operation_id);
    assert!(before_new.entries.is_empty());
    let remote = rig
        .session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.document)
        .unwrap();
    assert_eq!(rig.session.content(&remote).await.unwrap(), SENT);
    let server_head = rig.session.server_info().await.unwrap().head_seq;
    let stream_path = rig
        ._directory
        .path()
        .join("frozen-content-inspection.sqlite");
    let watermarks = stream_watermarks(&stream_path).await;
    let inspection = inspect_frozen_content_for_renderer(
        &rig.runtime,
        "owner",
        &rig.current,
        &rig.old,
        &rig.sent.operation_id,
    )
    .await
    .unwrap();
    assert!(inspection.operation == before_sent);
    assert!(inspection.outcome_unknown);
    assert_eq!(inspection.local_bytes, LOCAL);
    assert_eq!(inspection.base_bytes, BASE);
    assert_eq!(inspection.base_revision, rig.base_revision);
    assert_eq!(
        inspection.remote_revision,
        remote.content_rev.unwrap().as_str()
    );
    assert_eq!(inspection.remote_bytes, SENT);
    let followup_inspection = inspect_frozen_content_for_renderer(
        &rig.runtime,
        "owner",
        &rig.current,
        &rig.old,
        &before_followup.operation_id,
    )
    .await
    .unwrap();
    assert!(followup_inspection.operation == before_followup);
    assert_eq!(followup_inspection.operation.expected_revision, None);
    assert_eq!(followup_inspection.base_revision, rig.base_revision);
    assert_eq!(followup_inspection.local_bytes, LOCAL);
    assert_eq!(followup_inspection.remote_bytes, SENT);
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap(),
        before_draft
    );
    assert_eq!(
        state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        before_old
    );
    assert_eq!(
        state.store.list_outbox(&rig.current, 0, 100).await.unwrap(),
        before_new
    );
    assert!(
        state
            .store
            .read_outbox_operation(&rig.old, &rig.sent.operation_id)
            .await
            .unwrap()
            .unwrap()
            == before_sent
    );
    assert!(
        state
            .store
            .read_outbox_operation(&rig.old, &before_followup.operation_id)
            .await
            .unwrap()
            .unwrap()
            == before_followup
    );
    drop(state);
    assert_eq!(
        rig.session.server_info().await.unwrap().head_seq,
        server_head
    );
    assert_eq!(stream_watermarks(&stream_path).await, watermarks);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_inspection_rejects_wrong_owner_and_different_workspace_without_mutation() {
    let rig = rig().await;
    let before = {
        let mut state = rig.runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&rig.old, &rig.document)
                .await
                .unwrap(),
            state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
            state.store.list_outbox(&rig.current, 0, 100).await.unwrap(),
        )
    };
    assert!(
        inspect_frozen_content_for_renderer(
            &rig.runtime,
            "wrong-owner",
            &rig.current,
            &rig.old,
            &rig.sent.operation_id
        )
        .await
        .is_err()
    );
    let mut other = rig.old.clone();
    other.workspace_id = uuid::Uuid::new_v4().to_string();
    assert!(
        inspect_frozen_content_for_renderer(
            &rig.runtime,
            "owner",
            &rig.current,
            &other,
            &rig.sent.operation_id
        )
        .await
        .is_err()
    );
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap(),
        before.0
    );
    assert_eq!(
        state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        before.1
    );
    assert_eq!(
        state.store.list_outbox(&rig.current, 0, 100).await.unwrap(),
        before.2
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}
