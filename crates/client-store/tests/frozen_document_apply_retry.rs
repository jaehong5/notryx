include!("frozen_document_apply.rs");

use notryx_client_store::OutboxCommit;

fn receipt(item: &notryx_client_store::OutboxOperation, revision: &str) -> OutboxCommit {
    OutboxCommit {
        operation_id: item.operation_id.clone(),
        operation_seq: item.operation_seq,
        workspace_epoch: item.binding.workspace_epoch.clone(),
        document_id: item.document_id.clone(),
        content_revision: revision.into(),
        content_hash: item.payload_hash.clone(),
    }
}

#[tokio::test]
async fn frozen_document_apply_retry_returns_existing_id_while_later_session_is_open() {
    let mut rig = apply_rig().await;
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
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
    let before = snapshot(&rig.path).await;
    assert_eq!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .unwrap(),
        new_id
    );
    assert_eq!(snapshot(&rig.path).await, before);
    rig.store
        .close_registered_document(&rig.approval.current, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn frozen_document_apply_retry_returns_existing_id_after_normal_ack_and_payload_retirement() {
    let mut rig = apply_rig().await;
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let first = rig
        .store
        .prepare_outbox(&rig.approval.current)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.operation_id, new_id);
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
    assert!(matches!(
        rig.store
            .read_outbox_operation(&rig.approval.current, &new_id)
            .await,
        Err(StoreError::PayloadRetired)
    ));
    let before = snapshot(&rig.path).await;
    assert_eq!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .unwrap(),
        new_id
    );
    assert_eq!(snapshot(&rig.path).await, before);
}
