use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    remote_change_dirty::{DirtyRemoteState, inspect_dirty_remote},
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
};
use notryx_client_runtime::{ContentMutation, Session};
use notryx_client_store::{
    ClientStore, OutboxCommit, OutboxConflict, OutboxOperation, RegistrationBinding,
};
use notryx_protocol::{NodeState, OperationReceipt};
use notryx_server::{
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

const OWNER: &str = "owner";

struct Rig {
    _directory: tempfile::TempDir,
    server: RunningServer,
    runtime: Runtime,
    session: Arc<Session>,
    peer: Session,
    binding: RegistrationBinding,
    node: NodeState,
    edit_session: String,
}

async fn rig() -> Rig {
    let (directory, server, _original_runtime, session, document) = setup().await;
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
    let node = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == document)
        .unwrap();
    let binding = registration_binding(session.binding());
    let mut store = ClientStore::open(directory.path().join("dirty-race.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&binding).await.unwrap();
    let opened = store
        .open_registered_document(
            &binding,
            &document,
            b"server source\n",
            node.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            &document,
            &opened.edit_session_id,
            1,
            b"local first\n",
        )
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
            document_id: document,
            binding: binding.clone(),
            kind: ProductSessionKind::Active,
        },
    );
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    Rig {
        _directory: directory,
        server,
        runtime,
        session,
        peer,
        binding,
        node,
        edit_session: opened.edit_session_id,
    }
}

async fn inspect(rig: &Rig, generation: u64) -> DirtyRemoteState {
    inspect_dirty_remote(
        &rig.runtime,
        OWNER,
        &rig.node.node_id.to_string(),
        &rig.edit_session,
        generation,
    )
    .await
    .unwrap()
}

async fn catch_up(rig: &Rig) -> NodeState {
    let snapshot = rig.runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    let cursor = crate::sync_catchup::catch_up_metadata(&rig.runtime, &snapshot, &rig.session)
        .await
        .unwrap();
    assert_eq!(cursor, rig.session.server_info().await.unwrap().head_seq);
    rig.runtime
        .0
        .lock()
        .await
        .store
        .current_remote_file(&rig.binding, rig.node.node_id)
        .await
        .unwrap()
        .unwrap()
}

async fn queue(rig: &Rig) -> OutboxOperation {
    rig.runtime
        .0
        .lock()
        .await
        .store
        .queue_checkpoint(&rig.binding, &rig.node.node_id.to_string(), 1)
        .await
        .unwrap()
}

async fn prepare_and_commit_on_server(rig: &Rig) -> (OutboxOperation, OperationReceipt) {
    let item = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .prepare_outbox(&rig.binding)
        .await
        .unwrap()
        .unwrap();
    let mutation = ContentMutation {
        binding: rig.session.binding().clone(),
        operation_id: item.operation_id.parse().unwrap(),
        operation_seq: item.operation_seq,
        node_id: rig.node.node_id,
        expected_revision: notryx_domain::ContentRevision::new(
            item.expected_revision.clone().unwrap(),
        )
        .unwrap(),
        bytes: item.bytes.clone(),
    };
    let receipt = rig.session.put_content(&mutation).await.unwrap();
    (item, receipt)
}

async fn save_next(rig: &Rig) {
    rig.runtime
        .0
        .lock()
        .await
        .store
        .save_registered_checkpoint(
            &rig.binding,
            &rig.node.node_id.to_string(),
            &rig.edit_session,
            2,
            b"local second\n",
        )
        .await
        .unwrap();
}

async fn apply_commit(rig: &Rig, item: &OutboxOperation, receipt: &OperationReceipt) {
    let OperationReceipt::Committed {
        content_rev,
        content_hash,
        node_id,
        ..
    } = receipt
    else {
        panic!("expected committed content receipt");
    };
    rig.runtime
        .0
        .lock()
        .await
        .store
        .apply_outbox_commit(
            &rig.binding,
            &OutboxCommit {
                operation_id: item.operation_id.clone(),
                operation_seq: item.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                document_id: node_id.to_string(),
                content_revision: content_rev.as_str().into(),
                content_hash: content_hash.clone(),
            },
        )
        .await
        .unwrap();
}

async fn peer_put(
    rig: &Rig,
    expected: notryx_domain::ContentRevision,
    bytes: &[u8],
) -> OperationReceipt {
    let receipt = rig
        .peer
        .put_content(&ContentMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: rig.node.node_id,
            expected_revision: expected,
            bytes: bytes.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(&receipt, OperationReceipt::Committed { .. }));
    receipt
}

