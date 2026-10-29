include!("deleted_draft_copy.rs");

#[tokio::test]
async fn newer_queued_followup_needs_exact_id_seq_generation_hash_and_checkpoint() {
    let mut f = fixture(true, false).await;
    f.store
        .save_registered_checkpoint(
            &f.binding,
            DOCUMENT,
            &f.edit_session,
            2,
            b"confirmed copy\n",
        )
        .await
        .unwrap();
    let initial = f
        .store
        .queue_checkpoint(&f.binding, DOCUMENT, 2)
        .await
        .unwrap();
    let mut selection = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    selection.generation = 2;
    selection.local_bytes = b"confirmed copy\n".to_vec();
    let intent = f
        .store
        .record_deleted_draft_copy(&f.binding, DOCUMENT, &selection)
        .await
        .unwrap();
    f.store
        .apply_outbox_failure(
            &f.binding,
            &OutboxFailure {
                operation_id: f.operation.operation_id.clone(),
                operation_seq: f.operation.operation_seq,
                workspace_epoch: f.binding.workspace_epoch.clone(),
                kind: OutboxFailureKind::NodeDeleted,
            },
        )
        .await
        .unwrap();
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({
        "workspace_epoch": f.binding.workspace_epoch, "device_stream_id": f.binding.device_stream_id,
        "last_seq": f.operation.operation_seq.to_string(), "retired_through": f.operation.operation_seq.to_string()
    })).unwrap();
    f.store.confirm_receipt_ack(&f.binding, &ack).await.unwrap();
    f.store
        .save_registered_checkpoint(
            &f.binding,
            DOCUMENT,
            &f.edit_session,
            3,
            b"newer queued edit\n",
        )
        .await
        .unwrap();
    let current = f
        .store
        .queue_checkpoint(&f.binding, DOCUMENT, 3)
        .await
        .unwrap();
    assert_eq!(current.operation_id, initial.operation_id);
    assert_eq!(current.operation_seq, initial.operation_seq);
    assert_eq!(current.generation, 3);
    assert_ne!(current.payload_hash, initial.payload_hash);
    let shown = f
        .store
        .read_deleted_draft_copy_blocking_followup(&f.binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown.blocker_operation_id, f.operation.operation_id);
    assert_eq!(shown.followup_operation_id, current.operation_id);
    assert_eq!(shown.followup_operation_seq, current.operation_seq);
    assert_eq!(shown.followup_generation, 3);
    assert_eq!(shown.followup_hash, current.payload_hash);
    assert!(
        f.store
            .request_deleted_draft_copy_followup_abandon(
                &f.binding,
                DOCUMENT,
                &shown.blocker_operation_id,
                &shown.followup_operation_id,
                shown.followup_operation_seq + 1,
                shown.followup_generation,
                &shown.followup_hash
            )
            .await
            .is_err()
    );
    assert!(
        f.store
            .request_deleted_draft_copy_followup_abandon(
                &f.binding,
                DOCUMENT,
                &shown.blocker_operation_id,
                &shown.followup_operation_id,
                shown.followup_operation_seq,
                shown.followup_generation,
                &initial.payload_hash
            )
            .await
            .is_err()
    );
    f.store
        .save_registered_checkpoint(
            &f.binding,
            DOCUMENT,
            &f.edit_session,
            4,
            b"newer checkpoint not yet queued\n",
        )
        .await
        .unwrap();
    assert!(
        f.store
            .request_deleted_draft_copy_followup_abandon(
                &f.binding,
                DOCUMENT,
                &shown.blocker_operation_id,
                &shown.followup_operation_id,
                shown.followup_operation_seq,
                shown.followup_generation,
                &shown.followup_hash
            )
            .await
            .is_err()
    );
    let updated = f
        .store
        .queue_checkpoint(&f.binding, DOCUMENT, 4)
        .await
        .unwrap();
    assert_eq!(updated.operation_id, current.operation_id);
    assert_eq!(updated.operation_seq, current.operation_seq);
    assert!(
        f.store
            .request_deleted_draft_copy_followup_abandon(
                &f.binding,
                DOCUMENT,
                &shown.blocker_operation_id,
                &shown.followup_operation_id,
                shown.followup_operation_seq,
                shown.followup_generation,
                &shown.followup_hash
            )
            .await
            .is_err()
    );
    let shown = f
        .store
        .read_deleted_draft_copy_blocking_followup(&f.binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown.followup_generation, 4);
    assert_eq!(shown.followup_hash, updated.payload_hash);
    let approved = f
        .store
        .request_deleted_draft_copy_followup_abandon(
            &f.binding,
            DOCUMENT,
            &shown.blocker_operation_id,
            &shown.followup_operation_id,
            shown.followup_operation_seq,
            shown.followup_generation,
            &shown.followup_hash,
        )
        .await
        .unwrap();
    assert_eq!(approved.operation_id, updated.operation_id);
    assert_eq!(approved.bytes, b"newer checkpoint not yet queued\n");
    assert_eq!(
        approved.abandon_state,
        notryx_client_store::OutboxAbandonState::Requested
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    let draft = f
        .store
        .read_registered_checkpoint(&f.binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 4);
    assert_eq!(draft.bytes, b"newer checkpoint not yet queued\n");
    let sent = f.store.prepare_outbox(&f.binding).await.unwrap().unwrap();
    assert_eq!(
        sent.abandon_state,
        notryx_client_store::OutboxAbandonState::Sent
    );
    f.store
        .apply_outbox_abandoned(
            &f.binding,
            &notryx_client_store::OutboxAbandoned {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: f.binding.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({
        "workspace_epoch": f.binding.workspace_epoch, "device_stream_id": f.binding.device_stream_id,
        "last_seq": sent.operation_seq.to_string(), "retired_through": sent.operation_seq.to_string()
    })).unwrap();
    f.store.confirm_receipt_ack(&f.binding, &ack).await.unwrap();
    let retry = f
        .store
        .request_deleted_draft_copy_followup_abandon(
            &f.binding,
            DOCUMENT,
            &shown.blocker_operation_id,
            &shown.followup_operation_id,
            shown.followup_operation_seq,
            shown.followup_generation,
            &shown.followup_hash,
        )
        .await
        .unwrap();
    assert_eq!(retry.status, OutboxStatus::Abandoned);
    assert_eq!(retry.bytes, approved.bytes);
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_eq!(
        f.store
            .read_registered_checkpoint(&f.binding, DOCUMENT)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"newer checkpoint not yet queued\n"
    );
}
