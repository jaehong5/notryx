include!("frozen_content_existing_listing.rs");

use notryx_client_store::{
    FrozenDocumentOperationKind, FrozenDocumentReviewPage, OutboxAbandonState, StoreError,
};

async fn read_frozen_document_review(
    store: &mut ClientStore,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    after: u64,
    limit: u16,
) -> Result<FrozenDocumentReviewPage, StoreError> {
    store
        .frozen_document_review(current, old, document, after, limit)
        .await
}

async fn read_frozen_document_operation(
    store: &mut ClientStore,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
) -> Result<Vec<u8>, StoreError> {
    store
        .frozen_document_operation_bytes(current, old, document, operation_id)
        .await
}

#[tokio::test]
async fn frozen_document_review_enumerates_ordered_operations_dependencies_and_newer_draft_without_mutation()
 {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("frozen-document-review.sqlite");
    let mut store = ClientStore::open(&path).await.unwrap();
    let workspace = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let old = binding(&workspace, &epoch);
    store.activate_registration(&old).await.unwrap();
    let edit = store
        .open_registered_document(&old, DOC_A, b"base", "base-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, DOC_A, &edit.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    store.queue_checkpoint(&old, DOC_A, 1).await.unwrap();
    let sent = store.prepare_outbox(&old).await.unwrap().unwrap();
    let restore_request = RestoreNodeRequest {
        expected_meta_rev: notryx_domain::MetaRevision::new("deleted-meta").unwrap(),
        parent_id: ROOT.parse().unwrap(),
        name: "restored.md".into(),
    };
    let restore = store
        .queue_restore(&old, DOC_A.parse().unwrap(), &restore_request)
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, DOC_A, &edit.edit_session_id, 2, b"queued")
        .await
        .unwrap();
    let queued = store.queue_checkpoint(&old, DOC_A, 2).await.unwrap();
    store
        .close_registered_document(&old, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    let current = binding(&workspace, &epoch);
    store.activate_registration(&current).await.unwrap();
    let recovery = store.open_recovery_draft(&old, DOC_A).await.unwrap();
    store
        .save_recovery_checkpoint(&old, DOC_A, &recovery.edit_session_id, 3, b"newer draft")
        .await
        .unwrap();
    store
        .close_recovery_draft(&old, DOC_A, &recovery.edit_session_id)
        .await
        .unwrap();
    let before = snapshot(&path).await;

    let review = read_frozen_document_review(&mut store, &current, &old, DOC_A, 0, 2)
        .await
        .unwrap();
    assert_eq!(review.old_binding, old);
    assert_eq!(review.current_binding, current);
    assert_eq!(review.document_id, DOC_A);
    let draft = review.draft.unwrap();
    assert_eq!(draft.generation, 3);
    assert_eq!(draft.bytes, b"newer draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "base-revision");
    assert_eq!(review.operations.len(), 2);
    assert_eq!(review.next_after_seq, Some(2));
    assert!(
        read_frozen_document_review(&mut store, &current, &old, DOC_A, 0, 0)
            .await
            .is_err()
    );
    assert!(
        read_frozen_document_review(&mut store, &current, &old, DOC_A, 0, 101)
            .await
            .is_err()
    );
    let a = &review.operations[0];
    assert_eq!(
        (&a.operation_id, a.operation_seq, a.kind),
        (&sent.operation_id, 1, FrozenDocumentOperationKind::Content)
    );
    assert_eq!(a.generation, Some(1));
    assert_eq!(a.dependency, None);
    assert_eq!(a.expected_revision.as_deref(), Some("base-revision"));
    assert_eq!(
        read_frozen_document_operation(&mut store, &current, &old, DOC_A, &sent.operation_id)
            .await
            .unwrap(),
        b"sent"
    );
    assert_eq!(a.status, OutboxStatus::Sent);
    assert_eq!(a.abandon_state, OutboxAbandonState::None);
    let b = &review.operations[1];
    assert_eq!(
        (&b.operation_id, b.operation_seq, b.kind),
        (
            &restore.operation_id,
            2,
            FrozenDocumentOperationKind::Restore
        )
    );
    assert_eq!(b.generation, None);
    assert_eq!(b.dependency, None);
    assert_eq!(
        read_frozen_document_operation(&mut store, &current, &old, DOC_A, &restore.operation_id)
            .await
            .unwrap(),
        serde_json::to_vec(&restore_request).unwrap()
    );
    assert_eq!(b.status, OutboxStatus::Queued);
    let next = read_frozen_document_review(&mut store, &current, &old, DOC_A, 2, 2)
        .await
        .unwrap();
    assert_eq!(next.operations.len(), 1);
    assert_eq!(next.next_after_seq, None);
    let c = &next.operations[0];
    assert_eq!(
        (&c.operation_id, c.operation_seq, c.kind),
        (
            &queued.operation_id,
            3,
            FrozenDocumentOperationKind::Content
        )
    );
    assert_eq!(c.generation, Some(2));
    assert_eq!(c.dependency.as_deref(), Some(sent.operation_id.as_str()));
    assert_eq!(c.expected_revision, None);
    assert_eq!(
        read_frozen_document_operation(&mut store, &current, &old, DOC_A, &queued.operation_id)
            .await
            .unwrap(),
        b"queued"
    );
    assert_eq!(c.status, OutboxStatus::Queued);
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn frozen_document_review_keeps_draft_without_outbox_and_rejects_other_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("frozen-document-draft-only.sqlite");
    let mut store = ClientStore::open(&path).await.unwrap();
    let workspace = Uuid::new_v4().to_string();
    let old = binding(&workspace, &Uuid::new_v4().to_string());
    store.activate_registration(&old).await.unwrap();
    let edit = store
        .open_registered_document(&old, DOC_A, b"base", "base-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, DOC_A, &edit.edit_session_id, 1, b"draft only")
        .await
        .unwrap();
    store
        .close_registered_document(&old, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    let current = binding(&workspace, &Uuid::new_v4().to_string());
    store.activate_registration(&current).await.unwrap();
    let before = snapshot(&path).await;
    let review = read_frozen_document_review(&mut store, &current, &old, DOC_A, 0, 100)
        .await
        .unwrap();
    assert_eq!(review.old_binding, old);
    assert_eq!(review.current_binding, current);
    assert!(review.operations.is_empty());
    assert_eq!(review.draft.unwrap().bytes, b"draft only");
    assert_eq!(snapshot(&path).await, before);

    let foreign = binding(&Uuid::new_v4().to_string(), &Uuid::new_v4().to_string());
    store.activate_registration(&foreign).await.unwrap();
    let before_foreign = snapshot(&path).await;
    assert!(
        read_frozen_document_review(&mut store, &foreign, &old, DOC_A, 0, 100)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&path).await, before_foreign);
}

#[tokio::test]
async fn frozen_document_selected_operation_rejects_corrupt_payload_and_wrong_binding_without_mutation()
 {
    let mut rig = fixture().await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::query("UPDATE client_outbox SET payload_hash = ? WHERE operation_id = ?")
        .bind("0000000000000000000000000000000000000000000000000000000000000000")
        .bind(&rig.sent.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        read_frozen_document_operation(
            &mut rig.store,
            &rig.current,
            &rig.old,
            DOC_A,
            &rig.sent.operation_id
        )
        .await
        .is_err()
    );
    assert!(
        read_frozen_document_operation(
            &mut rig.store,
            &rig.current,
            &rig.old,
            DOC_B,
            &rig.successor.operation_id
        )
        .await
        .is_err()
    );
    assert!(
        read_frozen_document_operation(
            &mut rig.store,
            &rig.current,
            &rig.foreign,
            DOC_A,
            &rig.foreign_item.operation_id
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
}
