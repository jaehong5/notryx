include!("frozen_content_dismiss.rs");

async fn lookup_dismissal(
    store: &mut ClientStore,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
    fingerprint: &str,
) -> Result<Option<()>, StoreError> {
    store
        .lookup_frozen_content_dismissal(current, old, document, operation_id, fingerprint)
        .await
}

async fn saved_dismissal_rows(path: &Path) -> Vec<(String, i64, String, String, String)> {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let rows = sqlx::query_as("SELECT old_operation_id,current_registration_id,approval_hash,view_fingerprint,history_hash FROM frozen_content_dismissals ORDER BY old_operation_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

#[tokio::test]
async fn frozen_content_dismiss_lookup_missing_returns_none_without_recording_decision() {
    let mut rig = dismiss_rig("sent", false).await;
    let before = snapshot(&rig.path).await;
    let decisions = saved_dismissal_rows(&rig.path).await;
    assert_eq!(
        lookup_dismissal(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id,
            &rig.approval.view_fingerprint
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(saved_dismissal_rows(&rig.path).await, decisions);
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn frozen_content_dismiss_lookup_matches_exact_fingerprint_after_edit_and_restart() {
    let mut rig = dismiss_rig("sent", false).await;
    dismiss_frozen_content(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let edit = rig
        .store
        .open_recovery_draft(&rig.approval.old, DOC_A)
        .await
        .unwrap();
    rig.store
        .save_recovery_checkpoint(
            &rig.approval.old,
            DOC_A,
            &edit.edit_session_id,
            3,
            b"later recovery edit",
        )
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    let decisions = saved_dismissal_rows(&rig.path).await;
    assert_eq!(
        lookup_dismissal(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id,
            &rig.approval.view_fingerprint
        )
        .await
        .unwrap(),
        Some(())
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(saved_dismissal_rows(&rig.path).await, decisions);
    rig.store
        .close_recovery_draft(&rig.approval.old, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    rig.store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            lookup_dismissal(
                &mut reopened,
                &rig.approval.current,
                &rig.approval.old,
                DOC_A,
                &rig.approval.operation_id,
                &rig.approval.view_fingerprint
            )
            .await
            .unwrap(),
            Some(())
        );
    }
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(saved_dismissal_rows(&rig.path).await, decisions);
    assert_eq!(
        reopened
            .read_registered_checkpoint(&rig.approval.old, DOC_A)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"later recovery edit"
    );
}

#[tokio::test]
async fn frozen_content_dismiss_lookup_rejects_different_fingerprint_or_binding() {
    let mut rig = dismiss_rig("sent", false).await;
    dismiss_frozen_content(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    let decisions = saved_dismissal_rows(&rig.path).await;
    let different = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    assert!(
        lookup_dismissal(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id,
            different
        )
        .await
        .is_err()
    );
    assert!(
        lookup_dismissal(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_B,
            &rig.approval.operation_id,
            &rig.approval.view_fingerprint
        )
        .await
        .is_err()
    );
    let foreign = binding(
        &Uuid::new_v4().to_string(),
        &rig.approval.current.workspace_epoch,
    );
    assert!(
        lookup_dismissal(
            &mut rig.store,
            &foreign,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id,
            &rig.approval.view_fingerprint
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(saved_dismissal_rows(&rig.path).await, decisions);
}

#[tokio::test]
async fn frozen_content_dismiss_lookup_rejects_corrupt_saved_decision_without_mutation() {
    let mut rig = dismiss_rig("sent", false).await;
    dismiss_frozen_content(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE frozen_content_dismissals SET base_revision='other-revision' WHERE old_operation_id=?")
        .bind(&rig.approval.operation_id).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    let decisions = saved_dismissal_rows(&rig.path).await;
    assert!(
        lookup_dismissal(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id,
            &rig.approval.view_fingerprint
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(saved_dismissal_rows(&rig.path).await, decisions);
}
