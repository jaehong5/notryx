include!("frozen_document_apply_retry.rs");

use notryx_client_store::FrozenContentDecisionStatus as SavedDecisionStatus;

async fn frozen_content_decision_status(
    store: &mut ClientStore,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    old_operation_id: &str,
) -> Result<Option<SavedDecisionStatus>, StoreError> {
    store
        .frozen_content_decision_status(current, old, document, old_operation_id)
        .await
}

#[tokio::test]
async fn frozen_document_decision_status_no_record_does_not_create_anything() {
    let mut rig = apply_rig().await;
    let before = snapshot(&rig.path).await;
    assert_eq!(
        frozen_content_decision_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id
        )
        .await
        .unwrap(),
        None
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

#[tokio::test]
async fn frozen_document_decision_status_is_read_only_after_restart_and_later_edit() {
    let mut rig = apply_rig().await;
    let id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let first = rig
        .store
        .read_outbox_operation(&rig.approval.current, &id)
        .await
        .unwrap()
        .unwrap();
    let edit = rig
        .store
        .open_registered_document(
            &rig.approval.current,
            DOC_A,
            &rig.approval.remote_bytes,
            &rig.approval.remote_revision,
        )
        .await
        .unwrap();
    rig.store
        .save_registered_checkpoint(
            &rig.approval.current,
            DOC_A,
            &edit.edit_session_id,
            2,
            b"later durable edit",
        )
        .await
        .unwrap();
    rig.store
        .close_registered_document(&rig.approval.current, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    rig.store.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    let before = snapshot(&rig.path).await;
    let status = frozen_content_decision_status(
        &mut store,
        &rig.approval.current,
        &rig.approval.old,
        DOC_A,
        &rig.approval.old_operation_id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(status.operation_id, id);
    assert_eq!(status.operation_seq, first.operation_seq);
    assert_eq!(status.status, OutboxStatus::Queued);
    assert_eq!(status.approved_generation, rig.approval.generation);
    assert_eq!(status.approved_hash, first.payload_hash);
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(
        store
            .read_registered_checkpoint(&rig.approval.current, DOC_A)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"later durable edit"
    );
}

#[tokio::test]
async fn frozen_document_decision_status_rejects_other_workspace_and_corrupt_history() {
    let mut rig = apply_rig().await;
    apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let foreign = binding(
        &Uuid::new_v4().to_string(),
        &rig.approval.current.workspace_epoch,
    );
    assert!(
        frozen_content_decision_status(
            &mut rig.store,
            &foreign,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id
        )
        .await
        .is_err()
    );
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE frozen_content_reconciliations SET base_revision='other' WHERE old_operation_id=?",
    )
    .bind(&rig.approval.old_operation_id)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        frozen_content_decision_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
}