#[tokio::test]
async fn peer_change_keeps_dirty_checkpoint_and_queued_operation() {
    let rig = rig().await;
    queue(&rig).await;
    let (draft, outbox) = {
        let mut state = rig.runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
                .await
                .unwrap(),
            state.store.list_outbox(&rig.binding, 0, 100).await.unwrap(),
        )
    };
    peer_put(
        &rig,
        rig.node.content_rev.clone().unwrap(),
        b"peer content\n",
    )
    .await;
    assert_eq!(inspect(&rig, 1).await, DirtyRemoteState::Changed);
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap(),
        draft
    );
    assert_eq!(
        state.store.list_outbox(&rig.binding, 0, 100).await.unwrap(),
        outbox
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn own_server_commit_before_receipt_does_not_create_a_peer_warning() {
    let rig = rig().await;
    queue(&rig).await;
    let (item, receipt) = prepare_and_commit_on_server(&rig).await;
    save_next(&rig).await;
    let OperationReceipt::Committed { content_rev, .. } = &receipt else {
        panic!("expected own commit");
    };
    assert_eq!(catch_up(&rig).await.content_rev.as_ref(), Some(content_rev));
    assert_eq!(inspect(&rig, 2).await, DirtyRemoteState::Pending);
    apply_commit(&rig, &item, &receipt).await;
    assert_eq!(inspect(&rig, 2).await, DirtyRemoteState::Unchanged);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, b"local second\n");
    assert_eq!(draft.base_bytes, b"local first\n");
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn own_receipt_before_journal_catchup_does_not_create_a_peer_warning() {
    let rig = rig().await;
    queue(&rig).await;
    let (item, receipt) = prepare_and_commit_on_server(&rig).await;
    save_next(&rig).await;
    apply_commit(&rig, &item, &receipt).await;
    let old = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .current_remote_file(&rig.binding, rig.node.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.content_rev, rig.node.content_rev);
    assert_eq!(inspect(&rig, 2).await, DirtyRemoteState::Unchanged);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local second\n");
    assert_eq!(draft.base_bytes, b"local first\n");
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn peer_after_own_commit_is_reported_only_after_the_own_receipt() {
    let rig = rig().await;
    queue(&rig).await;
    let (item, receipt) = prepare_and_commit_on_server(&rig).await;
    save_next(&rig).await;
    let OperationReceipt::Committed { content_rev, .. } = &receipt else {
        panic!("expected own commit");
    };
    let peer_receipt = peer_put(&rig, content_rev.clone(), b"peer after own\n").await;
    let OperationReceipt::Committed {
        content_rev: peer_revision,
        ..
    } = &peer_receipt
    else {
        panic!("expected peer commit");
    };
    assert_eq!(
        catch_up(&rig).await.content_rev.as_ref(),
        Some(peer_revision)
    );
    assert_eq!(inspect(&rig, 2).await, DirtyRemoteState::Pending);
    apply_commit(&rig, &item, &receipt).await;
    assert_eq!(inspect(&rig, 2).await, DirtyRemoteState::Changed);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, b"local second\n");
    assert_eq!(draft.base_bytes, b"local first\n");
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn peer_first_then_precondition_failure_keeps_the_draft_and_conflict() {
    let rig = rig().await;
    queue(&rig).await;
    let peer_receipt = peer_put(&rig, rig.node.content_rev.clone().unwrap(), b"peer first\n").await;
    let (item, receipt) = prepare_and_commit_on_server(&rig).await;
    let OperationReceipt::PreconditionFailed {
        current_content_rev,
        current_content_etag,
        ..
    } = &receipt
    else {
        panic!("expected precondition failure");
    };
    let OperationReceipt::Committed {
        content_rev: peer_revision,
        ..
    } = &peer_receipt
    else {
        panic!("expected peer commit");
    };
    assert_eq!(
        catch_up(&rig).await.content_rev.as_ref(),
        Some(peer_revision)
    );
    assert_eq!(inspect(&rig, 1).await, DirtyRemoteState::Pending);
    rig.runtime
        .0
        .lock()
        .await
        .store
        .apply_outbox_conflict(
            &rig.binding,
            &OutboxConflict {
                operation_id: item.operation_id.clone(),
                operation_seq: item.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                current_revision: current_content_rev.as_str().into(),
                current_etag: current_content_etag.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(inspect(&rig, 1).await, DirtyRemoteState::Changed);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local first\n");
    assert_eq!(draft.base_bytes, b"server source\n");
    let outbox = state
        .store
        .document_outbox_state(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap();
    assert!(outbox.has_conflict);
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn sent_restore_for_same_node_keeps_the_change_unresolved() {
    let rig = rig().await;
    let request: notryx_protocol::RestoreNodeRequest = serde_json::from_value(serde_json::json!({
        "expected_meta_rev": rig.node.meta_rev.as_str(),
        "parent_id": rig.node.parent_id.unwrap().to_string(),
        "name": rig.node.name.clone(),
    }))
    .unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .queue_restore(&rig.binding, rig.node.node_id, &request)
            .await
            .unwrap();
        let pending = state
            .store
            .prepare_pending_outbox(&rig.binding)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            pending,
            notryx_client_store::PendingOutboxOperation::Restore(_)
        ));
    }
    let receipt = peer_put(
        &rig,
        rig.node.content_rev.clone().unwrap(),
        b"peer while restore pending\n",
    )
    .await;
    let OperationReceipt::Committed { content_rev, .. } = &receipt else {
        panic!("expected peer commit");
    };
    assert_eq!(catch_up(&rig).await.content_rev.as_ref(), Some(content_rev));
    assert_eq!(inspect(&rig, 1).await, DirtyRemoteState::Pending);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local first\n");
    assert_eq!(draft.base_bytes, b"server source\n");
    let outbox = state
        .store
        .list_restore_operations(&rig.binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(outbox.entries.len(), 1);
    assert_eq!(
        outbox.entries[0].operation.status,
        notryx_client_store::OutboxStatus::Sent
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}
