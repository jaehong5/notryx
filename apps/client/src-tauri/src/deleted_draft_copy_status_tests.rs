include!("deleted_draft_copy_recovery_tests.rs");
use notryx_client_store::{ContentConflictCopyContentState, DeletedDraftCopyCreateState};

#[tokio::test]
async fn deleted_copy_status_tracks_create_content_and_confirmed_ack() {
    let rig = deleted_copy_rig().await;
    let mut state = rig.runtime.0.lock().await;
    let status = state
        .store
        .read_deleted_draft_copy_status(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(status.create, DeletedDraftCopyCreateState::NotQueued);
    assert_eq!(status.content, ContentConflictCopyContentState::NotQueued);
    let create = state
        .store
        .queue_deleted_draft_copy(&rig.binding, &rig.document)
        .await
        .unwrap();
    let status = state
        .store
        .read_deleted_draft_copy_status(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(status.create, DeletedDraftCopyCreateState::Pending);
    assert_eq!(status.content, ContentConflictCopyContentState::NotQueued);
    assert!(matches!(
        state
            .store
            .prepare_pending_outbox(&rig.binding)
            .await
            .unwrap(),
        Some(PendingOutboxOperation::Create(_))
    ));
    drop(state);
    let receipt = rig
        .session_a
        .create(&CreateMutation {
            binding: rig.session_a.binding().clone(),
            operation_id: create.operation_id.parse().unwrap(),
            operation_seq: create.operation_seq,
            request: create.request.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Created { .. }));
    let mut state = rig.runtime.0.lock().await;
    state
        .store
        .apply_create_receipt(&rig.binding, &receipt)
        .await
        .unwrap();
    let status = state
        .store
        .read_deleted_draft_copy_status(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(status.create, DeletedDraftCopyCreateState::Pending);
    assert_eq!(status.content, ContentConflictCopyContentState::NotQueued);
    drop(state);
    let ack = rig
        .session_a
        .acknowledge(create.operation_seq)
        .await
        .unwrap();
    let mut state = rig.runtime.0.lock().await;
    state
        .store
        .confirm_receipt_ack(&rig.binding, &ack)
        .await
        .unwrap();
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy_status(&rig.binding, &rig.document)
            .await
            .unwrap()
            .create,
        DeletedDraftCopyCreateState::Created
    );
    let content = state
        .store
        .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy_status(&rig.binding, &rig.document)
            .await
            .unwrap()
            .content,
        ContentConflictCopyContentState::Pending {
            operation_id: content.operation_id.clone()
        }
    );
    assert!(matches!(
        state
            .store
            .prepare_pending_outbox(&rig.binding)
            .await
            .unwrap(),
        Some(PendingOutboxOperation::Content(_))
    ));
    drop(state);
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
        panic!("expected commit")
    };
    let mut state = rig.runtime.0.lock().await;
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
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy_status(&rig.binding, &rig.document)
            .await
            .unwrap()
            .content,
        ContentConflictCopyContentState::Pending {
            operation_id: content.operation_id.clone()
        }
    );
    drop(state);
    let rig = restart(rig).await;
    let ack = rig
        .session_a
        .acknowledge(content.operation_seq)
        .await
        .unwrap();
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy_status(&rig.binding, &rig.document)
            .await
            .unwrap()
            .content,
        ContentConflictCopyContentState::Pending {
            operation_id: content.operation_id.clone()
        }
    );
    state
        .store
        .confirm_receipt_ack(&rig.binding, &ack)
        .await
        .unwrap();
    let status = state
        .store
        .read_deleted_draft_copy_status(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(status.create, DeletedDraftCopyCreateState::Created);
    assert_eq!(
        status.content,
        ContentConflictCopyContentState::Saved {
            operation_id: content.operation_id
        }
    );
    drop(state);
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_status_reports_name_collision_as_needing_review() {
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
                parent_id: create.request.parent_id,
                name: create.request.name.clone(),
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
    let rig = restart(rig).await;
    let status = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_deleted_draft_copy_status(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(
        status.create,
        DeletedDraftCopyCreateState::NeedsReview {
            destination_exists: true
        }
    );
    assert_eq!(status.content, ContentConflictCopyContentState::NotQueued);
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_status_reports_content_conflict_as_needing_review() {
    let rig = deleted_copy_rig().await;
    rig.runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::Committed
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
    let node = rig
        .session_b
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.intent.node_id)
        .unwrap();
    assert!(matches!(
        rig.session_b
            .put_content(&ContentMutation {
                binding: rig.session_b.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 2,
                node_id: node.node_id,
                expected_revision: node.content_rev.unwrap(),
                bytes: b"peer copy content\n".to_vec()
            })
            .await
            .unwrap(),
        OperationReceipt::Committed { .. }
    ));
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::Conflict
    );
    let rig = restart(rig).await;
    let status = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_deleted_draft_copy_status(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(status.create, DeletedDraftCopyCreateState::Created);
    assert_eq!(
        status.content,
        ContentConflictCopyContentState::NeedsReview {
            operation_id: content.operation_id
        }
    );
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_status_waits_for_confirmed_ack_before_exposing_create_failure() {
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
    let peer = rig
        .session_b
        .create(&CreateMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            request: CreateNodeRequest {
                node_id: Uuid::now_v7().to_string().parse().unwrap(),
                parent_id: create.request.parent_id,
                name: create.request.name.clone(),
                kind: NodeKind::File,
                upload_id: None,
            },
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Created { .. }));
    let receipt = rig
        .session_a
        .create(&CreateMutation {
            binding: rig.session_a.binding().clone(),
            operation_id: create.operation_id.parse().unwrap(),
            operation_seq: create.operation_seq,
            request: create.request.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        receipt,
        OperationReceipt::DestinationExists { .. }
    ));
    {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .apply_create_receipt(&rig.binding, &receipt)
            .await
            .unwrap();
        let status = state
            .store
            .read_deleted_draft_copy_status(&rig.binding, &rig.document)
            .await
            .unwrap();
        assert_eq!(status.create, DeletedDraftCopyCreateState::Pending);
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &create.operation_id,
                    &rig.intent.parent_id,
                    "second-copy.md"
                )
                .await
                .is_err()
        );
    }
    let rig = restart(rig).await;
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy_status(&rig.binding, &rig.document)
            .await
            .unwrap()
            .create,
        DeletedDraftCopyCreateState::Pending
    );
    drop(state);
    let ack = rig
        .session_a
        .acknowledge(create.operation_seq)
        .await
        .unwrap();
    let mut state = rig.runtime.0.lock().await;
    state
        .store
        .confirm_receipt_ack(&rig.binding, &ack)
        .await
        .unwrap();
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy_status(&rig.binding, &rig.document)
            .await
            .unwrap()
            .create,
        DeletedDraftCopyCreateState::NeedsReview {
            destination_exists: true
        }
    );
    assert!(
        state
            .store
            .reconfirm_failed_deleted_draft_copy(
                &rig.binding,
                &rig.document,
                &create.operation_id,
                &rig.intent.parent_id,
                "second-copy.md"
            )
            .await
            .is_ok()
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}
