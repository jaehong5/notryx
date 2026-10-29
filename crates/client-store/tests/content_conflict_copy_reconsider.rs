include!("content_conflict_copy_intent.rs");

#[tokio::test]
async fn pending_copy_cannot_be_replaced_and_make_the_confirmed_copy_unreadable() {
    let (rig, mut store) = rig().await;
    let request = copy_request(&rig);
    let copy = record_copy(&mut store, &rig, &request, ROOT, COPY_NAME)
        .await
        .unwrap();
    let old = store
        .read_content_conflict_intent(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"later edit\n")
        .await
        .unwrap();
    let mut changed = request.clone();
    changed.generation = 3;
    changed.choice = ContentConflictChoice::ChooseRemote;
    assert!(
        store
            .replace_pending_content_conflict_intent(
                &rig.binding,
                DOC,
                &rig.operation,
                &old,
                &changed
            )
            .await
            .is_err()
    );
    assert_eq!(
        store
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(copy)
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(old)
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later edit\n");
    assert_eq!(draft.base_bytes, BASE);
}
