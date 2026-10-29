include!("frozen_document_apply.rs");

#[tokio::test]
async fn frozen_document_apply_empty_registration_reuses_closed_unmodified_document() {
    let mut rig = apply_rig().await;
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
        .close_registered_document(&rig.approval.current, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
    assert!(
        rig.store
            .read_registered_checkpoint(&rig.approval.current, DOC_A)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        rig.store
            .list_outbox(&rig.approval.current, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let before = snapshot(&rig.path).await;
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let new = rig
        .store
        .read_outbox_operation(&rig.approval.current, &new_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.bytes, rig.approval.draft);
    assert_eq!(
        new.expected_revision.as_deref(),
        Some(rig.approval.remote_revision.as_str())
    );
    let after = snapshot(&rig.path).await;
    assert!(
        after.0.contains(
            before
                .0
                .iter()
                .find(|row| row.0 == rig.approval.old_operation_id)
                .unwrap()
        )
    );
    assert!(
        after.1.contains(
            before
                .1
                .iter()
                .find(|row| row.0 == rig.approval.old_operation_id)
                .unwrap()
        )
    );
    for row in &before.2 {
        assert!(after.2.contains(row));
    }
    for row in &before.3 {
        assert!(after.3.contains(row));
    }
}
