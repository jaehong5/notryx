include!("frozen_document_apply.rs");

#[tokio::test]
async fn frozen_document_decision_link_integrity_rejects_other_valid_active_operation() {
    let mut rig = apply_rig().await;
    let approved_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let unrelated = queue(
        &mut rig.store,
        &rig.approval.current,
        DOC_C,
        b"unrelated active edit",
    )
    .await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let changed = sqlx::query(
        "UPDATE frozen_content_reconciliations SET new_operation_id=? WHERE old_operation_id=?",
    )
    .bind(&unrelated.operation_id)
    .bind(&rig.approval.old_operation_id)
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
    let before = snapshot(&rig.path).await;
    assert!(matches!(
        rig.store.prepare_outbox(&rig.approval.current).await,
        Err(StoreError::ReconciliationRequired)
    ));
    assert_eq!(snapshot(&rig.path).await, before);
    let approved = rig
        .store
        .read_outbox_operation(&rig.approval.current, &approved_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(approved.status, OutboxStatus::Queued);
    assert_eq!(approved.bytes, rig.approval.draft);
}
