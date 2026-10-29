include!("content_conflict_choose_remote_guards.rs");

#[tokio::test]
async fn completed_remote_checkpoint_is_not_a_new_content_operation_when_reopened() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&rig.binding, DOC, BASE, "base-rev")
        .await
        .unwrap();
    assert_eq!(opened.generation, selected.generation);
    assert_eq!(opened.bytes, REMOTE);
    assert!(matches!(
        store
            .queue_checkpoint(&rig.binding, DOC, opened.generation)
            .await,
        Err(StoreError::OutboxBlocked)
    ));
    let checkpoint = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.bytes, REMOTE);
    assert_eq!(checkpoint.base_bytes, REMOTE);
    assert_eq!(checkpoint.base_revision, "current-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
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
async fn pending_reopen_with_queued_followup_keeps_its_original_identity_and_blocked_dependency() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&rig.binding, DOC, BASE, "base-rev")
        .await
        .unwrap();
    assert_eq!(opened.generation, 2);
    assert_eq!(opened.bytes, LOCAL);
    let queued = store
        .queue_checkpoint(&rig.binding, DOC, opened.generation)
        .await
        .unwrap();
    assert_eq!(queued.operation_id, rig.followup);
    assert_eq!(queued.bytes, LOCAL);
    assert!(matches!(
        store.prepare_outbox(&rig.binding).await,
        Err(StoreError::OutboxBlocked)
    ));
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Queued).await;
}

#[tokio::test]
async fn pending_reopen_after_abandon_cannot_requeue_the_same_local_checkpoint() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&rig.binding, DOC, BASE, "base-rev")
        .await
        .unwrap();
    assert_eq!(opened.generation, 2);
    assert_eq!(opened.bytes, LOCAL);
    assert!(matches!(
        store
            .queue_checkpoint(&rig.binding, DOC, opened.generation)
            .await,
        Err(StoreError::OutboxBlocked)
    ));
    assert_unresolved_versions(&mut store, &rig, &intent, LOCAL, OutboxStatus::Abandoned).await;
}
