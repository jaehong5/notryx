include!("frozen_document_apply.rs");

#[tokio::test]
async fn frozen_document_missing_mapping_does_not_release_old_content_barrier() {
    let mut rig = apply_rig().await;
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(false),
    )
    .await
    .unwrap();
    let changed = sqlx::query("DELETE FROM registered_documents WHERE registration_id=(SELECT id FROM client_registrations WHERE device_stream_id=?) AND document_id=?")
        .bind(&rig.approval.old.device_stream_id).bind(DOC_A).execute(&mut db).await.unwrap();
    assert_eq!(changed.rows_affected(), 1);
    let missing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert!(missing > 0);
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    assert!(matches!(
        rig.store.prepare_outbox(&rig.approval.current).await,
        Err(StoreError::ReconciliationRequired)
    ));
    assert_eq!(snapshot(&rig.path).await, before);
    let pending = rig
        .store
        .read_outbox_operation(&rig.approval.current, &new_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.status, OutboxStatus::Queued);
    assert_eq!(pending.bytes, rig.approval.draft);
}
