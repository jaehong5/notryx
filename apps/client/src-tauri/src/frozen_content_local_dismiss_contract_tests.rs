include!("frozen_content_dismiss_contract_tests.rs");

use notryx_client_runtime::{DeleteMutation, RestoreMutation};
use notryx_client_store::OutboxAbandonState;
use notryx_protocol::RestoreNodeRequest;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalRemoteEvidence {
    Deleted { change_seq: u64 },
    TooLarge { change_seq: u64, size: u64 },
    Unknown,
}

#[derive(Debug, Clone)]
struct LocalDismissReview {
    inspected: notryx_protocol::WorkspaceFrozenContentLocalInspection,
    current: RegistrationBinding,
    old: RegistrationBinding,
    document_id: String,
    operation_id: String,
    operation_seq: u64,
    operation_status: OutboxStatus,
    abandon_state: OutboxAbandonState,
    dependency: Option<String>,
    expected_revision: Option<String>,
    operation_generation: u64,
    operation_bytes: Vec<u8>,
    operation_hash: String,
    draft_generation: u64,
    draft_bytes: Vec<u8>,
    draft_hash: String,
    base_bytes: Vec<u8>,
    base_hash: String,
    base_revision: String,
    remote_evidence: LocalRemoteEvidence,
}

async fn local_dismiss_review_for_renderer(
    runtime: &Runtime,
    owner: &str,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
) -> Result<LocalDismissReview, String> {
    let inspected = crate::frozen_content_local_native::review_frozen_content_local_for_renderer(
        runtime,
        notryx_protocol::WorkspaceFrozenContentLocalInspectRequest {
            renderer_session_id: owner.into(),
            current_binding: crate::to_contract(current.clone()),
            old_binding: crate::to_contract(old.clone()),
            document_id: document.into(),
            operation_id: operation_id.into(),
        },
    )
    .await?;
    let (operation_status, abandon_state) =
        match (inspected.operation_status, inspected.abandon_state) {
            (WorkspaceOutboxStatus::Queued, notryx_protocol::WorkspaceAbandonState::None) => {
                (OutboxStatus::Queued, OutboxAbandonState::None)
            }
            (WorkspaceOutboxStatus::Sent, notryx_protocol::WorkspaceAbandonState::None) => {
                (OutboxStatus::Sent, OutboxAbandonState::None)
            }
            (
                WorkspaceOutboxStatus::Abandoning,
                notryx_protocol::WorkspaceAbandonState::Requested,
            ) => (OutboxStatus::Abandoning, OutboxAbandonState::Requested),
            (WorkspaceOutboxStatus::Abandoning, notryx_protocol::WorkspaceAbandonState::Sent) => {
                (OutboxStatus::Abandoning, OutboxAbandonState::Sent)
            }
            _ => return Err("invalid_operation_state".into()),
        };
    let remote_evidence = match &inspected.remote_evidence {
        notryx_protocol::WorkspaceFrozenContentRemoteEvidence::Deleted { change_seq } => {
            LocalRemoteEvidence::Deleted {
                change_seq: change_seq.parse().unwrap(),
            }
        }
        notryx_protocol::WorkspaceFrozenContentRemoteEvidence::TooLarge { change_seq, size } => {
            LocalRemoteEvidence::TooLarge {
                change_seq: change_seq.parse().unwrap(),
                size: size.parse().unwrap(),
            }
        }
        notryx_protocol::WorkspaceFrozenContentRemoteEvidence::Unknown => {
            LocalRemoteEvidence::Unknown
        }
    };
    Ok(LocalDismissReview {
        current: crate::from_contract(&inspected.current_binding),
        old: crate::from_contract(&inspected.old_binding),
        document_id: inspected.document_id.clone(),
        operation_id: inspected.operation_id.clone(),
        operation_seq: inspected.operation_seq.parse().unwrap(),
        operation_status,
        abandon_state,
        dependency: inspected.dependency.clone(),
        expected_revision: inspected.expected_revision.clone(),
        operation_generation: inspected.operation_generation.parse().unwrap(),
        operation_bytes: STANDARD.decode(&inspected.operation_bytes_base64).unwrap(),
        operation_hash: inspected.operation_hash.clone(),
        draft_generation: inspected.local_generation.parse().unwrap(),
        draft_bytes: STANDARD.decode(&inspected.local_bytes_base64).unwrap(),
        draft_hash: inspected.local_hash.clone(),
        base_bytes: STANDARD.decode(&inspected.base_bytes_base64).unwrap(),
        base_hash: inspected.base_hash.clone(),
        base_revision: inspected.base_revision.clone(),
        remote_evidence,
        inspected,
    })
}

