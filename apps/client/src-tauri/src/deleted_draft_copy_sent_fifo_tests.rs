// Future store API used by these tests: queue_deleted_draft_copy(binding, document_id),
// queue_deleted_draft_copy_content(binding, document_id), and
// deleted_draft_copy_content_is_saved(binding, document_id). The file is not yet included
// in the test module; these calls specify the intended integration boundary.
use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
    sync_catchup::catch_up_metadata,
};
use notryx_client_runtime::{ContentMutation, DeleteMutation, Session};
use notryx_client_store::{
    ClientStore, DeletedDraftCopyEvidence, DeletedDraftCopyRequest, OutboxFailureKind, OutboxStatus,
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

async fn sent_put_then_copy(server_received_put: bool) {
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

    let put = ContentMutation {
        binding: session_a.binding().clone(),
        operation_id: original.operation_id.parse().unwrap(),
        operation_seq: original.operation_seq,
        node_id: source.node_id,
        expected_revision: notryx_domain::ContentRevision::new(
            original.expected_revision.clone().unwrap(),
        )
        .unwrap(),
        bytes: original.bytes.clone(),
    };
    if server_received_put {
        let received = session_a.put_content(&put).await.unwrap();
        assert!(matches!(received, OperationReceipt::Committed { .. }));
        // The returned receipt is deliberately not applied to client SQLite.
    }
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
    drop(runtime);

    let mut store = ClientStore::open(&client_path).await.unwrap();
    let reopened = store
        .open_registered_document(
            &binding,
            &document,
            BASE,
            source.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    let mut state = RuntimeState::new(store, OWNER.into());
    state.session = Some(session_a.clone());
    state
        .gate
        .open_session(reopened.edit_session_id.clone())
        .unwrap();
    state.product_sessions.insert(
        reopened.edit_session_id.clone(),
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
    let create = {
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_deleted_draft_copy(&binding, &document)
                .await
                .unwrap(),
            Some(intent.clone())
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&binding, &original.operation_id)
                .await
                .unwrap()
                .unwrap(),
            original
        );
        let create = state
            .store
            .queue_deleted_draft_copy(&binding, &document)
            .await
            .unwrap();
        assert_eq!(create.operation_id, intent.create_operation_id);
        assert_eq!(create.request.node_id.to_string(), intent.node_id);
        assert!(create.operation_seq > original.operation_seq);
        assert!(
            state
                .store
                .queue_deleted_draft_copy_content(&binding, &document)
                .await
                .is_err()
        );
        assert!(
            !state
                .store
                .deleted_draft_copy_content_is_saved(&binding, &document)
                .await
                .unwrap()
        );
        create
    };
    let before = session_a.server_info().await.unwrap().head_seq;
    let resolved = crate::send::send_one(runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        resolved.operation_id.as_deref(),
        Some(original.operation_id.as_str())
    );
    assert_eq!(
        resolved.status,
        if server_received_put {
            WorkspaceSendStatus::Committed
        } else {
            WorkspaceSendStatus::Failed
        }
    );
    assert_eq!(session_a.server_info().await.unwrap().head_seq, before);
    assert!(!config.workspace.notes_root.join("deleted-copy.md").exists());
    {
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            original.operation_seq
        );
        if !server_received_put {
            let terminal = state
                .store
                .read_outbox_operation(&binding, &original.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(terminal.failure_kind, Some(OutboxFailureKind::NodeDeleted));
        }
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, 3);
        assert_eq!(draft.bytes, LATER);
        assert_eq!(
            draft.base_bytes,
            if server_received_put { SENT } else { BASE }
        );
        assert_eq!(
            state
                .store
                .read_deleted_draft_copy(&binding, &document)
                .await
                .unwrap(),
            Some(intent.clone())
        );
        assert!(
            !state
                .store
                .deleted_draft_copy_content_is_saved(&binding, &document)
                .await
                .unwrap()
        );
    }
    let created = crate::send::send_one(runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(created.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        created.operation_id.as_deref(),
        Some(create.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        b""
    );
    let content = {
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            create.operation_seq
        );
        assert!(
            !state
                .store
                .deleted_draft_copy_content_is_saved(&binding, &document)
                .await
                .unwrap()
        );
        let content = state
            .store
            .queue_deleted_draft_copy_content(&binding, &document)
            .await
            .unwrap();
        assert_eq!(content.document_id, intent.node_id);
        assert_eq!(content.bytes, CONFIRMED);
        assert!(content.operation_seq > create.operation_seq);
        assert_eq!(
            state
                .store
                .queue_deleted_draft_copy_content(&binding, &document)
                .await
                .unwrap(),
            content
        );
        content
    };
    let sent = crate::send::send_one(runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(content.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        CONFIRMED
    );
    assert!(!config.workspace.notes_root.join("move.md").exists());
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .confirmed_receipt_through(&binding)
            .await
            .unwrap(),
        content.operation_seq
    );
    assert!(
        state
            .store
            .deleted_draft_copy_content_is_saved(&binding, &document)
            .await
            .unwrap()
    );
    let draft = state
        .store
        .read_registered_checkpoint(&binding, &document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LATER);
    assert_eq!(draft.generation, 3);
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy(&binding, &document)
            .await
            .unwrap(),
        Some(intent)
    );
    drop(state);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn sent_put_receipt_is_recovered_before_deleted_draft_copy() {
    sent_put_then_copy(true).await;
}

#[tokio::test]
async fn missing_sent_put_is_retried_and_terminal_before_deleted_draft_copy() {
    sent_put_then_copy(false).await;
}
