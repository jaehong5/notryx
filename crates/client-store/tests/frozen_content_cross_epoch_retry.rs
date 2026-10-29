include!("frozen_content_cross_epoch_apply.rs");

fn changed_confirmations(original: &CrossEpochConfirmation) -> Vec<CrossEpochConfirmation> {
    let mut values = Vec::new();
    let mut item = original.clone();
    item.selected_current_document = DOC_B.into();
    values.push(item);
    let mut item = original.clone();
    item.current_parent = DOC_B.into();
    values.push(item);
    let mut item = original.clone();
    item.current_name = "other.md".into();
    values.push(item);
    let mut item = original.clone();
    item.displayed_path = "/other.md".into();
    values.push(item);
    let mut item = original.clone();
    item.old_epoch = original.current_epoch.clone();
    values.push(item);
    let mut item = original.clone();
    item.current_epoch = original.old_epoch.clone();
    values.push(item);
    let mut item = original.clone();
    item.remote_revision = "other-revision".into();
    values.push(item);
    let mut item = original.clone();
    item.remote_hash =
        "sha256:2222222222222222222222222222222222222222222222222222222222222222".into();
    values.push(item);
    let mut item = original.clone();
    item.explicitly_confirmed = false;
    values.push(item);
    values
}

#[tokio::test]
async fn frozen_content_cross_epoch_retry_rejects_changed_confirmation_after_decision() {
    let (mut rig, confirmation) = cross_epoch_rig(false).await;
    let id = apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    let node = indexed_node(&rig.path).await;
    for changed in changed_confirmations(&confirmation) {
        assert!(
            apply_across_epochs(&mut rig.store, &rig.approval, Some(&changed))
                .await
                .is_err()
        );
        assert_eq!(snapshot(&rig.path).await, before);
    }
    assert_eq!(indexed_node(&rig.path).await, node);
    assert_eq!(
        apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
            .await
            .unwrap(),
        id
    );
    assert_eq!(snapshot(&rig.path).await, before);
}

#[tokio::test]
async fn frozen_content_cross_epoch_retry_after_restart_rejects_changed_confirmation() {
    let (mut rig, confirmation) = cross_epoch_rig(false).await;
    let id = apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
        .await
        .unwrap();
    rig.store.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    let before = snapshot(&rig.path).await;
    for changed in changed_confirmations(&confirmation) {
        assert!(
            apply_across_epochs(&mut store, &rig.approval, Some(&changed))
                .await
                .is_err()
        );
        assert_eq!(snapshot(&rig.path).await, before);
    }
    assert_eq!(
        apply_across_epochs(&mut store, &rig.approval, Some(&confirmation))
            .await
            .unwrap(),
        id
    );
    assert_eq!(snapshot(&rig.path).await, before);
}
