include!("frozen_content_dismiss_lookup.rs");

async fn create_conflicting_decision_rows(rig: &mut ApplyRig) {
    apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let operation = rig
        .store
        .read_outbox_operation(&rig.approval.old, &rig.other_operation_id)
        .await
        .unwrap()
        .unwrap();
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.old, DOC_B)
        .await
        .unwrap()
        .unwrap();
    let other = DismissApproval {
        current: rig.approval.current.clone(),
        old: rig.approval.old.clone(),
        document: DOC_B.into(),
        operation_id: operation.operation_id,
        operation_seq: operation.operation_seq,
        operation_generation: operation.generation,
        operation_status: operation.status,
        abandon_state: operation.abandon_state,
        expected_revision: operation.expected_revision,
        dependency: None,
        payload_hash: operation.payload_hash,
        payload: operation.bytes,
        draft_generation: draft.generation,
        draft: draft.bytes,
        base: draft.base_bytes,
        base_revision: draft.base_revision,
        view_fingerprint: rig.approval.view_fingerprint.clone(),
    };
    dismiss_frozen_content(&mut rig.store, &other)
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let changed = sqlx::query(
        "UPDATE frozen_content_dismissals SET old_operation_id=? WHERE old_operation_id=?",
    )
    .bind(&rig.approval.old_operation_id)
    .bind(&rig.other_operation_id)
    .execute(&mut db)
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(errors, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_content_decision_collision_status_rejects_apply_and_dismiss_rows_for_same_operation()
 {
    let mut rig = apply_rig().await;
    create_conflicting_decision_rows(&mut rig).await;
    let before = snapshot(&rig.path).await;
    let dismissals = saved_dismissal_rows(&rig.path).await;
    let result = rig
        .store
        .frozen_content_decision_status(
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(saved_dismissal_rows(&rig.path).await, dismissals);
}

#[tokio::test]
async fn frozen_content_decision_collision_lookup_rejects_apply_and_dismiss_rows_for_same_operation()
 {
    let mut rig = apply_rig().await;
    create_conflicting_decision_rows(&mut rig).await;
    let before = snapshot(&rig.path).await;
    let dismissals = saved_dismissal_rows(&rig.path).await;
    let result = rig
        .store
        .lookup_frozen_content_decision(
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
            &rig.approval.view_fingerprint,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(saved_dismissal_rows(&rig.path).await, dismissals);
}
