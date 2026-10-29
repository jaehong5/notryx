use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
    sync_catchup::catch_up_metadata,
};
use notryx_client_runtime::{ContentMutation, DeleteMutation, Session};
use notryx_client_store::{
    ClientStore, DeletedDraftCopyEvidence, DeletedDraftCopyRequest, OutboxStatus,
};
use notryx_protocol::{OperationReceipt, WorkspaceSendStatus};
use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

const OWNER: &str = "deleted-copy-owner";
const BASE: &[u8] = b"move source\n";
const SENT: &[u8] = b"sent local edit\n";
const CONFIRMED: &[u8] = b"confirmed local copy\n";
const LATER: &[u8] = b"later unsent edit\n";

use notryx_client_runtime::CreateMutation;
use notryx_client_store::{
    DeletedDraftCopyIntent, OutboxCommit, PendingOutboxOperation, RegistrationBinding,
};
use notryx_protocol::{CreateNodeRequest, NodeKind};

struct DeletedCopyRig {
    directory: tempfile::TempDir,
    server: notryx_server::http::RunningServer,
    config: Config,
    runtime: Runtime,
    session_a: Arc<Session>,
    session_b: Session,
    binding: RegistrationBinding,
    document: String,
    base_revision: String,
    intent: DeletedDraftCopyIntent,
}

async fn deleted_copy_rig() -> DeletedCopyRig {
    let (directory, server, _runtime, _session, _) = setup().await;
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
    let pairing_a = server_test_support::pairing_request(&config).await.0;
    let session_a = Arc::new(
        Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing_a.workspace_id,
            &pairing_a.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let pairing_b = server_test_support::pairing_request(&config).await.0;
    let session_b = Session::pair(
        &format!("http://{}", server.local_addr()),
        pairing_b.workspace_id,
        &pairing_b.pairing_secret,
    )
    .await
    .unwrap();
    let binding = registration_binding(session_a.binding());
    let inventory = session_a.inventory(None).await.unwrap();
    let source = inventory
        .nodes
        .iter()
        .find(|node| node.name == "move.md")
        .unwrap()
        .clone();
    let document = source.node_id.to_string();
    let parent = source.parent_id.unwrap().to_string();
    let client_path = directory.path().join("deleted-copy.sqlite");
    let mut store = ClientStore::open(&client_path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    let opened = store
        .open_registered_document(
            &binding,
            &document,
            BASE,
            source.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 1, SENT)
        .await
        .unwrap();
    store
        .queue_checkpoint(&binding, &document, 1)
        .await
        .unwrap();
    let original = store.prepare_outbox(&binding).await.unwrap().unwrap();
    assert_eq!(original.status, OutboxStatus::Sent);
    let mut state = RuntimeState::new(store, OWNER.into());
    state.session = Some(session_a.clone());
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
    bootstrap_metadata(&runtime, &snapshot, &session_a)
        .await
        .unwrap();

    let current = session_b
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == source.node_id)
        .unwrap();
    let deleted = session_b
        .delete(&DeleteMutation {
            binding: session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: source.node_id,
            expected_node_etag: current.node_etag,
        })
        .await
        .unwrap();
    assert!(matches!(deleted, OperationReceipt::Deleted { .. }));
    catch_up_metadata(&runtime, &snapshot, &session_a)
        .await
        .unwrap();
    let intent = {
        let mut state = runtime.0.lock().await;
        let (_, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        let tombstone = nodes
            .into_iter()
            .find(|node| node.node_id == source.node_id)
            .unwrap();
        assert!(tombstone.deleted);
        state
            .store
            .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 2, CONFIRMED)
            .await
            .unwrap();
        let intent = state
            .store
            .record_deleted_draft_copy(
                &binding,
                &document,
                &DeletedDraftCopyRequest {
                    edit_session_id: opened.edit_session_id.clone(),
                    generation: 2,
                    local_bytes: CONFIRMED.to_vec(),
                    parent_id: parent,
                    name: "deleted-copy.md".into(),
                    evidence: DeletedDraftCopyEvidence::Tombstone {
                        last_change_seq: tombstone.last_change_seq,
                    },
                },
            )
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 3, LATER)
            .await
            .unwrap();
        let current = state
            .store
            .read_outbox_operation(&binding, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current, original);
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            0
        );
        intent
    };
    let base_revision = source.content_rev.as_ref().unwrap().as_str().to_owned();
    let resolved = crate::send::send_one(runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(resolved.status, WorkspaceSendStatus::Failed);
    DeletedCopyRig {
        directory,
        server,
        config,
        runtime,
        session_a,
        session_b,
        binding,
        document,
        base_revision,
        intent,
    }
}

async fn restart(rig: DeletedCopyRig) -> DeletedCopyRig {
    let DeletedCopyRig {
        directory,
        server,
        config,
        runtime,
        session_a,
        session_b,
        binding,
        document,
        base_revision,
        intent,
    } = rig;
    drop(runtime);
    let mut store = ClientStore::open(&directory.path().join("deleted-copy.sqlite"))
        .await
        .unwrap();
    let reopened = store
        .open_registered_document(&binding, &document, BASE, &base_revision)
        .await
        .unwrap();
    let mut state = RuntimeState::new(store, OWNER.into());
    state.session = Some(session_a.clone());
    state
        .gate
        .open_session(reopened.edit_session_id.clone())
        .unwrap();
    state.product_sessions.insert(
        reopened.edit_session_id,
        ProductSession {
            document_id: document.clone(),
            binding: binding.clone(),
            kind: ProductSessionKind::Active,
        },
    );
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    bootstrap_metadata(&runtime, &snapshot, &session_a)
        .await
        .unwrap();
    DeletedCopyRig {
        directory,
        server,
        config,
        runtime,
        session_a,
        session_b,
        binding,
        document,
        base_revision,
        intent,
    }
}

