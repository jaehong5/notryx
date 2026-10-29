include!("content_conflict_choose_remote_guards.rs");

use notryx_client_store::ContentConflictResolutionState;

#[tokio::test]
async fn regressed_or_changed_same_generation_is_not_a_normal_pending_review() {
    for case in ["regressed", "same_generation_changed"] {
        let (rig, mut store) = rig().await;
        let intent = record_remote_choice(&mut store, &rig).await;
        assert_eq!(intent.generation, 2);
        store
            .close_registered_document(&rig.binding, DOC, &rig.session)
            .await
            .unwrap();
        assert!(matches!(
            store
                .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            ContentConflictResolutionState::Pending {
                needs_review: false,
                ..
            }
        ));
        let original = store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap();
        let followup = store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.bytes, SENT);
        assert_eq!(followup.bytes, LOCAL);
        let (generation, bytes): (i64, &[u8]) = if case == "regressed" {
            (1, SENT)
        } else {
            (2, b"\xef\xbb\xbfdifferent bytes at same generation\n")
        };
        let mut db = connect(&rig.path).await;
        let changed = sqlx::query("UPDATE draft_checkpoints SET generation=?,bytes=? WHERE document_id=(SELECT storage_id FROM registered_documents WHERE document_id=?)")
            .bind(generation).bind(bytes).bind(DOC).execute(&mut db).await.unwrap().rows_affected();
        assert_eq!(changed, 1);
        db.close().await.unwrap();
        store.close().await.unwrap();
        let mut store = ClientStore::open(&rig.path).await.unwrap();
        let before = store
            .read_registered_checkpoint(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.generation, generation as u64);
        assert_eq!(before.bytes, bytes);
        assert_eq!(before.base_bytes, BASE);
        assert_eq!(before.base_revision, "base-rev");

        let result = store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await;
        assert!(
            matches!(result, Err(StoreError::UnsupportedFormat)),
            "case={case}, result={result:?}"
        );
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(intent)
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&rig.binding, DOC)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            store
                .read_outbox_operation(&rig.binding, &rig.operation)
                .await
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(
            store
                .read_outbox_operation(&rig.binding, &rig.followup)
                .await
                .unwrap()
                .unwrap(),
            followup
        );
    }
}
