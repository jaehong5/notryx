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
async fn resolved_conflict_does_not_block_newer_remote_cache_after_clean_commit() {
    let (rig, mut store) = rig().await;
    let mut request = rig.request.clone();
    request.choice = ContentConflictChoice::ChooseRemote;
    let intent = record(&mut store, &rig, &request).await.unwrap();
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
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();

    let opened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    let local = b"committed local edit\n";
    let generation = selected.generation + 1;
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
        .confirm_receipt_ack(&rig.binding, &ack_through(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &opened.edit_session_id)
        .await
        .unwrap();

    let later_remote = b"later remote edit\n";
    let (_, original) = remote_nodes();
    let mut value = serde_json::to_value(original).unwrap();
    value["last_change_seq"] = json!("3");
    value["content_rev"] = json!("later-rev");
    value["content_etag"] = json!("\"later-rev\"");
    value["node_etag"] = json!("\"later-node\"");
    value["content_hash"] = json!(digest(later_remote));
    value["size"] = json!(later_remote.len().to_string());
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
            .cache_content(&rig.binding, &remote, later_remote)
            .await
            .unwrap()
    );
    assert!(
        store
            .remote_content_matches(&rig.binding, &remote)
            .await
            .unwrap()
    );
    let cached = store
        .read_cached_content(&rig.binding, remote.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(cached.current);
    assert_eq!(cached.content_rev, "later-rev");
    assert_eq!(cached.bytes, later_remote);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, generation);
    assert_eq!(draft.bytes, local);
    assert_eq!(draft.base_bytes, local);
    assert_eq!(draft.base_revision, "committed-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
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
            .has_confirmed_older_checkpoint(&rig.binding, DOC, "later-rev")
            .await
            .unwrap()
    );
}
