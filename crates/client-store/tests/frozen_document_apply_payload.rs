include!("frozen_document_apply.rs");

#[tokio::test]
async fn frozen_document_apply_payload_remains_fixed_when_new_edit_is_queued() {
    let mut rig = apply_rig().await;
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let first = rig
        .store
        .read_outbox_operation(&rig.approval.current, &new_id)
        .await
        .unwrap()
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
            b"newer edit",
        )
        .await
        .unwrap();
    let followup = rig
        .store
        .queue_checkpoint(&rig.approval.current, DOC_A, 2)
        .await
        .unwrap();
    rig.store
        .close_registered_document(&rig.approval.current, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
    assert_ne!(followup.operation_id, new_id);
    assert_eq!(followup.operation_seq, first.operation_seq + 1);
    assert_eq!(followup.bytes, b"newer edit");
    assert_eq!(followup.expected_revision, None);
    let still_first = rig
        .store
        .read_outbox_operation(&rig.approval.current, &new_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still_first, first);
    let rows = snapshot(&rig.path).await;
    let next = rows
        .1
        .iter()
        .find(|row| row.0 == followup.operation_id)
        .unwrap();
    assert_eq!(next.3.as_deref(), Some(new_id.as_str()));
}
