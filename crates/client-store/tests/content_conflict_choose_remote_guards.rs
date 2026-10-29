include!("content_conflict_intent.rs");

use notryx_client_store::{OutboxAbandoned, OutboxStatus, PendingOutboxOperation};

async fn record_remote_choice(store: &mut ClientStore, rig: &Rig) -> ContentConflictIntent {
    let mut request = rig.request.clone();
    request.choice = ContentConflictChoice::ChooseRemote;
    record(store, rig, &request).await.unwrap()
}

fn ack(binding: &RegistrationBinding, through: u64) -> notryx_protocol::AckResponse {
    serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": through.to_string(),
        "retired_through": through.to_string(),
    }))
    .unwrap()
}

async fn request_and_finish_followup(store: &mut ClientStore, rig: &Rig, confirm: bool) {
    let intent = store
        .read_content_conflict_intent(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let requested = if matches!(&intent.choice, ContentConflictChoice::SaveMerged { .. }) {
        store
            .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, 1))
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
    assert_eq!(requested.operation_id, rig.followup);
    let Some(PendingOutboxOperation::Content(sent)) =
        store.prepare_pending_outbox(&rig.binding).await.unwrap()
    else {
        panic!("expected content abandon")
    };
    assert_eq!(sent.operation_id, rig.followup);
    store
        .apply_outbox_abandoned(
            &rig.binding,
            &OutboxAbandoned {
                operation_id: sent.operation_id,
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    if confirm {
        store
            .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, 2))
            .await
            .unwrap();
    }
}

async fn assert_unresolved_versions(
    store: &mut ClientStore,
    rig: &Rig,
    intent: &ContentConflictIntent,
    local: &[u8],
    followup: OutboxStatus,
) {
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .as_ref(),
        Some(intent)
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, local);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, "base-rev");
    let failed = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, OutboxStatus::Conflict);
    assert_eq!(failed.bytes, SENT);
    assert_eq!(failed.expected_revision.as_deref(), Some("base-rev"));
    let later = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(later.status, followup);
    assert_eq!(later.bytes, LOCAL);
    assert_eq!(later.operation_seq, failed.operation_seq + 1);
    assert_eq!(
        store
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
}

#[tokio::test]
async fn changed_or_deleted_remote_after_approval_blocks_remote_choice_completion() {
    for deleted in [false, true] {
        let (rig, mut store) = rig().await;
        let intent = record_remote_choice(&mut store, &rig).await;
        request_and_finish_followup(&mut store, &rig, true).await;
        store
            .close_registered_document(&rig.binding, DOC, &rig.session)
            .await
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
            store
                .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
                .await
                .is_err(),
            "deleted={deleted}"
        );
        assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
        assert_eq!(
            store.confirmed_receipt_through(&rig.binding).await.unwrap(),
            2
        );
    }
}

#[tokio::test]
async fn newer_local_checkpoint_blocks_remote_choice_and_keeps_both_generations() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"later local edit\n")
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
    assert_unresolved_versions(
        &mut store,
        &rig,
        &intent,
        b"later local edit\n",
        OutboxStatus::Abandoned,
    )
    .await;
    assert_eq!(intent.local_bytes, LOCAL);
    assert_eq!(intent.generation, 2);
    assert_eq!(
        store
            .read_registered_checkpoint(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap()
            .generation,
        3
    );
}

#[tokio::test]
async fn replaced_registration_cannot_complete_or_attach_old_remote_choice() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = rig.binding.workspace_id.clone();
    replacement.workspace_epoch = rig.binding.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    assert!(
        store
            .complete_content_conflict_choose_remote(&replacement, &rig.operation)
            .await
            .is_err()
    );
    let old_intent = store
        .read_content_conflict_intent(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_intent, intent);
    assert!(
        store
            .read_content_conflict_intent(&replacement, &rig.operation)
            .await
            .unwrap()
            .is_none()
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .frozen
    );
    assert!(
        store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap()
            .frozen
    );
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn unresolved_or_unconfirmed_followup_blocks_remote_choice_completion() {
    for phase in ["queued", "requested", "sent", "local_ack_only"] {
        let (rig, mut store) = rig().await;
        let intent = record_remote_choice(&mut store, &rig).await;
        store
            .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, 1))
            .await
            .unwrap();
        if phase != "queued" {
            store
                .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
                .await
                .unwrap()
                .unwrap();
        }
        if matches!(phase, "sent" | "local_ack_only") {
            let Some(PendingOutboxOperation::Content(sent)) =
                store.prepare_pending_outbox(&rig.binding).await.unwrap()
            else {
                panic!("expected content abandon")
            };
            if phase == "local_ack_only" {
                store
                    .apply_outbox_abandoned(
                        &rig.binding,
                        &OutboxAbandoned {
                            operation_id: sent.operation_id,
                            operation_seq: sent.operation_seq,
                            workspace_epoch: rig.binding.workspace_epoch.clone(),
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        store
            .close_registered_document(&rig.binding, DOC, &rig.session)
            .await
            .unwrap();
        assert!(
            store
                .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
                .await
                .is_err(),
            "{phase}"
        );
        let expected = match phase {
            "queued" => OutboxStatus::Queued,
            "requested" | "sent" => OutboxStatus::Abandoning,
            "local_ack_only" => OutboxStatus::Abandoned,
            _ => unreachable!(),
        };
        assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, expected).await;
        assert_eq!(
            store.confirmed_receipt_through(&rig.binding).await.unwrap(),
            1,
            "{phase}"
        );
    }
}

#[tokio::test]
async fn active_editor_session_blocks_remote_choice_even_after_followup_is_confirmed() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    request_and_finish_followup(&mut store, &rig, true).await;
    assert!(
        store
            .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
    assert_eq!(
        store.confirmed_receipt_through(&rig.binding).await.unwrap(),
        2
    );
}