async fn preserved(rig: &DeletedCopyRig) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LATER);
    assert_eq!(draft.generation, 3);
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap(),
        Some(rig.intent.clone())
    );
}

#[tokio::test]
async fn deleted_copy_create_conflict_preserves_existing_file_and_confirmed_draft_after_restart() {
    let rig = deleted_copy_rig().await;
    let create = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy(&rig.binding, &rig.document)
        .await
        .unwrap();
    let peer = rig
        .session_b
        .create(&CreateMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            request: CreateNodeRequest {
                node_id: Uuid::now_v7().to_string().parse().unwrap(),
                parent_id: rig.intent.parent_id.parse().unwrap(),
                name: rig.intent.name.clone(),
                kind: NodeKind::File,
                upload_id: None,
            },
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Created { .. }));
    let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Failed);
    assert_eq!(
        result.operation_id.as_deref(),
        Some(create.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        b""
    );
    preserved(&rig).await;
    let rig = restart(rig).await;
    let mut state = rig.runtime.0.lock().await;
    let same = state
        .store
        .queue_deleted_draft_copy(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(same.operation_id, create.operation_id);
    assert_eq!(same.status, OutboxStatus::Failed);
    assert!(
        state
            .store
            .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
            .await
            .is_err()
    );
    assert!(
        !state
            .store
            .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
            .await
            .unwrap()
    );
    drop(state);
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_lost_responses_and_ack_survive_restart_without_reporting_early() {
    let rig = deleted_copy_rig().await;
    let create = {
        let mut state = rig.runtime.0.lock().await;
        let create = state
            .store
            .queue_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap();
        assert!(matches!(
            state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap(),
            Some(PendingOutboxOperation::Create(_))
        ));
        create
    };
    let created = rig
        .session_a
        .create(&CreateMutation {
            binding: rig.session_a.binding().clone(),
            operation_id: create.operation_id.parse().unwrap(),
            operation_seq: create.operation_seq,
            request: create.request.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(created, OperationReceipt::Created { .. }));
    let head = rig.session_a.server_info().await.unwrap().head_seq;
    let rig = restart(rig).await;
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(create.operation_id.as_str())
    );
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    assert_eq!(rig.session_a.server_info().await.unwrap().head_seq, head);
    let content = {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            !state
                .store
                .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
                .await
                .unwrap()
        );
        let content = state
            .store
            .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
            .await
            .unwrap();
        assert_eq!(content.bytes, CONFIRMED);
        assert!(matches!(
            state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap(),
            Some(PendingOutboxOperation::Content(_))
        ));
        content
    };
    let receipt = rig
        .session_a
        .put_content(&ContentMutation {
            binding: rig.session_a.binding().clone(),
            operation_id: content.operation_id.parse().unwrap(),
            operation_seq: content.operation_seq,
            node_id: content.document_id.parse().unwrap(),
            expected_revision: notryx_domain::ContentRevision::new(
                content.expected_revision.clone().unwrap(),
            )
            .unwrap(),
            bytes: content.bytes.clone(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed {
        content_rev,
        content_hash,
        ..
    } = receipt
    else {
        panic!("expected content commit")
    };
    let head = rig.session_a.server_info().await.unwrap().head_seq;
    let rig = restart(rig).await;
    {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            !state
                .store
                .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
                .await
                .unwrap()
        );
        assert_eq!(
            state
                .store
                .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
                .await
                .unwrap()
                .operation_id,
            content.operation_id
        );
        state
            .store
            .apply_outbox_commit(
                &rig.binding,
                &OutboxCommit {
                    operation_id: content.operation_id.clone(),
                    operation_seq: content.operation_seq,
                    workspace_epoch: rig.binding.workspace_epoch.clone(),
                    document_id: content.document_id.clone(),
                    content_revision: content_rev.as_str().into(),
                    content_hash,
                },
            )
            .await
            .unwrap();
        assert!(
            !state
                .store
                .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
                .await
                .unwrap()
        );
    }
    let rig = restart(rig).await;
    assert!(
        !rig.runtime
            .0
            .lock()
            .await
            .store
            .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
            .await
            .unwrap()
    );
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Idle);
    assert_eq!(rig.session_a.server_info().await.unwrap().head_seq, head);
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
            .await
            .unwrap()
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        CONFIRMED
    );
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_content_conflict_preserves_both_versions_after_restart() {
    let rig = deleted_copy_rig().await;
    rig.runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy(&rig.binding, &rig.document)
        .await
        .unwrap();
    let created = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(created.status, WorkspaceSendStatus::Committed);
    let content = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
        .await
        .unwrap();
    let node = rig
        .session_b
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.intent.node_id)
        .unwrap();
    let peer = rig
        .session_b
        .put_content(&ContentMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: node.node_id,
            expected_revision: node.content_rev.unwrap(),
            bytes: b"peer copy content\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Committed { .. }));
    let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Conflict);
    assert_eq!(
        result.operation_id.as_deref(),
        Some(content.operation_id.as_str())
    );
    let rig = restart(rig).await;
    let mut state = rig.runtime.0.lock().await;
    assert!(
        !state
            .store
            .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
            .await
            .unwrap()
    );
    let original = state
        .store
        .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(original.operation_id, content.operation_id);
    assert_eq!(original.bytes, CONFIRMED);
    assert_eq!(original.status, OutboxStatus::Conflict);
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        b"peer copy content\n"
    );
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}
