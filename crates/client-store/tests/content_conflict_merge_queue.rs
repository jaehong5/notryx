include!("content_conflict_choose_remote_guards.rs");

use notryx_client_store::OutboxOperation;

async fn queue_merge(
    store: &mut ClientStore,
    rig: &Rig,
    version: u64,
) -> Result<OutboxOperation, StoreError> {
    store
        .queue_content_conflict_merge(&rig.binding, &rig.operation, version)
        .await
}

async fn next_merge_sequence(rig: &Rig) -> i64 {
    let mut db = connect(&rig.path).await;
    sqlx::query_scalar("SELECT s.next_seq FROM outbox_streams s JOIN client_registrations r ON r.id=s.registration_id WHERE r.status='active'")
        .fetch_one(&mut db)
        .await
        .unwrap()
}

#[tokio::test]
async fn merged_content_is_queued_once_after_followup_ack_and_survives_reopen() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    let before = next_merge_sequence(&rig).await;
    assert!(queue_merge(&mut store, &rig, intent.version).await.is_err());
    assert_originals(&mut store, &rig).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    request_and_finish_followup(&mut store, &rig, false).await;
    assert!(queue_merge(&mut store, &rig, intent.version).await.is_err());
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
    assert_eq!(next_merge_sequence(&rig).await, before);
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, 2))
        .await
        .unwrap();

    let queued = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    assert_eq!(queued.document_id, DOC);
    assert_eq!(queued.operation_seq, before as u64);
    assert_ne!(queued.operation_id, rig.operation);
    assert_ne!(queued.operation_id, rig.followup);
    assert_eq!(queued.expected_revision.as_deref(), Some("current-rev"));
    assert_eq!(queued.bytes, b"\xef\xbb\xbfmerged\n");
    assert_eq!(queued.payload_hash, digest(&queued.bytes));
    assert_eq!(queued.status, OutboxStatus::Queued);
    assert!(matches!(
        store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending {
            choice: notryx_client_store::ContentConflictChoiceKind::SaveMerged,
            needs_review: false,
            ..
        }
    ));
    assert_eq!(
        store
            .first_content_conflict(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        rig.operation
    );
    assert!(queue_merge(&mut store, &rig, intent.version).await.unwrap() == queued);
    assert_eq!(next_merge_sequence(&rig).await, before + 1);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        LOCAL
    );

    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert!(
        queue_merge(&mut reopened, &rig, intent.version)
            .await
            .unwrap()
            == queued
    );
    assert_eq!(next_merge_sequence(&rig).await, before + 1);
    assert_eq!(
        reopened
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merge_queue_rejects_changed_remote_without_discarding_approved_content() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let before = next_merge_sequence(&rig).await;
    let (_, original) = remote_nodes();
    let mut value = serde_json::to_value(original).unwrap();
    value["last_change_seq"] = json!("3");
    value["content_rev"] = json!("later-rev");
    value["content_etag"] = json!("\"later-rev\"");
    value["node_etag"] = json!("\"later-node\"");
    value["content_hash"] = json!(digest(b"later remote\n"));
    value["size"] = json!("13");
    store
        .apply_changes(
            &rig.binding,
            &ChangesPage {
                workspace_epoch: info(&rig.binding).workspace_epoch,
                from_exclusive: ChangeSeq::new(2),
                through_seq: ChangeSeq::new(3),
                covered_through: ChangeSeq::new(3),
                has_more: false,
                nodes: vec![serde_json::from_value::<NodeState>(value).unwrap()],
            },
            ChangeSeq::new(3),
        )
        .await
        .unwrap();
    assert!(queue_merge(&mut store, &rig, intent.version).await.is_err());
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
}

#[tokio::test]
async fn merge_queue_insert_failure_rolls_back_and_retries_after_reopen() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let before = next_merge_sequence(&rig).await;
    let mut db = connect(&rig.path).await;
    sqlx::query("CREATE TRIGGER fail_merged_content BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='content' BEGIN SELECT RAISE(ABORT, 'injected merge failure'); END")
        .execute(&mut db).await.unwrap();
    assert!(queue_merge(&mut store, &rig, intent.version).await.is_err());
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
    sqlx::query("DROP TRIGGER fail_merged_content")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    let queued = queue_merge(&mut reopened, &rig, intent.version)
        .await
        .unwrap();
    assert_eq!(queued.operation_seq, before as u64);
    assert_eq!(queued.bytes, b"\xef\xbb\xbfmerged\n");
    assert_eq!(queued.expected_revision.as_deref(), Some("current-rev"));
}

