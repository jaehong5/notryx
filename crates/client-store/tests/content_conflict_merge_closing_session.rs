include!("content_conflict_merge_queue.rs");

async fn queue_with_session(
    store: &mut ClientStore,
    rig: &Rig,
    version: u64,
) -> Result<OutboxOperation, StoreError> {
    store
        .queue_content_conflict_merge_closing_session(
            &rig.binding,
            DOC,
            &rig.session,
            &rig.operation,
            version,
        )
        .await
}

#[tokio::test]
async fn merge_closing_session_queues_exact_approved_content_and_revokes_old_session() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    let queued = queue_with_session(&mut store, &rig, intent.version)
        .await
        .unwrap();
    assert_eq!(queued.bytes, b"\xef\xbb\xbfmerged\n");
    assert_eq!(queued.expected_revision.as_deref(), Some("current-rev"));
    assert_eq!(queued.generation, intent.generation + 1);
    assert_eq!(queued.status, OutboxStatus::Queued);
    assert!(
        store
            .save_registered_checkpoint(
                &rig.binding,
                DOC,
                &rig.session,
                queued.generation + 1,
                b"stale session\n"
            )
            .await
            .is_err()
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, queued.generation);
    assert_eq!(draft.bytes, queued.bytes);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, "current-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
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
    let reopened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    assert_ne!(reopened.edit_session_id, rig.session);
    assert_eq!(reopened.generation, queued.generation);
    assert_eq!(reopened.bytes, queued.bytes);
}

#[tokio::test]
async fn merge_closing_session_insert_failure_restores_unchanged_session_and_retries() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    let before = next_merge_sequence(&rig).await;
    let mut db = connect(&rig.path).await;
    sqlx::query("CREATE TRIGGER fail_merge_content BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='content' BEGIN SELECT RAISE(ABORT, 'injected merge failure'); END")
        .execute(&mut db).await.unwrap();
    assert!(
        queue_with_session(&mut store, &rig, intent.version)
            .await
            .is_err()
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
    sqlx::query("DROP TRIGGER fail_merge_content")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let queued = queue_with_session(&mut store, &rig, intent.version)
        .await
        .unwrap();
    assert_eq!(queued.operation_seq, before as u64);
    assert_eq!(queued.bytes, b"\xef\xbb\xbfmerged\n");
    assert_eq!(queued.expected_revision.as_deref(), Some("current-rev"));
    assert!(
        store
            .save_registered_checkpoint(
                &rig.binding,
                DOC,
                &rig.session,
                queued.generation + 1,
                b"stale session\n"
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn merge_closing_session_rejects_wrong_session_and_preserves_live_session() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    let before = next_merge_sequence(&rig).await;
    let wrong = Uuid::new_v4().to_string();
    assert!(
        store
            .queue_content_conflict_merge_closing_session(
                &rig.binding,
                DOC,
                &wrong,
                &rig.operation,
                intent.version
            )
            .await
            .is_err()
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"session still live\n")
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"session still live\n");
    assert_eq!(draft.base_bytes, BASE);
}

#[tokio::test]
async fn merge_closing_session_rejects_newer_checkpoint_but_keeps_its_session_and_bytes() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    let newer = b"newer input before approval\n";
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, newer)
        .await
        .unwrap();
    let before = next_merge_sequence(&rig).await;
    assert!(
        queue_with_session(&mut store, &rig, intent.version)
            .await
            .is_err()
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_unresolved_versions(&mut store, &rig, &intent, newer, OutboxStatus::Abandoned).await;
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 4, b"later input\n")
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 4);
    assert_eq!(draft.bytes, b"later input\n");
    assert_eq!(draft.base_bytes, BASE);
}

#[tokio::test]
async fn merge_closing_session_rejects_wrong_document_or_version_without_closing_session() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    let before = next_merge_sequence(&rig).await;
    assert!(
        store
            .queue_content_conflict_merge_closing_session(
                &rig.binding,
                OTHER,
                &rig.session,
                &rig.operation,
                intent.version
            )
            .await
            .is_err()
    );
    assert!(
        queue_with_session(&mut store, &rig, intent.version + 1)
            .await
            .is_err()
    );
    assert_eq!(next_merge_sequence(&rig).await, before);
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"session still live\n")
        .await
        .unwrap();
}
