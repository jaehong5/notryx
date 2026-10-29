include!("frozen_document_review.rs");

async fn redirect_sent_storage_to_other_registration(rig: &Fixture) {
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    let changed = sqlx::query("UPDATE client_outbox SET storage_id=(SELECT storage_id FROM client_outbox WHERE operation_id=?) WHERE operation_id=?")
        .bind(&rig.foreign_item.operation_id)
        .bind(&rig.sent.operation_id)
        .execute(&mut db).await.unwrap();
    assert_eq!(changed.rows_affected(), 1);
    db.close().await.unwrap();
}

async fn storage_ids(path: &Path) -> Vec<(String, Option<String>)> {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let rows =
        sqlx::query_as("SELECT operation_id,storage_id FROM client_outbox ORDER BY operation_id")
            .fetch_all(&mut db)
            .await
            .unwrap();
    db.close().await.unwrap();
    rows
}

#[tokio::test]
async fn frozen_document_storage_integrity_summary_rejects_cross_registration_storage() {
    let mut rig = fixture().await;
    redirect_sent_storage_to_other_registration(&rig).await;
    let before = snapshot(&rig.path).await;
    let before_storage = storage_ids(&rig.path).await;
    assert!(
        read_frozen_document_review(&mut rig.store, &rig.current, &rig.old, DOC_A, 0, 100)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(storage_ids(&rig.path).await, before_storage);
}

#[tokio::test]
async fn frozen_document_storage_integrity_selected_rejects_cross_registration_storage() {
    let mut rig = fixture().await;
    redirect_sent_storage_to_other_registration(&rig).await;
    let before = snapshot(&rig.path).await;
    let before_storage = storage_ids(&rig.path).await;
    assert!(
        read_frozen_document_operation(
            &mut rig.store,
            &rig.current,
            &rig.old,
            DOC_A,
            &rig.sent.operation_id
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(storage_ids(&rig.path).await, before_storage);
}