#[tokio::test]
async fn editing_after_merge_queue_preserves_approved_operation_and_queues_a_dependent_payload() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let merged = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    assert_eq!(merged.bytes, b"\xef\xbb\xbfmerged\n");
    assert_eq!(merged.expected_revision.as_deref(), Some("current-rev"));

    let opened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    assert_eq!(opened.generation, merged.generation);
    assert_eq!(opened.bytes, merged.bytes);
    let later = b"\xef\xbb\xbflater edit after approving merge\n";
    let later_generation = merged.generation + 1;
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &opened.edit_session_id,
            later_generation,
            later,
        )
        .await
        .unwrap();
    let followup = store
        .queue_checkpoint(&rig.binding, DOC, later_generation)
        .await
        .unwrap();
    assert_ne!(followup.operation_id, merged.operation_id);
    assert!(followup.operation_seq > merged.operation_seq);
    assert_eq!(followup.generation, later_generation);
    assert_eq!(followup.bytes, later);
    assert_eq!(followup.expected_revision, None);
    let persisted_merge = store
        .read_outbox_operation(&rig.binding, &merged.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(persisted_merge == merged);
    let mut db = connect(&rig.path).await;
    let dependency: Option<String> =
        sqlx::query_scalar("SELECT dependency FROM client_outbox WHERE operation_id=?")
            .bind(&followup.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(dependency.as_deref(), Some(merged.operation_id.as_str()));
    db.close().await.unwrap();

    let sent = store
        .prepare_outbox_matching(&rig.binding, &merged)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent.bytes, merged.bytes);
    store
        .apply_outbox_commit(
            &rig.binding,
            &notryx_client_store::OutboxCommit {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                document_id: DOC.to_owned(),
                content_revision: "merged-commit-rev".to_owned(),
                content_hash: sent.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    let persisted_followup = store
        .read_outbox_operation(&rig.binding, &followup.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted_followup.bytes, later);
    assert_eq!(
        persisted_followup.expected_revision.as_deref(),
        Some("merged-commit-rev")
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, later_generation);
    assert_eq!(draft.bytes, later);
    assert_eq!(draft.base_bytes, merged.bytes);
    assert_eq!(draft.base_revision, "merged-commit-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merged_commit_without_confirmed_ack_is_still_pending() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let queued = queue_merge(&mut store, &rig, intent.version).await.unwrap();
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
                document_id: DOC.to_owned(),
                content_revision: "merged-commit-rev".to_owned(),
                content_hash: sent.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.receipt_ack_through(&rig.binding).await.unwrap(),
        sent.operation_seq
    );
    assert_eq!(
        store.confirmed_receipt_through(&rig.binding).await.unwrap(),
        2
    );
    assert!(matches!(
        store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending {
            choice: notryx_client_store::ContentConflictChoiceKind::SaveMerged,
            ..
        }
    ));
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merged_412_is_the_next_conflict_but_review_waits_for_confirmed_ack() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let merged = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    let sent = store
        .prepare_outbox_matching(&rig.binding, &merged)
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
                current_revision: "later-remote-rev".to_owned(),
                current_etag: "\"later-remote-rev\"".to_owned(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending {
            needs_review: false,
            ..
        }
    ));
    assert_eq!(
        store
            .first_content_conflict(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        merged.operation_id
    );
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    assert!(matches!(
        store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending {
            needs_review: true,
            ..
        }
    ));
    assert_eq!(
        store
            .first_content_conflict(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        merged.operation_id
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, merged.bytes);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, "current-rev");
    let (_, original) = remote_nodes();
    let later_remote = b"later remote\n";
    let mut value = serde_json::to_value(original).unwrap();
    value["last_change_seq"] = json!("3");
    value["content_rev"] = json!("later-remote-rev");
    value["content_etag"] = json!("\"later-remote-rev\"");
    value["node_etag"] = json!("\"later-node\"");
    value["content_hash"] = json!(digest(later_remote));
    value["size"] = json!(later_remote.len().to_string());
    let node: NodeState = serde_json::from_value(value).unwrap();
    store
        .apply_changes(
            &rig.binding,
            &ChangesPage {
                workspace_epoch: info(&rig.binding).workspace_epoch,
                from_exclusive: ChangeSeq::new(2),
                through_seq: ChangeSeq::new(3),
                covered_through: ChangeSeq::new(3),
                has_more: false,
                nodes: vec![node.clone()],
            },
            ChangeSeq::new(3),
        )
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&rig.binding, &node, later_remote)
            .await
            .unwrap()
    );
    let replacement = ContentConflictIntentRequest {
        generation: merged.generation,
        remote_revision: "later-remote-rev".to_owned(),
        remote_hash: digest(later_remote),
        remote_bytes: later_remote.to_vec(),
        choice: ContentConflictChoice::ChooseRemote,
    };
    let new_intent = store
        .record_content_conflict_intent(&rig.binding, DOC, &merged.operation_id, &replacement)
        .await
        .unwrap();
    assert_eq!(new_intent.base_bytes, REMOTE);
    assert_eq!(new_intent.local_bytes, merged.bytes);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}
