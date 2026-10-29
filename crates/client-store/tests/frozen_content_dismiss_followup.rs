include!("frozen_content_dismiss.rs");

async fn approval_for_old_operation(rig: &mut ApplyRig, operation_id: &str) -> DismissApproval {
    let item = rig
        .store
        .read_outbox_operation(&rig.approval.old, operation_id)
        .await
        .unwrap()
        .unwrap();
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    let evidence = snapshot(&rig.path).await;
    let dependency = evidence
        .1
        .iter()
        .find(|row| row.0 == operation_id)
        .unwrap()
        .3
        .clone();
    DismissApproval {
        current: rig.approval.current.clone(),
        old: rig.approval.old.clone(),
        document: DOC_A.into(),
        operation_id: item.operation_id,
        operation_seq: item.operation_seq,
        operation_generation: item.generation,
        operation_status: item.status,
        abandon_state: item.abandon_state,
        expected_revision: item.expected_revision,
        dependency,
        payload_hash: item.payload_hash,
        payload: item.bytes,
        draft_generation: draft.generation,
        draft: draft.bytes,
        base: draft.base_bytes,
        base_revision: draft.base_revision,
        view_fingerprint: rig.approval.view_fingerprint.clone(),
    }
}

async fn successor_id(rig: &mut ApplyRig) -> String {
    let page = rig
        .store
        .list_outbox(&rig.approval.old, 0, 100)
        .await
        .unwrap();
    page.entries
        .into_iter()
        .find(|item| {
            item.document_id == DOC_A && item.operation_id != rig.approval.old_operation_id
        })
        .unwrap()
        .operation_id
}

async fn refreshed_draft_approval(rig: &mut ApplyRig) {
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    rig.approval.generation = draft.generation;
    rig.approval.draft = draft.bytes;
    rig.approval.base = draft.base_bytes;
    rig.approval.base_revision = draft.base_revision;
    rig.approval.view_fingerprint =
        "sha256:3333333333333333333333333333333333333333333333333333333333333333".into();
}

#[tokio::test]
async fn frozen_content_dismiss_followup_allows_confirmed_draft_after_only_other_operation_is_dismissed()
 {
    let mut rig = apply_rig_with_successor(true).await;
    let second_id = successor_id(&mut rig).await;
    let dismissed = approval_for_old_operation(&mut rig, &second_id).await;
    let before = snapshot(&rig.path).await;
    dismiss_frozen_content(&mut rig.store, &dismissed)
        .await
        .unwrap();
    assert_eq!(snapshot(&rig.path).await, before);
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    refreshed_draft_approval(&mut rig).await;
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let new = rig
        .store
        .read_outbox_operation(&rig.approval.current, &new_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.bytes, b"later local draft");
    assert_eq!(new.expected_revision.as_deref(), Some("remote-revision"));
    let sent = rig
        .store
        .prepare_outbox(&rig.approval.current)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent.operation_id, new_id);
    let old = rig
        .store
        .list_outbox(&rig.approval.old, 0, 100)
        .await
        .unwrap();
    assert!(
        old.entries
            .iter()
            .any(|item| item.operation_id == dismissed.operation_id
                && item.status == OutboxStatus::Queued)
    );
    assert!(
        old.entries
            .iter()
            .any(|item| item.operation_id == rig.approval.old_operation_id
                && item.status == OutboxStatus::Sent)
    );
    assert!(
        dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &dismissed.operation_id
        )
        .await
        .unwrap()
    );
    let after = snapshot(&rig.path).await;
    for row in &before.0 {
        assert!(after.0.contains(row));
    }
    for row in &before.1 {
        assert!(after.1.contains(row));
    }
    for row in &before.2 {
        assert!(after.2.contains(row));
    }
    for row in &before.3 {
        assert!(after.3.contains(row));
    }
}

#[tokio::test]
async fn frozen_content_dismiss_followup_does_not_release_unresolved_dependency_or_latest_draft() {
    let mut rig = apply_rig_with_successor(true).await;
    let second_id = successor_id(&mut rig).await;
    let predecessor_id = rig.approval.old_operation_id.clone();
    let predecessor = approval_for_old_operation(&mut rig, &predecessor_id).await;
    dismiss_frozen_content(&mut rig.store, &predecessor)
        .await
        .unwrap();
    let successor = rig
        .store
        .read_outbox_operation(&rig.approval.old, &second_id)
        .await
        .unwrap()
        .unwrap();
    let mut attempted = rig.approval.clone();
    attempted.old_operation_id = successor.operation_id;
    attempted.old_operation_seq = successor.operation_seq;
    attempted.old_operation_hash = successor.payload_hash;
    attempted.old_operation_status = successor.status;
    attempted.generation = 2;
    attempted.draft = b"later local draft".to_vec();
    let before = snapshot(&rig.path).await;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &attempted)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert!(
        !dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &second_id
        )
        .await
        .unwrap()
    );
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn frozen_content_dismiss_followup_rejects_corrupt_dismissal_without_creating_new_operation()
{
    let mut rig = apply_rig_with_successor(true).await;
    let second_id = successor_id(&mut rig).await;
    let dismissed = approval_for_old_operation(&mut rig, &second_id).await;
    dismiss_frozen_content(&mut rig.store, &dismissed)
        .await
        .unwrap();
    refreshed_draft_approval(&mut rig).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE frozen_content_dismissals SET history_hash='sha256:2222222222222222222222222222222222222222222222222222222222222222' WHERE old_operation_id=?")
        .bind(&second_id).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
}