async fn local_dismiss_for_renderer(
    runtime: &Runtime,
    owner: &str,
    review: LocalDismissReview,
) -> Result<DismissReceipt, String> {
    let mut inspected = review.inspected;
    inspected.dependency = review.dependency;
    let result = crate::frozen_content_local_native::dismiss_frozen_content_local_for_renderer(
        runtime,
        notryx_protocol::WorkspaceFrozenContentLocalDismissRequest {
            renderer_session_id: owner.into(),
            inspected,
        },
    )
    .await?;
    Ok(DismissReceipt {
        old_operation_id: result.operation_id,
        old_operation_seq: result.operation_seq,
    })
}

async fn local_review(rig: &Rig, operation_id: &str) -> Result<LocalDismissReview, String> {
    local_dismiss_review_for_renderer(
        &rig.runtime,
        "owner",
        &rig.current,
        &rig.old,
        &rig.document,
        operation_id,
    )
    .await
}

async fn delete_remote_with_peer(rig: &Rig) -> (NodeState, Session, NodeState) {
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: rig.server.local_addr(),
            admin_socket: rig._directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: rig._directory.path().join("notes"),
            state_root: rig._directory.path().join("server-state"),
            uploads: Default::default(),
        },
    };
    let pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", rig.server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let node = peer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.document)
        .unwrap();
    let receipt = peer
        .delete(&DeleteMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_node_etag: node.node_etag.clone(),
        })
        .await
        .unwrap();
    let OperationReceipt::Deleted { node: deleted, .. } = receipt else {
        panic!("synthetic deletion")
    };
    let snapshot = rig.runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_catchup::catch_up_metadata(&rig.runtime, &snapshot, &rig.session)
        .await
        .unwrap();
    (deleted, peer, node)
}

async fn saved_operation_evidence(rig: &Rig) -> Vec<(String, Vec<u8>, Option<String>)> {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(
                rig._directory
                    .path()
                    .join("frozen-content-inspection.sqlite"),
            )
            .read_only(true),
    )
    .await
    .unwrap();
    let rows = sqlx::query_as("SELECT operation_id, bytes, receipt_json FROM client_outbox ORDER BY registration_id, operation_seq")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

fn assert_local_review_sources(
    review: &LocalDismissReview,
    rig: &Rig,
    operation: &OutboxOperation,
) {
    assert_eq!(review.current, rig.current);
    assert_eq!(review.old, rig.old);
    assert_eq!(review.document_id, rig.document);
    assert_eq!(review.operation_id, operation.operation_id);
    assert_eq!(review.operation_seq, operation.operation_seq);
    assert_eq!(review.operation_status, operation.status);
    assert_eq!(review.abandon_state, operation.abandon_state);
    assert_eq!(review.expected_revision, operation.expected_revision);
    assert_eq!(review.operation_generation, operation.generation);
    assert_eq!(review.operation_bytes, operation.bytes);
    assert_eq!(review.operation_hash, content_hash(&operation.bytes));
    assert_eq!(review.draft_generation, 2);
    assert_eq!(review.draft_bytes, LOCAL);
    assert_eq!(review.draft_hash, content_hash(LOCAL));
    assert_eq!(review.base_bytes, BASE);
    assert_eq!(review.base_hash, content_hash(BASE));
    assert_eq!(review.base_revision, rig.base_revision);
}

