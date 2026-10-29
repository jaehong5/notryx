include!("content_conflict_intent_replace.rs");

#[tokio::test]
async fn single_history_read_returns_exact_old_versions_and_stays_stable_after_new_decisions() {
    let (rig, mut store) = rig().await;
    let (first, first_request) = reviewed(&mut store, &rig).await;
    assert_eq!(first.version, 0);
    let second = replace(&mut store, &rig, &first, &first_request)
        .await
        .unwrap();
    assert_eq!(second.version, 1);
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 0)
            .await
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 1)
            .await
            .unwrap(),
        None
    );
    let mut second_request = first_request.clone();
    second_request.choice = ContentConflictChoice::KeepLocalCopy;
    let third = replace(&mut store, &rig, &second, &second_request)
        .await
        .unwrap();
    assert_eq!(third.version, 2);
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 0)
            .await
            .unwrap(),
        Some(first)
    );
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 1)
            .await
            .unwrap(),
        Some(second)
    );
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 2)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 3)
            .await
            .unwrap(),
        None
    );
    let huge = store
        .read_content_conflict_intent_history(&rig.binding, &rig.operation, u64::MAX)
        .await;
    assert!(huge.is_err() || matches!(huge, Ok(None)));
    preserved_outbox(&mut store, &rig).await;
}

#[tokio::test]
async fn single_history_read_supports_frozen_binding_without_attaching_it_to_replacement() {
    let (rig, mut store) = rig().await;
    let (first, request) = reviewed(&mut store, &rig).await;
    replace(&mut store, &rig, &first, &request).await.unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = rig.binding.workspace_id.clone();
    replacement.workspace_epoch = rig.binding.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 0)
            .await
            .unwrap(),
        Some(first)
    );
    assert_eq!(
        store
            .read_content_conflict_intent_history(&replacement, &rig.operation, 0)
            .await
            .unwrap(),
        None
    );
    assert!(
        store
            .read_content_conflict_intent_history(&binding(), &rig.operation, 0)
            .await
            .is_err()
    );
    assert!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .frozen
    );
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn single_history_read_rejects_missing_or_corrupt_evidence_without_modifying_draft() {
    for remove in [false, true] {
        let (rig, mut store) = rig().await;
        let (first, request) = reviewed(&mut store, &rig).await;
        let current = replace(&mut store, &rig, &first, &request).await.unwrap();
        let mut db = connect(&rig.path).await;
        if remove {
            sqlx::query("DELETE FROM content_conflict_intent_history WHERE operation_id=? AND history_seq=1")
                .bind(&rig.operation).execute(&mut db).await.unwrap();
        } else {
            sqlx::query("UPDATE content_conflict_intent_history SET remote_hash='sha256:invalid' WHERE operation_id=? AND history_seq=1")
                .bind(&rig.operation).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        assert!(
            store
                .read_content_conflict_intent_history(&rig.binding, &rig.operation, 0)
                .await
                .is_err(),
            "remove={remove}"
        );
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(current)
        );
        preserved_outbox(&mut store, &rig).await;
    }
}
