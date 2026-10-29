include!("content_conflict_intent.rs");

fn ack_at(binding: &RegistrationBinding, through: u64) -> notryx_protocol::AckResponse {
    serde_json::from_value(json!({"workspace_epoch":binding.workspace_epoch, "device_stream_id":binding.device_stream_id,
        "last_seq":through.to_string(), "retired_through":through.to_string()})).unwrap()
}

async fn prepare_choose_remote(store: &mut ClientStore, rig: &Rig) -> ContentConflictIntent {
    let mut request = rig.request.clone();
    request.choice = ContentConflictChoice::ChooseRemote;
    record(store, rig, &request).await.unwrap()
}

async fn abandon_followup_without_confirmation(store: &mut ClientStore, rig: &Rig) {
    let intent = store
        .read_content_conflict_intent(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let marked = if matches!(&intent.choice, ContentConflictChoice::SaveMerged { .. }) {
        store
            .confirm_receipt_ack(&rig.binding, &ack_at(&rig.binding, 1))
            .await
            .unwrap();
        let shown = store
            .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
            .await
            .unwrap()
            .unwrap();
        store
            .request_content_conflict_merge_followup(
                &rig.binding,
                &rig.operation,
                intent.version,
                &shown.operation_id,
                shown.operation_seq,
                shown.generation,
                &shown.payload_hash,
            )
            .await
            .unwrap()
    } else {
        store
            .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(marked.operation_id, rig.followup);
    let sent = store
        .prepare_outbox_matching(&rig.binding, &marked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        sent.abandon_state,
        notryx_client_store::OutboxAbandonState::Sent
    );
    store
        .apply_outbox_abandoned(
            &rig.binding,
            &notryx_client_store::OutboxAbandoned {
                operation_id: sent.operation_id,
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 2);
}

async fn assert_conflict_payload_and_intent(
    store: &mut ClientStore,
    rig: &Rig,
    intent: &ContentConflictIntent,
) {
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    let failed = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.bytes, SENT);
    assert_eq!(failed.expected_revision.as_deref(), Some("base-rev"));
    assert_eq!(failed.conflict_revision.as_deref(), Some("peer-rev"));
    let followup = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(followup.bytes, LOCAL);
}

#[tokio::test]
async fn choose_remote_waits_for_closed_session_and_verified_followup_receipt() {
    let (rig, mut store) = rig().await;
    let intent = prepare_choose_remote(&mut store, &rig).await;
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    assert_originals(&mut store, &rig).await;
    let marked = store
        .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    let sent = store
        .prepare_outbox_matching(&rig.binding, &marked)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    store
        .apply_outbox_abandoned(
            &rig.binding,
            &notryx_client_store::OutboxAbandoned {
                operation_id: sent.operation_id,
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    store
        .confirm_receipt_ack(&rig.binding, &ack_at(&rig.binding, 2))
        .await
        .unwrap();
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err(),
        "an active editor still owns the old base"
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_conflict_payload_and_intent(&mut store, &rig, &intent).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    assert_eq!(selected.binding, rig.binding);
    assert_eq!(selected.document_id, DOC);
    assert_eq!(selected.operation_id, rig.operation);
    assert_eq!(selected.remote_revision, "current-rev");
    assert_eq!(selected.remote_bytes, REMOTE);
    assert!(selected.generation > 2);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, selected.generation);
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, "current-rev");
    assert_conflict_payload_and_intent(&mut store, &rig, &intent).await;
    assert!(
        store
            .first_content_conflict(&rig.binding, DOC)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .document_outbox_state(&rig.binding, DOC)
            .await
            .unwrap()
            .has_conflict
    );
    assert!(
        store
            .next_pending_outbox(&rig.binding)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 2);
    assert_eq!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        selected
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        selected
    );
    let reopened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    assert_eq!(reopened.generation, selected.generation);
    assert_eq!(reopened.bytes, REMOTE);
    assert!(
        store
            .save_registered_checkpoint(&rig.binding, DOC, &reopened.edit_session_id, 2, LOCAL)
            .await
            .is_err()
    );
    let next_generation = selected.generation + 1;
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &reopened.edit_session_id,
            next_generation,
            b"\xef\xbb\xbfafter edit\n",
        )
        .await
        .unwrap();
    let next = store
        .queue_checkpoint(&rig.binding, DOC, next_generation)
        .await
        .unwrap();
    assert_eq!(next.operation_seq, 3);
    assert_ne!(next.operation_id, rig.operation);
    assert_eq!(next.expected_revision.as_deref(), Some("current-rev"));
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, "current-rev");
    assert_eq!(draft.bytes, b"\xef\xbb\xbfafter edit\n");
    assert_conflict_payload_and_intent(&mut store, &rig, &intent).await;
}

#[tokio::test]
async fn choose_remote_rejects_missing_intent_and_other_choices() {
    let (first_rig, mut store) = rig().await;
    store
        .close_registered_document(&first_rig.binding, DOC, &first_rig.session)
        .await
        .unwrap();
    assert!(
        store
            .complete_content_conflict_choose_remote(&first_rig.binding, &first_rig.operation)
            .await
            .is_err()
    );
    assert_originals(&mut store, &first_rig).await;
    for choice in [
        ContentConflictChoice::KeepLocalCopy,
        ContentConflictChoice::SaveMerged {
            bytes: b"merged\n".to_vec(),
        },
    ] {
        let (rig, mut store) = rig().await;
        let mut request = rig.request.clone();
        request.choice = choice;
        let intent = record(&mut store, &rig, &request).await.unwrap();
        abandon_followup_without_confirmation(&mut store, &rig).await;
        store
            .confirm_receipt_ack(&rig.binding, &ack_at(&rig.binding, 2))
            .await
            .unwrap();
        store
            .close_registered_document(&rig.binding, DOC, &rig.session)
            .await
            .unwrap();
        assert!(
            store
                .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
                .await
                .is_err()
        );
        let draft = store
            .read_registered_checkpoint(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, LOCAL);
        assert_eq!(draft.base_bytes, BASE);
        assert_conflict_payload_and_intent(&mut store, &rig, &intent).await;
    }
}

#[tokio::test]
async fn choose_remote_rejects_newer_local_checkpoint_after_confirmation() {
    let (rig, mut store) = rig().await;
    let intent = prepare_choose_remote(&mut store, &rig).await;
    abandon_followup_without_confirmation(&mut store, &rig).await;
    store
        .confirm_receipt_ack(&rig.binding, &ack_at(&rig.binding, 2))
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"newer local\n")
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    assert_conflict_payload_and_intent(&mut store, &rig, &intent).await;
}

#[tokio::test]
async fn choose_remote_transaction_failure_preserves_everything_and_can_retry_after_restart() {
    let (rig, mut store) = rig().await;
    let intent = prepare_choose_remote(&mut store, &rig).await;
    abandon_followup_without_confirmation(&mut store, &rig).await;
    store
        .confirm_receipt_ack(&rig.binding, &ack_at(&rig.binding, 2))
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_conflict_completion BEFORE UPDATE OF completed_generation ON content_conflict_intents BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await,
        Err(StoreError::Storage)
    ));
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 2);
    assert_conflict_payload_and_intent(&mut store, &rig, &intent).await;
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::query("DROP TRIGGER reject_conflict_completion")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    assert_eq!(selected.remote_bytes, REMOTE);
    assert!(selected.generation > 2);
    assert_conflict_payload_and_intent(&mut store, &rig, &intent).await;
}
