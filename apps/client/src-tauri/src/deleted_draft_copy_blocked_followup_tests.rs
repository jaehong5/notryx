use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
    sync_catchup::catch_up_metadata,
};
use notryx_client_runtime::{DeleteMutation, Session};
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

use notryx_client_store::{
    DeletedDraftCopyIntent, OutboxAbandonState, OutboxFailureKind, OutboxOperation,
    RegistrationBinding,
};

struct DeletedCopyRig {
    _directory: tempfile::TempDir,
    server: notryx_server::http::RunningServer,
    config: Config,
    runtime: Runtime,
    binding: RegistrationBinding,
    document: String,
    intent: DeletedDraftCopyIntent,
    original: OutboxOperation,
    followup: OutboxOperation,
    unrelated: OutboxOperation,
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
    store
        .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 2, CONFIRMED)
        .await
        .unwrap();
    let followup = store
        .queue_checkpoint(&binding, &document, 2)
        .await
        .unwrap();
    let other = inventory
        .nodes
        .iter()
        .find(|node| node.name == "example.md")
        .unwrap();
    let other_id = other.node_id.to_string();
    let other_edit = store
        .open_registered_document(
            &binding,
            &other_id,
            b"server source\n",
            other.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            &other_id,
            &other_edit.edit_session_id,
            1,
            b"unrelated local\n",
        )
        .await
        .unwrap();
    let unrelated = store
        .queue_checkpoint(&binding, &other_id, 1)
        .await
        .unwrap();
    store
        .close_registered_document(&binding, &other_id, &other_edit.edit_session_id)
        .await
        .unwrap();
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
    let (intent, followup) = {
        let mut state = runtime.0.lock().await;
        let (_, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        let tombstone = nodes
            .into_iter()
            .find(|node| node.node_id == source.node_id)
            .unwrap();
        assert!(tombstone.deleted);
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
        let merged = state
            .store
            .queue_checkpoint(&binding, &document, 3)
            .await
            .unwrap();
        assert_eq!(merged.operation_id, followup.operation_id);
        assert_eq!(merged.operation_seq, followup.operation_seq);
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
        (intent, merged)
    };
    let resolved = crate::send::send_one(runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(resolved.status, WorkspaceSendStatus::Failed);
    DeletedCopyRig {
        _directory: directory,
        server,
        config,
        runtime,
        binding,
        document,
        intent,
        original,
        followup,
        unrelated,
    }
}

#[tokio::test]
async fn deleted_copy_followup_requires_approval_of_exact_queued_operation_after_sent_is_resolved()
{
    let rig = deleted_copy_rig().await;
    assert_eq!(rig.followup.status, OutboxStatus::Queued);
    assert!(rig.followup.expected_revision.is_none());
    assert!(rig.original.operation_seq < rig.followup.operation_seq);
    assert!(rig.followup.operation_seq < rig.unrelated.operation_seq);
    let create = {
        let mut state = rig.runtime.0.lock().await;
        let original = state
            .store
            .read_outbox_operation(&rig.binding, &rig.original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.status, OutboxStatus::Failed);
        assert_eq!(original.failure_kind, Some(OutboxFailureKind::NodeDeleted));
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            original.operation_seq
        );
        state
            .store
            .queue_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap()
    };
    let blocked = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(blocked.status, WorkspaceSendStatus::NeedsReview);
    let reason = blocked
        .content_block
        .expect("blocked content must identify both operations");
    assert_eq!(reason.conflict_operation_id, rig.original.operation_id);
    assert_eq!(reason.blocked_operation_id, rig.followup.operation_id);
    assert_eq!(reason.blocked_seq, rig.followup.operation_seq.to_string());
    {
        let mut state = rig.runtime.0.lock().await;
        let candidate = state
            .store
            .read_deleted_draft_copy_blocking_followup(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(candidate.blocker_operation_id, rig.original.operation_id);
        assert_eq!(candidate.followup_operation_id, rig.followup.operation_id);
        assert_eq!(candidate.followup_operation_seq, rig.followup.operation_seq);
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.followup.operation_id)
                .await
                .unwrap()
                .unwrap()
                .abandon_state,
            OutboxAbandonState::None
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.unrelated.operation_id)
                .await
                .unwrap()
                .unwrap(),
            rig.unrelated
        );
        assert!(
            state
                .store
                .request_deleted_draft_copy_followup_abandon(
                    &rig.binding,
                    &rig.document,
                    &rig.original.operation_id,
                    &rig.unrelated.operation_id,
                    rig.followup.operation_seq,
                    rig.followup.generation,
                    &rig.followup.payload_hash,
                )
                .await
                .is_err()
        );
        assert!(
            state
                .store
                .request_deleted_draft_copy_followup_abandon(
                    &rig.binding,
                    &rig.document,
                    &rig.original.operation_id,
                    &Uuid::now_v7().to_string(),
                    rig.followup.operation_seq,
                    rig.followup.generation,
                    &rig.followup.payload_hash,
                )
                .await
                .is_err()
        );
        assert!(
            state
                .store
                .request_deleted_draft_copy_followup_abandon(
                    &rig.binding,
                    &rig.document,
                    &Uuid::now_v7().to_string(),
                    &rig.followup.operation_id,
                    rig.followup.operation_seq,
                    rig.followup.generation,
                    &rig.followup.payload_hash,
                )
                .await
                .is_err()
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.followup.operation_id)
                .await
                .unwrap()
                .unwrap()
                .abandon_state,
            OutboxAbandonState::None
        );
        let approved = state
            .store
            .request_deleted_draft_copy_followup_abandon(
                &rig.binding,
                &rig.document,
                &rig.original.operation_id,
                &rig.followup.operation_id,
                rig.followup.operation_seq,
                rig.followup.generation,
                &rig.followup.payload_hash,
            )
            .await
            .unwrap();
        assert_eq!(approved.operation_id, rig.followup.operation_id);
        assert_eq!(approved.abandon_state, OutboxAbandonState::Requested);
    }
    let abandoned = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(abandoned.status, WorkspaceSendStatus::Abandoned);
    assert_eq!(
        abandoned.operation_id.as_deref(),
        Some(rig.followup.operation_id.as_str())
    );
    let unrelated = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(unrelated.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        unrelated.operation_id.as_deref(),
        Some(rig.unrelated.operation_id.as_str())
    );
    let created = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(created.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        created.operation_id.as_deref(),
        Some(create.operation_id.as_str())
    );
    let content = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(content.bytes, CONFIRMED);
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(content.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        CONFIRMED
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        b"unrelated local\n"
    );
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
        Some(rig.intent)
    );
    assert!(
        state
            .store
            .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
            .await
            .unwrap()
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_followup_approval_retry_after_ack_keeps_unrelated_fifo_unchanged() {
    let rig = deleted_copy_rig().await;
    {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .queue_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap();
        let approved = state
            .store
            .request_deleted_draft_copy_followup_abandon(
                &rig.binding,
                &rig.document,
                &rig.original.operation_id,
                &rig.followup.operation_id,
                rig.followup.operation_seq,
                rig.followup.generation,
                &rig.followup.payload_hash,
            )
            .await
            .unwrap();
        assert_eq!(approved.abandon_state, OutboxAbandonState::Requested);
        assert_eq!(
            state
                .store
                .request_deleted_draft_copy_followup_abandon(
                    &rig.binding,
                    &rig.document,
                    &rig.original.operation_id,
                    &rig.followup.operation_id,
                    rig.followup.operation_seq,
                    rig.followup.generation,
                    &rig.followup.payload_hash,
                )
                .await
                .unwrap(),
            approved
        );
    }
    let abandoned = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(abandoned.status, WorkspaceSendStatus::Abandoned);
    assert_eq!(
        abandoned.operation_id.as_deref(),
        Some(rig.followup.operation_id.as_str())
    );
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap(),
        rig.followup.operation_seq
    );
    let terminal = state
        .store
        .read_outbox_operation(&rig.binding, &rig.followup.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.status, OutboxStatus::Abandoned);
    let retry = state
        .store
        .request_deleted_draft_copy_followup_abandon(
            &rig.binding,
            &rig.document,
            &rig.original.operation_id,
            &rig.followup.operation_id,
            rig.followup.operation_seq,
            rig.followup.generation,
            &rig.followup.payload_hash,
        )
        .await
        .unwrap();
    assert_eq!(retry, terminal);
    assert!(
        state
            .store
            .request_deleted_draft_copy_followup_abandon(
                &rig.binding,
                &rig.document,
                &Uuid::now_v7().to_string(),
                &rig.followup.operation_id,
                rig.followup.operation_seq,
                rig.followup.generation,
                &rig.followup.payload_hash,
            )
            .await
            .is_err()
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.unrelated.operation_id)
            .await
            .unwrap()
            .unwrap(),
        rig.unrelated
    );
    assert_eq!(
        state
            .store
            .next_outbox(&rig.binding)
            .await
            .unwrap()
            .unwrap(),
        rig.unrelated
    );
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap(),
        Some(rig.intent)
    );
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LATER);
    drop(state);
    rig.server.shutdown().await.unwrap();
}
