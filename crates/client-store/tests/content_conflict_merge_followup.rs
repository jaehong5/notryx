include!("content_conflict_merge_queue.rs");

async fn inspect_merge_followup(
    store: &mut ClientStore,
    rig: &Rig,
    version: u64,
) -> Result<Option<OutboxOperation>, StoreError> {
    store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, version)
        .await
}

async fn approve_merge_followup(
    store: &mut ClientStore,
    rig: &Rig,
    version: u64,
    item: &OutboxOperation,
) -> Result<OutboxOperation, StoreError> {
    store
        .request_content_conflict_merge_followup(
            &rig.binding,
            &rig.operation,
            version,
            &item.operation_id,
            item.operation_seq,
            item.generation,
            &item.payload_hash,
        )
        .await
}

async fn confirmed_first_conflict(store: &mut ClientStore, rig: &Rig) {
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, 1))
        .await
        .unwrap();
}

#[tokio::test]
async fn merge_followup_candidate_waits_for_confirmed_conflict_and_does_not_mutate() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    let before = next_merge_sequence(&rig).await;
    let unconfirmed = inspect_merge_followup(&mut store, &rig, intent.version).await;
    assert!(matches!(unconfirmed, Ok(None) | Err(_)));
    let direct = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert!(
        approve_merge_followup(&mut store, &rig, intent.version, &direct)
            .await
            .is_err()
    );
    assert_originals(&mut store, &rig).await;
    confirmed_first_conflict(&mut store, &rig).await;
    let candidate = inspect_merge_followup(&mut store, &rig, intent.version)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(candidate.operation_id, rig.followup);
    assert_eq!(candidate.operation_seq, 2);
    assert_eq!(candidate.generation, intent.generation);
    assert_eq!(candidate.payload_hash, digest(LOCAL));
    assert_eq!(candidate.bytes, LOCAL);
    assert_eq!(candidate.status, OutboxStatus::Queued);
    assert_eq!(
        candidate.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_originals(&mut store, &rig).await;
}