#[tokio::test]
async fn frozen_content_local_dismiss_contract_deleted_remote_keeps_evidence_and_sources() {
    let (rig, principal) = editor_rig().await;
    let (tombstone, _peer, _previous) = delete_remote_with_peer(&rig).await;
    let before = command_sources(&rig).await;
    let operation_evidence = saved_operation_evidence(&rig).await;
    let watermarks = stream_watermarks(
        &rig._directory
            .path()
            .join("frozen-content-inspection.sqlite"),
    )
    .await;
    let review = local_review(&rig, &rig.sent.operation_id).await.unwrap();
    assert_local_review_sources(&review, &rig, &rig.sent);
    assert_eq!(
        review.remote_evidence,
        LocalRemoteEvidence::Deleted {
            change_seq: tombstone.last_change_seq.get()
        }
    );
    assert_eq!(review.dependency, None);
    change_role(&rig, &principal, WorkspaceRole::Reader).await;
    let result = local_dismiss_for_renderer(&rig.runtime, "owner", review.clone())
        .await
        .unwrap();
    assert_eq!(result.old_operation_id, rig.sent.operation_id);
    assert_eq!(result.old_operation_seq, rig.sent.operation_seq.to_string());
    assert!(command_sources(&rig).await == before);
    assert_eq!(saved_operation_evidence(&rig).await, operation_evidence);
    assert_eq!(
        stream_watermarks(
            &rig._directory
                .path()
                .join("frozen-content-inspection.sqlite")
        )
        .await,
        watermarks
    );
    assert_eq!(
        dismiss_status(&rig, "owner").await.unwrap(),
        Some(result.clone())
    );
    assert!(!rig._directory.path().join("notes/move.md").exists());
    let Rig {
        _directory: directory,
        server,
        runtime,
        session,
        old,
        current,
        document,
        ..
    } = rig;
    server.shutdown().await.unwrap();
    drop(runtime);
    drop(session);
    let store = ClientStore::open(directory.path().join("frozen-content-inspection.sqlite"))
        .await
        .unwrap();
    let runtime = Runtime(Arc::new(Mutex::new(RuntimeState::new(
        store,
        "new-owner".into(),
    ))));
    assert_eq!(
        local_dismiss_for_renderer(&runtime, "new-owner", review)
            .await
            .unwrap(),
        result
    );
    assert_eq!(
        dismissal_status_for_renderer(
            &runtime,
            "new-owner",
            &current,
            &old,
            &document,
            &result.old_operation_id
        )
        .await
        .unwrap(),
        Some(result)
    );
}

