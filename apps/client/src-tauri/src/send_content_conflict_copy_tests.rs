include!("conflict_snapshot_server_tests.rs");

use notryx_client_runtime::CreateMutation;
use notryx_client_store::{
    ContentConflictChoice, ContentConflictIntentRequest, OutboxStatus, PendingOutboxOperation,
};
use notryx_protocol::WorkspaceSendStatus;

async fn queue_confirmed_copy(
    rig: &Rig,
    name: &str,
) -> (
    notryx_client_store::ContentConflictCopyIntent,
    notryx_client_store::CreateOutboxOperation,
) {
    let request = ContentConflictIntentRequest {
        generation: 2,
        remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(),
        choice: ContentConflictChoice::KeepLocalCopy,
    };
    let mut state = rig.runtime.0.lock().await;
    let copy = state
        .store
        .record_content_conflict_copy(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &request,
            &rig.initial.parent_id.unwrap().to_string(),
            name,
            None,
        )
        .await
        .unwrap();
    let queued = state
        .store
        .queue_content_conflict_copy(
            &rig.binding,
            &rig.operation.operation_id,
            copy.intent_version,
        )
        .await
        .unwrap();
    (copy, queued)
}

async fn preserve_original(rig: &Rig, copy: &notryx_client_store::ContentConflictCopyIntent) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    let conflict = state
        .store
        .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conflict.status, OutboxStatus::Conflict);
    assert_eq!(conflict.bytes, LOCAL);
    assert_eq!(
        state
            .store
            .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(copy.clone())
    );
    assert!(matches!(
        state
            .store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending {
            choice: notryx_client_store::ContentConflictChoiceKind::KeepLocalCopy,
            ..
        }
    ));
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
}

async fn advance_followup(rig: &Rig) {
    let blocked = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(blocked.status, WorkspaceSendStatus::NeedsReview);
    assert!(!rig.config.workspace.notes_root.join("copy.md").exists());
    let later = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .request_content_conflict_followup_abandon(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    let abandoned = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(abandoned.status, WorkspaceSendStatus::Abandoned);
    assert_eq!(
        abandoned.operation_id.as_deref(),
        Some(later.operation_id.as_str())
    );
}

#[tokio::test]
async fn copy_create_fifo_recovers_lost_receipt_without_reporting_copy_complete() {
    for lost in [false, true] {
        let rig = rig_with_followup(true).await;
        let (copy, queued) = queue_confirmed_copy(&rig, "copy.md").await;
        preserve_original(&rig, &copy).await;
        advance_followup(&rig).await;
        let head = if lost {
            let prepared = rig
                .runtime
                .0
                .lock()
                .await
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap();
            let PendingOutboxOperation::Create(prepared) = prepared else {
                panic!("expected create")
            };
            assert_eq!(prepared.operation_id, queued.operation_id);
            let receipt = rig
                .session
                .create(&CreateMutation {
                    binding: rig.session.binding().clone(),
                    operation_id: queued.operation_id.parse().unwrap(),
                    operation_seq: queued.operation_seq,
                    request: queued.request.clone(),
                })
                .await
                .unwrap();
            assert!(matches!(receipt, OperationReceipt::Created { .. }));
            Some(rig.session.server_info().await.unwrap().head_seq)
        } else {
            None
        };
        let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(queued.operation_id.as_str())
        );
        if let Some(head) = head {
            assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
        }
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("copy.md")).unwrap(),
            b""
        );
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            queued.operation_seq
        );
        let entries = state
            .store
            .list_create_operations(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].operation.operation_id, copy.create_operation_id);
        assert_eq!(entries[0].operation.status, OutboxStatus::Committed);
        assert_eq!(
            state
                .store
                .queue_content_conflict_copy(
                    &rig.binding,
                    &rig.operation.operation_id,
                    copy.intent_version
                )
                .await
                .unwrap(),
            entries[0].operation
        );
        drop(state);
        preserve_original(&rig, &copy).await;
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn copy_destination_conflict_preserves_file_and_terminal_receipt_on_retry() {
    let rig = rig_with_followup(true).await;
    let (copy, queued) = queue_confirmed_copy(&rig, "example.md").await;
    advance_followup(&rig).await;
    let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Failed);
    assert_eq!(
        result.operation_id.as_deref(),
        Some(queued.operation_id.as_str())
    );
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap(),
        queued.operation_seq
    );
    let entries = state
        .store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation.status, OutboxStatus::Failed);
    assert_eq!(
        state
            .store
            .queue_content_conflict_copy(
                &rig.binding,
                &rig.operation.operation_id,
                copy.intent_version
            )
            .await
            .unwrap(),
        entries[0].operation
    );
    drop(state);
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
