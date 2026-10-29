include!("frozen_document_apply_retry.rs");

const VIEW_FINGERPRINT: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const OTHER_FINGERPRINT: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

use notryx_client_store::FrozenContentDecision as DecisionLookup;

async fn apply_with_view_fingerprint(
    store: &mut ClientStore,
    approval: &ApplyApproval,
    fingerprint: &str,
) -> Result<String, StoreError> {
    let mut approval = approval.clone();
    approval.view_fingerprint = fingerprint.to_owned();
    apply_confirmed_frozen_draft(store, &approval).await
}

async fn lookup_frozen_content_decision(
    store: &mut ClientStore,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
    fingerprint: &str,
) -> Result<Option<DecisionLookup>, StoreError> {
    store
        .lookup_frozen_content_decision(current, old, document, operation_id, fingerprint)
        .await
}

async fn decisions(path: &Path) -> Vec<(String, i64, String, String)> {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let rows = sqlx::query_as("SELECT old_operation_id,current_registration_id,new_operation_id,approval_hash FROM frozen_content_reconciliations ORDER BY old_operation_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

#[tokio::test]
async fn frozen_document_decision_lookup_missing_is_read_only_and_does_not_approve() {
    let mut rig = apply_rig().await;
    let before = snapshot(&rig.path).await;
    let before_decisions = decisions(&rig.path).await;
    assert_eq!(
        lookup_frozen_content_decision(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
            VIEW_FINGERPRINT
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(decisions(&rig.path).await, before_decisions);
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn frozen_document_decision_lookup_survives_restart_and_later_durable_edit() {
    let mut rig = apply_rig().await;
    let id = apply_with_view_fingerprint(&mut rig.store, &rig.approval, VIEW_FINGERPRINT)
        .await
        .unwrap();
    let session = rig
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
            &session.edit_session_id,
            2,
            b"later durable edit",
        )
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    let before_decisions = decisions(&rig.path).await;
    let expected = Some(DecisionLookup {
        operation_id: id.clone(),
        operation_seq: 1,
        status: OutboxStatus::Queued,
    });
    assert_eq!(
        lookup_frozen_content_decision(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
            VIEW_FINGERPRINT
        )
        .await
        .unwrap(),
        expected
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(decisions(&rig.path).await, before_decisions);
    rig.store
        .close_registered_document(&rig.approval.current, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
    rig.store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    let restart_snapshot = snapshot(&rig.path).await;
    let restart_decisions = decisions(&rig.path).await;
    for _ in 0..2 {
        assert_eq!(
            lookup_frozen_content_decision(
                &mut reopened,
                &rig.approval.current,
                &rig.approval.old,
                DOC_A,
                &rig.approval.old_operation_id,
                VIEW_FINGERPRINT
            )
            .await
            .unwrap(),
            expected
        );
    }
    assert_eq!(snapshot(&rig.path).await, restart_snapshot);
    assert_eq!(decisions(&rig.path).await, restart_decisions);
    assert_eq!(
        reopened
            .read_registered_checkpoint(&rig.approval.current, DOC_A)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"later durable edit"
    );
}

#[tokio::test]
async fn frozen_document_decision_lookup_rejects_different_fingerprint_and_binding_without_mutation()
 {
    let mut rig = apply_rig().await;
    apply_with_view_fingerprint(&mut rig.store, &rig.approval, VIEW_FINGERPRINT)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    let before_decisions = decisions(&rig.path).await;
    assert!(
        lookup_frozen_content_decision(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
            OTHER_FINGERPRINT
        )
        .await
        .is_err()
    );
    assert!(
        lookup_frozen_content_decision(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_C,
            &rig.approval.old_operation_id,
            VIEW_FINGERPRINT
        )
        .await
        .is_err()
    );
    let other = binding(
        &Uuid::new_v4().to_string(),
        &rig.approval.current.workspace_epoch,
    );
    assert!(
        lookup_frozen_content_decision(
            &mut rig.store,
            &other,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
            VIEW_FINGERPRINT
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(decisions(&rig.path).await, before_decisions);
}

#[tokio::test]
async fn frozen_document_decision_lookup_rejects_link_to_other_operation_without_mutation() {
    let mut rig = apply_rig().await;
    apply_with_view_fingerprint(&mut rig.store, &rig.approval, VIEW_FINGERPRINT)
        .await
        .unwrap();
    let other = queue(
        &mut rig.store,
        &rig.approval.current,
        DOC_C,
        b"unrelated edit",
    )
    .await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE frozen_content_reconciliations SET new_operation_id=? WHERE old_operation_id=?",
    )
    .bind(&other.operation_id)
    .bind(&rig.approval.old_operation_id)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    let before_decisions = decisions(&rig.path).await;
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
    assert_eq!(decisions(&rig.path).await, before_decisions);
}

#[tokio::test]
async fn frozen_document_decision_lookup_returns_committed_status_after_normal_payload_retirement()
{
    let mut rig = apply_rig().await;
    let id = apply_with_view_fingerprint(&mut rig.store, &rig.approval, VIEW_FINGERPRINT)
        .await
        .unwrap();
    let first = rig
        .store
        .prepare_outbox(&rig.approval.current)
        .await
        .unwrap()
        .unwrap();
    rig.store
        .apply_outbox_commit(&rig.approval.current, &receipt(&first, "committed-1"))
        .await
        .unwrap();
    let session = rig
        .store
        .open_registered_document(&rig.approval.current, DOC_A, &first.bytes, "committed-1")
        .await
        .unwrap();
    rig.store
        .save_registered_checkpoint(
            &rig.approval.current,
            DOC_A,
            &session.edit_session_id,
            2,
            b"later committed edit",
        )
        .await
        .unwrap();
    rig.store
        .queue_checkpoint(&rig.approval.current, DOC_A, 2)
        .await
        .unwrap();
    let second = rig
        .store
        .prepare_outbox(&rig.approval.current)
        .await
        .unwrap()
        .unwrap();
    rig.store
        .apply_outbox_commit(&rig.approval.current, &receipt(&second, "committed-2"))
        .await
        .unwrap();
    rig.store
        .close_registered_document(&rig.approval.current, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({"workspace_epoch":rig.approval.current.workspace_epoch,"device_stream_id":rig.approval.current.device_stream_id,"last_seq":"2","retired_through":"2"})).unwrap();
    rig.store
        .confirm_receipt_ack(&rig.approval.current, &ack)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    let before_decisions = decisions(&rig.path).await;
    assert_eq!(
        lookup_frozen_content_decision(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.old_operation_id,
            VIEW_FINGERPRINT
        )
        .await
        .unwrap(),
        Some(DecisionLookup {
            operation_id: id,
            operation_seq: 1,
            status: OutboxStatus::Committed
        })
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(decisions(&rig.path).await, before_decisions);
}
