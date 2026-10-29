include!("content_conflict_intent.rs");

async fn confirm_original_conflict(store: &mut ClientStore, rig: &Rig) {
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({
        "workspace_epoch":rig.binding.workspace_epoch,
        "device_stream_id":rig.binding.device_stream_id,
        "last_seq":"1", "retired_through":"1"
    }))
    .unwrap();
    store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
}

async fn request_selected_merge_followup(
    store: &mut ClientStore,
    rig: &Rig,
    intent_version: u64,
    selected: &notryx_client_store::OutboxOperation,
) -> Result<notryx_client_store::OutboxOperation, StoreError> {
    store
        .request_content_conflict_merge_followup(
            &rig.binding,
            &rig.operation,
            intent_version,
            &selected.operation_id,
            selected.operation_seq,
            selected.generation,
            &selected.payload_hash,
        )
        .await
}

#[tokio::test]
async fn confirmed_intent_requests_one_existing_followup_abandon_and_survives_restart() {
    use notryx_client_store::{
        OutboxAbandonState, OutboxAbandoned, OutboxStatus, PendingOutboxOperation,
    };
    let (rig, mut store) = rig().await;
    let original = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert!(
        request_selected_merge_followup(&mut store, &rig, 0, &original)
            .await
            .is_err()
    );
    assert_originals(&mut store, &rig).await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirm_original_conflict(&mut store, &rig).await;
    let shown = store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown.operation_id, original.operation_id);
    assert_eq!(shown.operation_seq, original.operation_seq);
    assert_eq!(shown.generation, original.generation);
    assert_eq!(shown.payload_hash, original.payload_hash);
    let requested = request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
        .await
        .unwrap();
    assert_eq!(requested.operation_id, original.operation_id);
    assert_eq!(requested.operation_seq, original.operation_seq);
    assert_eq!(requested.generation, original.generation);
    assert_eq!(requested.bytes, original.bytes);
    assert_eq!(requested.payload_hash, original.payload_hash);
    assert_eq!(requested.expected_revision, original.expected_revision);
    assert_eq!(requested.status, OutboxStatus::Abandoning);
    assert_eq!(requested.abandon_state, OutboxAbandonState::Requested);
    assert_eq!(
        request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
            .await
            .unwrap(),
        requested
    );
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&rig.path).await.unwrap();
    let reopened = request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
        .await
        .unwrap();
    assert_eq!(reopened, requested);
    let Some(PendingOutboxOperation::Content(sent)) =
        store.prepare_pending_outbox(&rig.binding).await.unwrap()
    else {
        panic!("abandon must remain FIFO head")
    };
    assert_eq!(sent.operation_id, original.operation_id);
    assert_eq!(sent.operation_seq, original.operation_seq);
    assert_eq!(sent.bytes, original.bytes);
    assert_eq!(sent.abandon_state, OutboxAbandonState::Sent);
    let retry = request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
        .await
        .unwrap();
    assert_eq!(retry.operation_id, sent.operation_id);
    assert_eq!(retry.operation_seq, sent.operation_seq);
    assert_eq!(retry.bytes, sent.bytes);
    store
        .apply_outbox_abandoned(
            &rig.binding,
            &OutboxAbandoned {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    assert!(
        request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
            .await
            .is_err()
    );
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 2);
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({
        "workspace_epoch":rig.binding.workspace_epoch, "device_stream_id":rig.binding.device_stream_id,
        "last_seq":"2", "retired_through":"2"
    })).unwrap();
    store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    let terminal = request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
        .await
        .unwrap();
    assert_eq!(terminal.operation_id, shown.operation_id);
    assert_eq!(terminal.status, OutboxStatus::Abandoned);
    let persisted = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.status, OutboxStatus::Abandoned);
    assert_eq!(persisted.bytes, LOCAL);
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, "base-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    assert!(
        store
            .next_pending_outbox(&rig.binding)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn newer_checkpoint_after_intent_is_not_included_or_abandoned_implicitly() {
    use notryx_client_store::OutboxAbandonState;
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirm_original_conflict(&mut store, &rig).await;
    let shown = store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap()
        .unwrap();
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"newer local input\n")
        .await
        .unwrap();
    assert!(
        request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
            .await
            .is_err()
    );
    let pending = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.abandon_state, OutboxAbandonState::None);
    assert_eq!(pending.bytes, LOCAL);
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local input\n");
    assert_eq!(draft.generation, 3);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
}

#[tokio::test]
async fn change_or_deletion_of_confirmed_remote_blocks_abandon_and_preserves_everything() {
    use notryx_client_store::OutboxAbandonState;
    for deleted in [false, true] {
        let (rig, mut store) = rig().await;
        let intent = record(&mut store, &rig, &rig.request).await.unwrap();
        confirm_original_conflict(&mut store, &rig).await;
        let shown = store
            .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
            .await
            .unwrap()
            .unwrap();
        let (mut root, original) = remote_nodes();
        let mut value = serde_json::to_value(original).unwrap();
        value["last_change_seq"] = json!("3");
        let nodes = if deleted {
            value["deleted"] = json!(true);
            value["content_rev"] = json!("deleted-content-rev");
            value["content_etag"] = json!("\"deleted-content-rev\"");
            value["meta_rev"] = json!("deleted-meta");
            value["meta_etag"] = json!("\"deleted-meta\"");
            value["node_etag"] = json!("\"deleted-node\"");
            root.children_generation = ChangeSeq::new(2);
            root.last_change_seq = ChangeSeq::new(3);
            vec![root, serde_json::from_value::<NodeState>(value).unwrap()]
        } else {
            value["content_rev"] = json!("later-rev");
            value["content_etag"] = json!("\"later-rev\"");
            value["node_etag"] = json!("\"later-node\"");
            value["content_hash"] = json!(digest(b"later remote\n"));
            value["size"] = json!("13");
            vec![serde_json::from_value::<NodeState>(value).unwrap()]
        };
        let page = ChangesPage {
            workspace_epoch: info(&rig.binding).workspace_epoch,
            from_exclusive: ChangeSeq::new(2),
            through_seq: ChangeSeq::new(3),
            covered_through: ChangeSeq::new(3),
            has_more: false,
            nodes,
        };
        store
            .apply_changes(&rig.binding, &page, ChangeSeq::new(3))
            .await
            .unwrap();
        assert!(
            request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
                .await
                .is_err(),
            "deleted={deleted}"
        );
        assert_eq!(
            store
                .read_outbox_operation(&rig.binding, &rig.followup)
                .await
                .unwrap()
                .unwrap()
                .abandon_state,
            OutboxAbandonState::None
        );
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(intent)
        );
        assert_originals(&mut store, &rig).await;
    }
}

#[tokio::test]
async fn abandon_intent_write_failure_rolls_back_request_without_losing_approved_versions() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirm_original_conflict(&mut store, &rig).await;
    let shown = store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap()
        .unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_conflict_abandon BEFORE UPDATE OF abandon_state ON client_outbox WHEN NEW.abandon_state='requested' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        request_selected_merge_followup(&mut store, &rig, intent.version, &shown).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    assert_originals(&mut store, &rig).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::query("DROP TRIGGER reject_conflict_abandon")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    let requested = request_selected_merge_followup(&mut store, &rig, intent.version, &shown)
        .await
        .unwrap();
    assert_eq!(requested.operation_id, rig.followup);
    assert_eq!(requested.bytes, LOCAL);
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
}