#[tokio::test]
async fn frozen_content_local_dismiss_contract_unobserved_oversized_remote_is_unknown() {
    let rig = single_operation_rig().await;
    let bytes = vec![b'x'; 25 * 1024 * 1024 + 1];
    std::fs::write(rig._directory.path().join("notes/move.md"), &bytes).unwrap();
    let before = command_sources(&rig).await;
    let review = local_review(&rig, &rig.sent.operation_id).await.unwrap();
    assert_local_review_sources(&review, &rig, &rig.sent);
    assert_eq!(review.remote_evidence, LocalRemoteEvidence::Unknown);
    let result = local_dismiss_for_renderer(&rig.runtime, "owner", review)
        .await
        .unwrap();
    assert_eq!(result.old_operation_id, rig.sent.operation_id);
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::metadata(rig._directory.path().join("notes/move.md"))
            .unwrap()
            .len(),
        bytes.len() as u64
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_local_dismiss_contract_offline_is_unknown_and_cannot_create_new_decision() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    {
        let mut state = rig.runtime.0.lock().await;
        state.session = None;
    }
    let review = local_review(&rig, &rig.sent.operation_id).await.unwrap();
    assert_local_review_sources(&review, &rig, &rig.sent);
    assert_eq!(review.remote_evidence, LocalRemoteEvidence::Unknown);
    assert_eq!(
        local_dismiss_for_renderer(&rig.runtime, "owner", review)
            .await
            .unwrap_err(),
        "not_connected"
    );
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
    assert!(command_sources(&rig).await == before);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_local_dismiss_contract_changed_remote_evidence_requires_review() {
    let rig = single_operation_rig().await;
    let (deleted, peer, original) = delete_remote_with_peer(&rig).await;
    let review = local_review(&rig, &rig.sent.operation_id).await.unwrap();
    assert!(matches!(
        review.remote_evidence,
        LocalRemoteEvidence::Deleted { .. }
    ));
    let receipt = peer
        .restore(&RestoreMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: original.node_id,
            request: RestoreNodeRequest {
                expected_meta_rev: deleted.meta_rev,
                parent_id: original.parent_id.unwrap(),
                name: original.name,
            },
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Restored { .. }));
    let snapshot = rig.runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_catchup::catch_up_metadata(&rig.runtime, &snapshot, &rig.session)
        .await
        .unwrap();
    let before = command_sources(&rig).await;
    let evidence = saved_operation_evidence(&rig).await;
    assert_eq!(
        local_dismiss_for_renderer(&rig.runtime, "owner", review)
            .await
            .unwrap_err(),
        "stale_inspection"
    );
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
    assert!(command_sources(&rig).await == before);
    assert_eq!(saved_operation_evidence(&rig).await, evidence);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_local_dismiss_contract_dependency_is_shown_and_rechecked() {
    let rig = rig().await;
    let before = command_sources(&rig).await;
    let evidence = saved_operation_evidence(&rig).await;
    assert_eq!(before.1.entries.len(), 2);
    let successor_id = before
        .1
        .entries
        .iter()
        .find(|entry| entry.operation_id != rig.sent.operation_id)
        .unwrap()
        .operation_id
        .clone();
    let review = local_review(&rig, &successor_id).await.unwrap();
    assert_eq!(review.operation_id, successor_id);
    assert_eq!(
        review.dependency.as_deref(),
        Some(rig.sent.operation_id.as_str())
    );
    assert_eq!(review.operation_bytes, LOCAL);
    let mut changed = review.clone();
    changed.dependency = None;
    let error = local_dismiss_for_renderer(&rig.runtime, "owner", changed)
        .await
        .unwrap_err();
    assert!(matches!(
        error.as_str(),
        "ReconciliationRequired" | "stale_inspection"
    ));
    assert!(command_sources(&rig).await == before);
    let result = local_dismiss_for_renderer(&rig.runtime, "owner", review)
        .await
        .unwrap();
    assert_eq!(result.old_operation_id, successor_id);
    assert!(command_sources(&rig).await == before);
    assert_eq!(saved_operation_evidence(&rig).await, evidence);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_local_dismiss_contract_stale_owner_and_checkpoint_are_rejected() {
    let rig = single_operation_rig().await;
    let review = local_review(&rig, &rig.sent.operation_id).await.unwrap();
    let before = command_sources(&rig).await;
    assert_eq!(
        local_dismiss_for_renderer(&rig.runtime, "other-renderer", review.clone())
            .await
            .unwrap_err(),
        "stale_renderer_session"
    );
    {
        let mut state = rig.runtime.0.lock().await;
        let edit = state
            .store
            .open_recovery_draft(&rig.old, &rig.document)
            .await
            .unwrap();
        state
            .store
            .save_recovery_checkpoint(
                &rig.old,
                &rig.document,
                &edit.edit_session_id,
                3,
                b"later local change\n",
            )
            .await
            .unwrap();
        state
            .store
            .close_recovery_draft(&rig.old, &rig.document, &edit.edit_session_id)
            .await
            .unwrap();
    }
    let changed = command_sources(&rig).await;
    assert_ne!(changed.0.bytes, before.0.bytes);
    let error = local_dismiss_for_renderer(&rig.runtime, "owner", review)
        .await
        .unwrap_err();
    assert!(matches!(
        error.as_str(),
        "GenerationMismatch" | "stale_inspection"
    ));
    assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
    assert!(command_sources(&rig).await == changed);
    rig.server.shutdown().await.unwrap();
}
