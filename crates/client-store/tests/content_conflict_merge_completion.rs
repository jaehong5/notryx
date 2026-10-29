include!("content_conflict_merge_queue.rs");

async fn commit_merged(store: &mut ClientStore, rig: &Rig, sent: &OutboxOperation) {
    store
        .apply_outbox_commit(
            &rig.binding,
            &notryx_client_store::OutboxCommit {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                document_id: DOC.to_owned(),
                content_revision: "merged-revision".to_owned(),
                content_hash: sent.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
}

async fn assert_merge_pending(store: &mut ClientStore, rig: &Rig) {
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
    assert!(
        store
            .document_outbox_state(&rig.binding, DOC)
            .await
            .unwrap()
            .has_conflict
    );
}

async fn assert_merge_completed(store: &mut ClientStore, rig: &Rig, generation: u64) {
    assert!(matches!(
        store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Completed {
            generation: actual,
            frozen: false,
        } if actual == generation
    ));
    assert!(
        !store
            .document_outbox_state(&rig.binding, DOC)
            .await
            .unwrap()
            .has_conflict
    );
    assert!(
        store
            .first_content_conflict(&rig.binding, DOC)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn merged_resolution_stays_pending_until_confirmed_ack_and_survives_reopen() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let merged = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    assert_merge_pending(&mut store, &rig).await;
    let sent = store
        .prepare_outbox_matching(&rig.binding, &merged)
        .await
        .unwrap()
        .unwrap();
    assert_merge_pending(&mut store, &rig).await;
    commit_merged(&mut store, &rig, &sent).await;
    assert_eq!(
        store.confirmed_receipt_through(&rig.binding).await.unwrap(),
        2
    );
    assert_merge_pending(&mut store, &rig).await;
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    assert_merge_completed(&mut store, &rig, merged.generation).await;
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, merged.generation);
    assert_eq!(draft.bytes, merged.bytes);
    assert_eq!(draft.base_bytes, merged.bytes);
    assert_eq!(draft.base_revision, "merged-revision");
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
    assert_merge_completed(&mut reopened, &rig, merged.generation).await;
    let retry = queue_merge(&mut reopened, &rig, intent.version)
        .await
        .unwrap();
    assert_eq!(retry.operation_id, merged.operation_id);
    assert_eq!(retry.operation_seq, merged.operation_seq);
    assert_eq!(retry.bytes, merged.bytes);
    assert_eq!(retry.status, OutboxStatus::Committed);
    assert_eq!(
        reopened
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merged_completion_preserves_later_draft_and_dependent_payload() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let merged = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    let opened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    let later = b"later local edit after merge\n";
    let generation = merged.generation + 1;
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &opened.edit_session_id,
            generation,
            later,
        )
        .await
        .unwrap();
    let followup = store
        .queue_checkpoint(&rig.binding, DOC, generation)
        .await
        .unwrap();
    let sent = store
        .prepare_outbox_matching(&rig.binding, &merged)
        .await
        .unwrap()
        .unwrap();
    commit_merged(&mut store, &rig, &sent).await;
    assert_merge_pending(&mut store, &rig).await;
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    assert_merge_completed(&mut store, &rig, merged.generation).await;
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, generation);
    assert_eq!(draft.bytes, later);
    assert_eq!(draft.base_bytes, merged.bytes);
    assert_eq!(draft.base_revision, "merged-revision");
    let later_operation = store
        .read_outbox_operation(&rig.binding, &followup.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(later_operation.bytes, later);
    assert_eq!(
        later_operation.expected_revision.as_deref(),
        Some("merged-revision")
    );
    assert_eq!(later_operation.status, OutboxStatus::Queued);
    store
        .close_registered_document(&rig.binding, DOC, &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_merge_completed(&mut reopened, &rig, merged.generation).await;
    let draft = reopened
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, generation);
    assert_eq!(draft.bytes, later);
    assert_eq!(draft.base_bytes, merged.bytes);
    assert_eq!(
        reopened
            .read_outbox_operation(&rig.binding, &followup.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        later
    );
}