#[tokio::test]
async fn merge_followup_never_uses_legacy_unselected_abandon_path() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirmed_first_conflict(&mut store, &rig).await;
    assert!(
        store
            .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    let candidate = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(candidate.status, OutboxStatus::Queued);
    assert_eq!(
        candidate.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    assert_eq!(candidate.bytes, LOCAL);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merge_followup_rejects_generic_id_only_abandon_without_affecting_other_documents() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirmed_first_conflict(&mut store, &rig).await;
    let other_session = store
        .open_registered_document(&rig.binding, OTHER, b"other base\n", "other-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &rig.binding,
            OTHER,
            &other_session.edit_session_id,
            1,
            b"other draft\n",
        )
        .await
        .unwrap();
    let other = store
        .queue_checkpoint(&rig.binding, OTHER, 1)
        .await
        .unwrap();
    assert!(
        store
            .request_outbox_abandon(&rig.binding, &rig.followup)
            .await
            .is_err()
    );
    let protected = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(protected.status, OutboxStatus::Queued);
    assert_eq!(
        protected.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    assert_eq!(protected.bytes, LOCAL);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    store
        .request_outbox_abandon(&rig.binding, &other.operation_id)
        .await
        .unwrap();
    let other_marked = store
        .read_outbox_operation(&rig.binding, &other.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        other_marked.abandon_state,
        notryx_client_store::OutboxAbandonState::Requested
    );
    assert_eq!(other_marked.bytes, b"other draft\n");
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
}

#[tokio::test]
async fn merge_followup_approval_rejects_stale_or_forged_identity_without_mutation() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirmed_first_conflict(&mut store, &rig).await;
    let item = inspect_merge_followup(&mut store, &rig, intent.version)
        .await
        .unwrap()
        .unwrap();
    let wrong_id = Uuid::new_v4().to_string();
    let other = binding();
    for (binding, op, version, followup, seq, generation, hash) in [
        (
            &other,
            rig.operation.as_str(),
            intent.version,
            item.operation_id.as_str(),
            item.operation_seq,
            item.generation,
            item.payload_hash.as_str(),
        ),
        (
            &rig.binding,
            rig.operation.as_str(),
            intent.version + 1,
            item.operation_id.as_str(),
            item.operation_seq,
            item.generation,
            item.payload_hash.as_str(),
        ),
        (
            &rig.binding,
            rig.operation.as_str(),
            intent.version,
            wrong_id.as_str(),
            item.operation_seq,
            item.generation,
            item.payload_hash.as_str(),
        ),
        (
            &rig.binding,
            rig.operation.as_str(),
            intent.version,
            item.operation_id.as_str(),
            item.operation_seq + 1,
            item.generation,
            item.payload_hash.as_str(),
        ),
        (
            &rig.binding,
            rig.operation.as_str(),
            intent.version,
            item.operation_id.as_str(),
            item.operation_seq,
            item.generation + 1,
            item.payload_hash.as_str(),
        ),
        (
            &rig.binding,
            rig.operation.as_str(),
            intent.version,
            item.operation_id.as_str(),
            item.operation_seq,
            item.generation,
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ),
    ] {
        assert!(
            store
                .request_content_conflict_merge_followup(
                    binding, op, version, followup, seq, generation, hash
                )
                .await
                .is_err()
        );
    }
    let unchanged = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        unchanged.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    assert_eq!(unchanged.bytes, LOCAL);
    assert_originals(&mut store, &rig).await;
}

#[tokio::test]
async fn merge_followup_approval_retries_same_identity_and_waits_for_terminal_ack() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirmed_first_conflict(&mut store, &rig).await;
    let other_session = store
        .open_registered_document(&rig.binding, OTHER, b"other base\n", "other-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &rig.binding,
            OTHER,
            &other_session.edit_session_id,
            1,
            b"other draft\n",
        )
        .await
        .unwrap();
    let other = store
        .queue_checkpoint(&rig.binding, OTHER, 1)
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, OTHER, &other_session.edit_session_id)
        .await
        .unwrap();
    let item = inspect_merge_followup(&mut store, &rig, intent.version)
        .await
        .unwrap()
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    assert!(queue_merge(&mut store, &rig, intent.version).await.is_err());
    let marked = approve_merge_followup(&mut store, &rig, intent.version, &item)
        .await
        .unwrap();
    assert_eq!(marked.operation_id, item.operation_id);
    assert_eq!(marked.operation_seq, item.operation_seq);
    assert_eq!(marked.bytes, item.bytes);
    assert_eq!(
        marked.abandon_state,
        notryx_client_store::OutboxAbandonState::Requested
    );
    let retry = approve_merge_followup(&mut store, &rig, intent.version, &item)
        .await
        .unwrap();
    assert!(retry == marked);
    let Some(PendingOutboxOperation::Content(sent)) =
        store.prepare_pending_outbox(&rig.binding).await.unwrap()
    else {
        panic!("expected followup abandon")
    };
    assert_eq!(sent.operation_id, item.operation_id);
    assert_eq!(sent.operation_seq, item.operation_seq);
    assert_eq!(
        sent.abandon_state,
        notryx_client_store::OutboxAbandonState::Sent
    );
    let retry = approve_merge_followup(&mut store, &rig, intent.version, &item)
        .await
        .unwrap();
    assert_eq!(retry.operation_id, sent.operation_id);
    assert!(queue_merge(&mut store, &rig, intent.version).await.is_err());
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
        approve_merge_followup(&mut store, &rig, intent.version, &item)
            .await
            .is_err()
    );
    assert!(queue_merge(&mut store, &rig, intent.version).await.is_err());
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    let terminal = approve_merge_followup(&mut store, &rig, intent.version, &item)
        .await
        .unwrap();
    assert_eq!(terminal.operation_id, item.operation_id);
    assert_eq!(terminal.operation_seq, item.operation_seq);
    assert_eq!(terminal.status, OutboxStatus::Abandoned);
    assert_eq!(terminal.bytes, LOCAL);
    let merged = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    assert_eq!(merged.bytes, b"\xef\xbb\xbfmerged\n");
    assert_eq!(merged.expected_revision.as_deref(), Some("current-rev"));
    assert!(merged.operation_seq > other.operation_seq);
    assert!(
        store
            .read_outbox_operation(&rig.binding, &other.operation_id)
            .await
            .unwrap()
            == Some(other)
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merge_followup_stale_checkpoint_requires_reconsideration_and_preserves_both_contents() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirmed_first_conflict(&mut store, &rig).await;
    let shown = inspect_merge_followup(&mut store, &rig, intent.version)
        .await
        .unwrap()
        .unwrap();
    let later = b"later local edit\n";
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, later)
        .await
        .unwrap();
    let coalesced = store.queue_checkpoint(&rig.binding, DOC, 3).await.unwrap();
    assert_eq!(coalesced.operation_id, shown.operation_id);
    assert_eq!(coalesced.generation, 3);
    assert_ne!(coalesced.payload_hash, shown.payload_hash);
    assert!(
        approve_merge_followup(&mut store, &rig, intent.version, &shown)
            .await
            .is_err()
    );
    let stored = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    assert_eq!(stored.bytes, later);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, later);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        intent.choice,
        ContentConflictChoice::SaveMerged {
            bytes: b"\xef\xbb\xbfmerged\n".to_vec()
        }
    );
    let mut request = rig.request.clone();
    request.generation = 3;
    request.choice = ContentConflictChoice::SaveMerged {
        bytes: b"newly reviewed merge\n".to_vec(),
    };
    let revised = store
        .replace_pending_content_conflict_intent(
            &rig.binding,
            DOC,
            &rig.operation,
            &intent,
            &request,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, intent.version)
            .await
            .unwrap(),
        Some(intent)
    );
    let newer = inspect_merge_followup(&mut store, &rig, revised.version)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newer.operation_id, shown.operation_id);
    assert_eq!(newer.operation_seq, shown.operation_seq);
    assert_eq!(newer.generation, 3);
    assert_eq!(newer.payload_hash, digest(later));
    let requested = approve_merge_followup(&mut store, &rig, revised.version, &newer)
        .await
        .unwrap();
    assert_eq!(
        requested.abandon_state,
        notryx_client_store::OutboxAbandonState::Requested
    );
    assert_eq!(requested.bytes, later);
}

#[tokio::test]
async fn merge_followup_approval_rechecks_new_checkpoint_even_before_it_is_queued() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirmed_first_conflict(&mut store, &rig).await;
    let shown = inspect_merge_followup(&mut store, &rig, intent.version)
        .await
        .unwrap()
        .unwrap();
    let later = b"saved but not queued\n";
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, later)
        .await
        .unwrap();
    assert!(
        approve_merge_followup(&mut store, &rig, intent.version, &shown)
            .await
            .is_err()
    );
    let unchanged = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.operation_id, shown.operation_id);
    assert_eq!(unchanged.operation_seq, shown.operation_seq);
    assert_eq!(unchanged.generation, shown.generation);
    assert_eq!(unchanged.payload_hash, shown.payload_hash);
    assert_eq!(
        unchanged.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    assert_eq!(unchanged.bytes, LOCAL);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 3);
    assert_eq!(draft.bytes, later);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}
