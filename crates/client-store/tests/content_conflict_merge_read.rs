include!("content_conflict_merge_queue.rs");

async fn read_merge(
    store: &mut ClientStore,
    rig: &Rig,
    version: u64,
) -> Result<Option<OutboxOperation>, StoreError> {
    store
        .read_queued_content_conflict_merge(&rig.binding, &rig.operation, version)
        .await
}

#[tokio::test]
async fn merge_read_absent_does_not_create_work_or_close_the_session() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    let before = next_merge_sequence(&rig).await;
    assert!(
        read_merge(&mut store, &rig, intent.version)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_originals(&mut store, &rig).await;
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"continue local\n")
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"continue local\n");
    assert_eq!(draft.base_bytes, BASE);
}

#[tokio::test]
async fn merge_read_recovers_same_operation_without_changing_newer_draft_or_session() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let queued = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    let before = next_merge_sequence(&rig).await;
    let opened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    let later_generation = queued.generation + 1;
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &opened.edit_session_id,
            later_generation,
            b"later input\n",
        )
        .await
        .unwrap();
    assert!(read_merge(&mut store, &rig, intent.version).await.unwrap() == Some(queued.clone()));
    assert_eq!(next_merge_sequence(&rig).await, before);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, later_generation);
    assert_eq!(draft.bytes, b"later input\n");
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, "current-rev");
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &opened.edit_session_id,
            later_generation + 1,
            b"session still live\n",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert!(
        read_merge(&mut reopened, &rig, intent.version)
            .await
            .unwrap()
            == Some(queued.clone())
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    let draft = reopened
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, later_generation + 1);
    assert_eq!(draft.bytes, b"session still live\n");
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(
        reopened
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merge_read_rejects_wrong_version_or_binding_and_keeps_evidence() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let queued = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    let before = next_merge_sequence(&rig).await;
    let other = binding();
    assert!(
        read_merge(&mut store, &rig, intent.version + 1)
            .await
            .is_err()
    );
    assert!(
        store
            .read_queued_content_conflict_merge(&other, &rig.operation, intent.version)
            .await
            .is_err()
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert!(
        store
            .read_outbox_operation(&rig.binding, &queued.operation_id)
            .await
            .unwrap()
            == Some(queued)
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
async fn merge_read_rejects_corrupt_linked_payload_without_changing_it_or_checkpoint() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let queued = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    let before = next_merge_sequence(&rig).await;
    let corrupt = b"\xef\xbb\xbfmutate\n";
    assert_eq!(corrupt.len(), queued.bytes.len());
    let mut db = connect(&rig.path).await;
    sqlx::query("UPDATE client_outbox SET bytes=? WHERE operation_id=?")
        .bind(corrupt.as_slice())
        .bind(&queued.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read_merge(&mut store, &rig, intent.version).await.is_err());
    assert_eq!(next_merge_sequence(&rig).await, before);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, queued.bytes);
    assert_eq!(draft.base_bytes, REMOTE);
    let mut db = connect(&rig.path).await;
    let retained: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM client_outbox WHERE operation_id=?")
            .bind(&queued.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(retained, corrupt.as_slice());
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merge_read_reports_old_registration_as_frozen_after_replacement() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let queued = queue_merge(&mut store, &rig, intent.version).await.unwrap();
    let mut replacement = binding();
    replacement.workspace_id = rig.binding.workspace_id.clone();
    replacement.workspace_epoch = rig.binding.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    let saved = read_merge(&mut store, &rig, intent.version)
        .await
        .unwrap()
        .unwrap();
    assert!(saved.frozen);
    assert_eq!(saved.operation_id, queued.operation_id);
    assert_eq!(saved.operation_seq, queued.operation_seq);
    assert_eq!(saved.bytes, queued.bytes);
    assert!(
        store
            .read_queued_content_conflict_merge(&replacement, &rig.operation, intent.version)
            .await
            .unwrap()
            .is_none()
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, queued.bytes);
    assert_eq!(draft.base_bytes, REMOTE);
}

#[tokio::test]
async fn merge_read_rejects_committed_revision_that_disagrees_with_durable_base() {
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
    let mut db = connect(&rig.path).await;
    sqlx::query("UPDATE client_outbox SET committed_revision='wrong-rev' WHERE operation_id=?")
        .bind(&queued.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read_merge(&mut store, &rig, intent.version).await.is_err());
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, queued.bytes);
    assert_eq!(draft.base_bytes, queued.bytes);
    assert_eq!(draft.base_revision, "merged-commit-rev");
}

async fn assert_merge_read_rejects_bad_conflict_evidence(corrupt_etag: bool) {
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
        .apply_outbox_conflict(
            &rig.binding,
            &OutboxConflict {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                current_revision: "newer-rev".to_owned(),
                current_etag: "\"newer-rev\"".to_owned(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    let normal = read_merge(&mut store, &rig, intent.version)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(normal.operation_id, sent.operation_id);
    assert_eq!(normal.status, OutboxStatus::Conflict);
    assert_eq!(normal.conflict_revision.as_deref(), Some("newer-rev"));
    assert_eq!(normal.expected_revision.as_deref(), Some("current-rev"));
    let mut db = connect(&rig.path).await;
    if corrupt_etag {
        sqlx::query("UPDATE client_outbox SET conflict_etag='\"wrong-rev\"' WHERE operation_id=?")
            .bind(&sent.operation_id)
            .execute(&mut db)
            .await
            .unwrap();
    } else {
        sqlx::query("UPDATE client_outbox SET conflict_revision=expected_revision,conflict_etag='\"current-rev\"' WHERE operation_id=?")
            .bind(&sent.operation_id).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    assert!(read_merge(&mut store, &rig, intent.version).await.is_err());
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, queued.bytes);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
}

#[tokio::test]
async fn merge_read_rejects_mismatched_conflict_etag() {
    assert_merge_read_rejects_bad_conflict_evidence(true).await;
}

#[tokio::test]
async fn merge_read_rejects_conflict_revision_equal_to_precondition() {
    assert_merge_read_rejects_bad_conflict_evidence(false).await;
}
