include!("frozen_content_dismiss_followup.rs");

#[derive(Clone)]
struct DismissWithDependency {
    approval: DismissApproval,
    observed_dependency: Option<String>,
}

async fn dismiss_with_dependency(
    store: &mut ClientStore,
    approval: &DismissWithDependency,
) -> Result<(), StoreError> {
    let mut selected = approval.approval.clone();
    selected.dependency = approval.observed_dependency.clone();
    store.dismiss_frozen_content(&selected).await
}

async fn dependent_approval(rig: &mut ApplyRig) -> DismissWithDependency {
    let selected_id = successor_id(rig).await;
    let approval = approval_for_old_operation(rig, &selected_id).await;
    let rows = snapshot(&rig.path).await;
    let evidence = rows.1.iter().find(|row| row.0 == selected_id).unwrap();
    assert_eq!(
        evidence.3.as_deref(),
        Some(rig.approval.old_operation_id.as_str())
    );
    DismissWithDependency {
        approval,
        observed_dependency: evidence.3.clone(),
    }
}

#[tokio::test]
async fn frozen_content_dismiss_dependency_accepts_reviewed_dependency_and_preserves_sources() {
    let mut rig = apply_rig_with_successor(true).await;
    let approval = dependent_approval(&mut rig).await;
    let before = snapshot(&rig.path).await;
    let receipts = receipt_and_generation(&rig.path).await;
    dismiss_with_dependency(&mut rig.store, &approval)
        .await
        .unwrap();
    assert!(
        dismissal_status(
            &mut rig.store,
            &approval.approval.current,
            &approval.approval.old,
            DOC_A,
            &approval.approval.operation_id
        )
        .await
        .unwrap()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(receipt_and_generation(&rig.path).await, receipts);
    dismiss_with_dependency(&mut rig.store, &approval)
        .await
        .unwrap();
    assert_eq!(snapshot(&rig.path).await, before);
}

#[tokio::test]
async fn frozen_content_dismiss_dependency_rejects_changed_dependency_after_review() {
    let mut rig = apply_rig_with_successor(true).await;
    let approval = dependent_approval(&mut rig).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let changed = sqlx::query("UPDATE client_outbox SET dependency=NULL WHERE operation_id=?")
        .bind(&approval.approval.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    let foreign_errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(foreign_errors, 0);
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    let receipts = receipt_and_generation(&rig.path).await;
    assert!(
        dismiss_with_dependency(&mut rig.store, &approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(receipt_and_generation(&rig.path).await, receipts);
    assert!(
        !dismissal_status(
            &mut rig.store,
            &approval.approval.current,
            &approval.approval.old,
            DOC_A,
            &approval.approval.operation_id
        )
        .await
        .unwrap()
    );
}
