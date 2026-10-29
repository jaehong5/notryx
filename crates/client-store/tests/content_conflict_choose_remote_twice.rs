include!("content_conflict_intent.rs");

fn ack_through(binding: &RegistrationBinding, through: u64) -> notryx_protocol::AckResponse {
    serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": through.to_string(),
        "retired_through": through.to_string(),
    }))
    .unwrap()
}

#[tokio::test]
async fn later_remote_choice_keeps_earlier_completed_choice_valid() {
    let (rig, mut store) = rig().await;
    let mut first_request = rig.request.clone();
    first_request.choice = ContentConflictChoice::ChooseRemote;
    let first_intent = record(&mut store, &rig, &first_request).await.unwrap();
    let marked = store
        .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let abandoned = store
        .prepare_outbox_matching(&rig.binding, &marked)
        .await
        .unwrap()
        .unwrap();
    store
        .apply_outbox_abandoned(
            &rig.binding,
            &notryx_client_store::OutboxAbandoned {
                operation_id: abandoned.operation_id,
                operation_seq: abandoned.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&rig.binding, &ack_through(&rig.binding, 2))
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let first_result = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();

    let opened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    let local = b"second local edit\n";
    let generation = first_result.generation + 1;
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &opened.edit_session_id,
            generation,
            local,
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
        .apply_outbox_conflict(
            &rig.binding,
            &OutboxConflict {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                current_revision: "second-rev".into(),
                current_etag: "\"second-rev\"".into(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&rig.binding, &ack_through(&rig.binding, sent.operation_seq))
        .await
        .unwrap();

    let second_remote = b"second remote\n";
    let (_, original) = remote_nodes();
    let mut value = serde_json::to_value(original).unwrap();
    value["last_change_seq"] = json!("3");
    value["content_rev"] = json!("second-rev");
    value["content_etag"] = json!("\"second-rev\"");
    value["node_etag"] = json!("\"second-node\"");
    value["content_hash"] = json!(digest(second_remote));
    value["size"] = json!(second_remote.len().to_string());
    let remote: NodeState = serde_json::from_value(value).unwrap();
    let page = ChangesPage {
        workspace_epoch: info(&rig.binding).workspace_epoch,
        from_exclusive: ChangeSeq::new(2),
        through_seq: ChangeSeq::new(3),
        covered_through: ChangeSeq::new(3),
        has_more: false,
        nodes: vec![remote.clone()],
    };
    store
        .apply_changes(&rig.binding, &page, ChangeSeq::new(3))
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&rig.binding, &remote, second_remote)
            .await
            .unwrap()
    );
    let second_request = ContentConflictIntentRequest {
        generation,
        remote_revision: "second-rev".into(),
        remote_hash: digest(second_remote),
        remote_bytes: second_remote.to_vec(),
        choice: ContentConflictChoice::ChooseRemote,
    };
    let second_intent = store
        .record_content_conflict_intent(&rig.binding, DOC, &sent.operation_id, &second_request)
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &opened.edit_session_id)
        .await
        .unwrap();
    let second_result = store
        .complete_content_conflict_choose_remote(&rig.binding, &sent.operation_id)
        .await
        .unwrap();
    assert_eq!(second_result.generation, generation + 1);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, second_result.generation);
    assert_eq!(draft.bytes, second_remote);
    assert_eq!(draft.base_bytes, second_remote);
    assert_eq!(draft.base_revision, "second-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(first_intent)
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &sent.operation_id)
            .await
            .unwrap(),
        Some(second_intent)
    );
    let first_op = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let second_op = store
        .read_outbox_operation(&rig.binding, &sent.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_op.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(first_op.bytes, SENT);
    assert_eq!(first_op.conflict_revision.as_deref(), Some("peer-rev"));
    assert_eq!(
        second_op.status,
        notryx_client_store::OutboxStatus::Conflict
    );
    assert_eq!(second_op.bytes, local);
    assert_eq!(second_op.conflict_revision.as_deref(), Some("second-rev"));
    let retry = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await;
    let conflicts = store.first_content_conflict(&rig.binding, DOC).await;
    let state = store.document_outbox_state(&rig.binding, DOC).await;
    assert_eq!(retry.unwrap(), first_result);
    assert!(conflicts.unwrap().is_none());
    assert!(!state.unwrap().has_conflict);
}
