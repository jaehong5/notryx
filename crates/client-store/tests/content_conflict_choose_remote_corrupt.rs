include!("content_conflict_intent.rs");

fn ack_at(binding: &RegistrationBinding, through: u64) -> notryx_protocol::AckResponse {
    serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": through.to_string(),
        "retired_through": through.to_string(),
    }))
    .unwrap()
}

async fn confirmed_choice(store: &mut ClientStore, rig: &Rig) -> ContentConflictIntent {
    let mut request = rig.request.clone();
    request.choice = ContentConflictChoice::ChooseRemote;
    let intent = record(store, rig, &request).await.unwrap();
    let marked = store
        .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let sent = store
        .prepare_outbox_matching(&rig.binding, &marked)
        .await
        .unwrap()
        .unwrap();
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
    store
        .confirm_receipt_ack(&rig.binding, &ack_at(&rig.binding, 2))
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    intent
}

#[tokio::test]
async fn forged_completion_marker_cannot_hide_unapplied_choice() {
    for through in [1_i64, 2] {
        let (rig, mut store) = rig().await;
        let intent = confirmed_choice(&mut store, &rig).await;
        let before = store
            .read_registered_checkpoint(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap();
        let failed = store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap();
        let followup = store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap();
        let mut db = connect(&rig.path).await;
        sqlx::query("UPDATE content_conflict_intents SET completed_generation=?, completed_through=? WHERE operation_id=?")
            .bind(i64::try_from(intent.generation + 1).unwrap()).bind(through).bind(&rig.operation)
            .execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let completion = store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await;
        let conflict = store.first_content_conflict(&rig.binding, DOC).await;
        let state = store.document_outbox_state(&rig.binding, DOC).await;
        let after = store
            .read_registered_checkpoint(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.bytes, before.bytes);
        assert_eq!(after.base_bytes, before.base_bytes);
        assert_eq!(after.base_revision, before.base_revision);
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(intent)
        );
        let failed_after = store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap();
        let followup_after = store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed_after.operation_id, failed.operation_id);
        assert_eq!(failed_after.operation_seq, failed.operation_seq);
        assert_eq!(failed_after.status, failed.status);
        assert_eq!(failed_after.bytes, failed.bytes);
        assert_eq!(followup_after.operation_id, followup.operation_id);
        assert_eq!(followup_after.operation_seq, followup.operation_seq);
        assert_eq!(followup_after.status, followup.status);
        assert_eq!(followup_after.bytes, followup.bytes);
        assert!(
            completion.is_err() && conflict.is_err() && state.is_err(),
            "through={through}, completion_error={}, conflict_error={}, state_error={}",
            completion.is_err(),
            conflict.is_err(),
            state.is_err()
        );
    }
}

#[tokio::test]
async fn completed_choice_remains_valid_after_later_edit_commit_and_ack() {
    let (rig, mut store) = rig().await;
    let intent = confirmed_choice(&mut store, &rig).await;
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    let edited = b"later edit\n";
    let generation = selected.generation + 1;
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &opened.edit_session_id,
            generation,
            edited,
        )
        .await
        .unwrap();
    let queued = store
        .queue_checkpoint(&rig.binding, DOC, generation)
        .await
        .unwrap();
    assert_eq!(queued.expected_revision.as_deref(), Some("current-rev"));
    let sent = store
        .prepare_outbox_matching(&rig.binding, &queued)
        .await
        .unwrap()
        .unwrap();
    store
        .apply_outbox_commit(
            &rig.binding,
            &notryx_client_store::OutboxCommit {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                document_id: sent.document_id.clone(),
                content_revision: "committed-rev".into(),
                content_hash: sent.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&rig.binding, &ack_at(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &opened.edit_session_id)
        .await
        .unwrap();
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
    assert_eq!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        selected
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, generation);
    assert_eq!(draft.bytes, edited);
    assert_eq!(draft.base_bytes, edited);
    assert_eq!(draft.base_revision, "committed-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn truncated_completion_through_cannot_hide_confirmed_followup() {
    let (rig, mut store) = rig().await;
    let intent = confirmed_choice(&mut store, &rig).await;
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    let before = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    let failed = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let followup = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.generation, before.generation);
    assert_eq!(before.bytes, REMOTE);
    assert_eq!(before.base_bytes, REMOTE);
    assert_eq!(before.base_revision, "current-rev");
    assert_eq!(
        followup.status,
        notryx_client_store::OutboxStatus::Abandoned
    );
    assert!(followup.operation_seq > failed.operation_seq);
    let mut db = connect(&rig.path).await;
    sqlx::query("UPDATE content_conflict_intents SET completed_through=? WHERE operation_id=?")
        .bind(i64::try_from(failed.operation_seq).unwrap())
        .bind(&rig.operation)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let completion = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await;
    let conflict = store.first_content_conflict(&rig.binding, DOC).await;
    let state = store.document_outbox_state(&rig.binding, DOC).await;
    let after = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.generation, before.generation);
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.base_bytes, before.base_bytes);
    assert_eq!(after.base_revision, before.base_revision);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    let failed_after = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let followup_after = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed_after.operation_seq, failed.operation_seq);
    assert_eq!(failed_after.status, failed.status);
    assert_eq!(failed_after.bytes, failed.bytes);
    assert_eq!(followup_after.operation_seq, followup.operation_seq);
    assert_eq!(followup_after.status, followup.status);
    assert_eq!(followup_after.bytes, followup.bytes);
    assert!(
        completion.is_err() && conflict.is_err() && state.is_err(),
        "completion_error={}, conflict_error={}, state_error={}",
        completion.is_err(),
        conflict.is_err(),
        state.is_err()
    );
}
