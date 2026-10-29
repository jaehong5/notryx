include!("frozen_document_review.rs");

async fn corrupt_status(rig: &Fixture, status: &str, abandon: &str, terminal: Option<&str>) {
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    let changed = sqlx::query(
        "UPDATE client_outbox SET status=?,abandon_state=?,terminal_kind=? WHERE operation_id=?",
    )
    .bind(status)
    .bind(abandon)
    .bind(terminal)
    .bind(&rig.sent.operation_id)
    .execute(&mut db)
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_document_status_integrity_rejects_sent_content_with_abandon_marker() {
    let mut rig = fixture().await;
    corrupt_status(&rig, "sent", "requested", None).await;
    let before = snapshot(&rig.path).await;
    assert!(
        read_frozen_document_review(&mut rig.store, &rig.current, &rig.old, DOC_A, 0, 100)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
}

#[tokio::test]
async fn frozen_document_status_integrity_rejects_structure_terminal_on_content() {
    let mut rig = fixture().await;
    corrupt_status(&rig, "conflict", "none", Some("parent_not_folder")).await;
    let before = snapshot(&rig.path).await;
    assert!(
        read_frozen_document_review(&mut rig.store, &rig.current, &rig.old, DOC_A, 0, 100)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
}
