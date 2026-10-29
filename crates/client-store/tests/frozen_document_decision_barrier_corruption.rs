include!("frozen_document_apply.rs");

async fn verify_corrupt_decision_keeps_barrier(column: &str) {
    let mut rig = apply_rig().await;
    let id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    match column {
        "history_hash" => sqlx::query("UPDATE frozen_content_reconciliations SET history_hash='sha256:2222222222222222222222222222222222222222222222222222222222222222' WHERE old_operation_id=?")
            .bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
        "view_fingerprint" => sqlx::query("UPDATE frozen_content_reconciliations SET view_fingerprint='sha256:2222222222222222222222222222222222222222222222222222222222222222' WHERE old_operation_id=?")
            .bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
        "approval_hash" => sqlx::query("UPDATE frozen_content_reconciliations SET approval_hash='sha256:2222222222222222222222222222222222222222222222222222222222222222' WHERE old_operation_id=?")
            .bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
        _ => unreachable!(),
    };
    let foreign_errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(foreign_errors, 0);
    let before_decision: (String, String, String) = sqlx::query_as("SELECT history_hash,view_fingerprint,approval_hash FROM frozen_content_reconciliations WHERE old_operation_id=?")
        .bind(&rig.approval.old_operation_id).fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        rig.store
            .prepare_outbox(&rig.approval.current)
            .await
            .is_err(),
        "mode {column}"
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let pending = rig
        .store
        .read_outbox_operation(&rig.approval.current, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.status, OutboxStatus::Queued);
    assert_eq!(pending.bytes, rig.approval.draft);
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .read_only(true),
    )
    .await
    .unwrap();
    let after_decision: (String, String, String) = sqlx::query_as("SELECT history_hash,view_fingerprint,approval_hash FROM frozen_content_reconciliations WHERE old_operation_id=?")
        .bind(&rig.approval.old_operation_id).fetch_one(&mut db).await.unwrap();
    assert_eq!(after_decision, before_decision);
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_document_decision_barrier_corruption_history_hash() {
    verify_corrupt_decision_keeps_barrier("history_hash").await;
}
#[tokio::test]
async fn frozen_document_decision_barrier_corruption_view_fingerprint() {
    verify_corrupt_decision_keeps_barrier("view_fingerprint").await;
}
#[tokio::test]
async fn frozen_document_decision_barrier_corruption_approval_hash() {
    verify_corrupt_decision_keeps_barrier("approval_hash").await;
}
