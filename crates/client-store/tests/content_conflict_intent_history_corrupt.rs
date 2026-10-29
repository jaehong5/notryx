include!("content_conflict_intent_replace.rs");

#[tokio::test]
async fn missing_last_or_all_history_is_rejected_before_read_or_replacement() {
    for remove_all in [false, true] {
        let (rig, mut store) = rig().await;
        let (old, request) = reviewed(&mut store, &rig).await;
        let second = replace(&mut store, &rig, &old, &request).await.unwrap();
        let mut third_request = request.clone();
        third_request.choice = ContentConflictChoice::KeepLocalCopy;
        let current = replace(&mut store, &rig, &second, &third_request)
            .await
            .unwrap();
        assert_eq!(history(&mut store, &rig).await, vec![old, second]);
        let mut db = connect(&rig.path).await;
        let sql = if remove_all {
            "DELETE FROM content_conflict_intent_history WHERE operation_id=?"
        } else {
            "DELETE FROM content_conflict_intent_history WHERE operation_id=? AND history_seq=(SELECT MAX(history_seq) FROM content_conflict_intent_history WHERE operation_id=?)"
        };
        if remove_all {
            sqlx::query(sql)
                .bind(&rig.operation)
                .execute(&mut db)
                .await
                .unwrap();
        } else {
            sqlx::query(sql)
                .bind(&rig.operation)
                .bind(&rig.operation)
                .execute(&mut db)
                .await
                .unwrap();
        }
        db.close().await.unwrap();
        let listed = store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation, 0)
            .await;
        let mut new_request = third_request.clone();
        new_request.choice = ContentConflictChoice::SaveMerged {
            bytes: b"fresh merged\n".to_vec(),
        };
        let replaced = replace(&mut store, &rig, &current, &new_request).await;
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(current)
        );
        preserved_outbox(&mut store, &rig).await;
        assert!(
            listed.is_err() && replaced.is_err(),
            "remove_all={remove_all}, list_error={}, replace_error={}",
            listed.is_err(),
            replaced.is_err()
        );
    }
}

#[tokio::test]
async fn stale_expected_version_cannot_replay_after_same_generation_choice_returns_to_a() {
    let (rig, mut store) = rig().await;
    let (initial, choice_a) = reviewed(&mut store, &rig).await;
    let original_a = replace(&mut store, &rig, &initial, &choice_a)
        .await
        .unwrap();
    let mut choice_b = choice_a.clone();
    choice_b.choice = ContentConflictChoice::KeepLocalCopy;
    let current_b = replace(&mut store, &rig, &original_a, &choice_b)
        .await
        .unwrap();
    let current_a = replace(&mut store, &rig, &current_b, &choice_a)
        .await
        .unwrap();
    assert_eq!(current_a.generation, original_a.generation);
    assert_eq!(current_a.choice, original_a.choice);
    assert_eq!(current_a.local_bytes, original_a.local_bytes);
    assert_ne!(current_a, original_a);
    assert!(
        replace(&mut store, &rig, &original_a, &choice_b)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(current_a)
    );
    assert_eq!(
        history(&mut store, &rig).await,
        vec![initial, original_a, current_b]
    );
    preserved_outbox(&mut store, &rig).await;
}
