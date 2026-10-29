include!("content_conflict_copy_intent.rs");

#[tokio::test]
async fn reviewed_keep_local_choice_can_record_copy_without_losing_prior_intent() {
    let (rig, mut store) = rig().await;
    let old = record(&mut store, &rig, &rig.request).await.unwrap();
    let request = copy_request(&rig);
    let reviewed = store
        .replace_pending_content_conflict_intent(&rig.binding, DOC, &rig.operation, &old, &request)
        .await
        .unwrap();
    assert_eq!(reviewed.version, 1);
    let before = next_sequence(&rig).await;
    assert!(
        store
            .record_content_conflict_copy(
                &rig.binding,
                DOC,
                &rig.operation,
                &request,
                ROOT,
                COPY_NAME,
                Some(0)
            )
            .await
            .is_err()
    );
    let copy = store
        .record_content_conflict_copy(
            &rig.binding,
            DOC,
            &rig.operation,
            &request,
            ROOT,
            COPY_NAME,
            Some(reviewed.version),
        )
        .await
        .unwrap();
    assert_eq!(copy.intent_version, reviewed.version);
    assert_eq!(copy.local_bytes, LOCAL);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(reviewed.clone())
    );
    assert_eq!(
        store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, old.version)
            .await
            .unwrap(),
        Some(old.clone())
    );
    assert_eq!(
        store
            .record_content_conflict_copy(
                &rig.binding,
                DOC,
                &rig.operation,
                &request,
                ROOT,
                COPY_NAME,
                Some(reviewed.version)
            )
            .await
            .unwrap(),
        copy
    );
    assert_eq!(next_sequence(&rig).await, before);
    assert_originals(&mut store, &rig).await;
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(
        reopened
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(copy)
    );
    assert_eq!(
        reopened
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, old.version)
            .await
            .unwrap(),
        Some(old)
    );
    assert_originals(&mut reopened, &rig).await;
}

#[tokio::test]
async fn old_initial_copy_confirmation_cannot_cross_a_keep_choice_aba() {
    let (rig, mut store) = rig().await;
    let request = copy_request(&rig);
    let first = record(&mut store, &rig, &request).await.unwrap();
    let middle = store
        .replace_pending_content_conflict_intent(
            &rig.binding,
            DOC,
            &rig.operation,
            &first,
            &rig.request,
        )
        .await
        .unwrap();
    let latest = store
        .replace_pending_content_conflict_intent(
            &rig.binding,
            DOC,
            &rig.operation,
            &middle,
            &request,
        )
        .await
        .unwrap();
    assert_eq!(latest.version, 2);
    assert!(
        record_copy(&mut store, &rig, &request, ROOT, COPY_NAME)
            .await
            .is_err()
    );
    assert!(
        store
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(latest.clone())
    );
    let copy = store
        .record_content_conflict_copy(
            &rig.binding,
            DOC,
            &rig.operation,
            &request,
            ROOT,
            COPY_NAME,
            Some(latest.version),
        )
        .await
        .unwrap();
    assert_eq!(copy.intent_version, 2);
    assert_eq!(copy.local_bytes, LOCAL);
    assert_originals(&mut store, &rig).await;
}
