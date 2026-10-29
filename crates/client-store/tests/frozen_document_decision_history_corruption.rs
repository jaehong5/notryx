include!("frozen_document_decision_lookup.rs");

async fn history_state(path: &Path) -> (i64, Vec<u8>, String, String) {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let row = sqlx::query_as("SELECT generation,base_bytes,base_revision,approval_hash FROM frozen_content_reconciliations")
        .fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();
    row
}

async fn rejects_changed_history(column: &str) {
    let mut rig = apply_rig().await;
    apply_with_view_fingerprint(&mut rig.store, &rig.approval, VIEW_FINGERPRINT)
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
        "generation" => sqlx::query("UPDATE frozen_content_reconciliations SET generation=generation+1 WHERE old_operation_id=?")
            .bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
        "base_bytes" => sqlx::query("UPDATE frozen_content_reconciliations SET base_bytes=X'74616d7065726564' WHERE old_operation_id=?")
            .bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
        "base_revision" => sqlx::query("UPDATE frozen_content_reconciliations SET base_revision='other-revision' WHERE old_operation_id=?")
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
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    let history = history_state(&rig.path).await;
    assert!(
        lookup_frozen_content_decision(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
            VIEW_FINGERPRINT
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(history_state(&rig.path).await, history);
}

#[tokio::test]
async fn frozen_document_decision_history_corruption_rejects_generation() {
    rejects_changed_history("generation").await;
}
#[tokio::test]
async fn frozen_document_decision_history_corruption_rejects_base_bytes() {
    rejects_changed_history("base_bytes").await;
}
#[tokio::test]
async fn frozen_document_decision_history_corruption_rejects_base_revision() {
    rejects_changed_history("base_revision").await;
}

#[tokio::test]
async fn frozen_document_decision_history_corruption_rejects_approval_hash() {
    rejects_changed_history("approval_hash").await;
}
