include!("content_conflict_choose_remote_guards.rs");

use notryx_client_store::{ContentConflictChoiceKind, ContentConflictResolutionState};

async fn resolution(
    store: &mut ClientStore,
    binding: &RegistrationBinding,
    operation_id: &str,
) -> Result<ContentConflictResolutionState, StoreError> {
    store
        .read_content_conflict_resolution_state(binding, operation_id)
        .await
}

#[tokio::test]
async fn absent_pending_and_newer_local_are_distinct_without_an_editor_session() {
    let (rig, mut store) = rig().await;
    assert!(matches!(
        resolution(&mut store, &rig.binding, &rig.operation)
            .await
            .unwrap(),
        ContentConflictResolutionState::NotFound
    ));
    let intent = record_remote_choice(&mut store, &rig).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    match resolution(&mut store, &rig.binding, &rig.operation)
        .await
        .unwrap()
    {
        ContentConflictResolutionState::Pending {
            version,
            generation,
            choice,
            needs_review,
            frozen,
        } => {
            assert_eq!(version, intent.version);
            assert_eq!(generation, intent.generation);
            assert_eq!(choice, ContentConflictChoiceKind::ChooseRemote);
            assert!(!needs_review);
            assert!(!frozen);
        }
        _ => panic!("expected pending choice"),
    }
    let opened = store
        .open_registered_document(&rig.binding, DOC, BASE, "base-rev")
        .await
        .unwrap();
    let later = b"later local after selection\n";
    store
        .save_registered_checkpoint(&rig.binding, DOC, &opened.edit_session_id, 3, later)
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &opened.edit_session_id)
        .await
        .unwrap();
    match resolution(&mut store, &rig.binding, &rig.operation)
        .await
        .unwrap()
    {
        ContentConflictResolutionState::Pending {
            version,
            generation,
            choice,
            needs_review,
            frozen,
        } => {
            assert_eq!(version, intent.version);
            assert_eq!(generation, intent.generation);
            assert_eq!(choice, ContentConflictChoiceKind::ChooseRemote);
            assert!(needs_review);
            assert!(!frozen);
        }
        _ => panic!("expected review of pending choice"),
    }
    assert_unresolved_versions(&mut store, &rig, &intent, later, OutboxStatus::Queued).await;
}

#[tokio::test]
async fn completed_choice_survives_response_loss_and_restart_without_mutation() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    match resolution(&mut store, &rig.binding, &rig.operation)
        .await
        .unwrap()
    {
        ContentConflictResolutionState::Completed { generation, frozen } => {
            assert_eq!(generation, selected.generation);
            assert!(!frozen);
        }
        _ => panic!("completed choice was lost"),
    }
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, selected.generation);
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, "current-rev");
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    let failed = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, OutboxStatus::Conflict);
    assert_eq!(failed.bytes, SENT);
    assert_eq!(
        store.confirmed_receipt_through(&rig.binding).await.unwrap(),
        2
    );
    assert!(
        store
            .next_pending_outbox(&rig.binding)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn frozen_pending_or_completed_stays_read_only_in_original_registration() {
    for complete in [false, true] {
        let (rig, mut store) = rig().await;
        let intent = record_remote_choice(&mut store, &rig).await;
        if complete {
            request_and_finish_followup(&mut store, &rig, true).await;
        }
        store
            .close_registered_document(&rig.binding, DOC, &rig.session)
            .await
            .unwrap();
        if complete {
            store
                .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
                .await
                .unwrap();
        }
        let mut replacement = binding();
        replacement.workspace_id = rig.binding.workspace_id.clone();
        replacement.workspace_epoch = rig.binding.workspace_epoch.clone();
        store.activate_registration(&replacement).await.unwrap();
        let old_state = resolution(&mut store, &rig.binding, &rig.operation)
            .await
            .unwrap();
        match (complete, old_state) {
            (
                false,
                ContentConflictResolutionState::Pending {
                    version,
                    generation,
                    choice,
                    needs_review,
                    frozen,
                },
            ) => {
                assert_eq!(version, intent.version);
                assert_eq!(generation, intent.generation);
                assert_eq!(choice, ContentConflictChoiceKind::ChooseRemote);
                assert!(!needs_review);
                assert!(frozen);
            }
            (true, ContentConflictResolutionState::Completed { frozen, .. }) => {
                assert!(frozen);
            }
            _ => panic!("wrong frozen state"),
        }
        assert!(matches!(
            resolution(&mut store, &replacement, &rig.operation)
                .await
                .unwrap(),
            ContentConflictResolutionState::NotFound
        ));
        assert!(
            store
                .next_pending_outbox(&replacement)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_outbox_operation(&rig.binding, &rig.operation)
                .await
                .unwrap()
                .unwrap()
                .frozen
        );
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(intent)
        );
    }
}

#[tokio::test]
async fn damaged_completion_marker_never_reports_completed() {
    let (rig, mut store) = rig().await;
    let intent = record_remote_choice(&mut store, &rig).await;
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let selected = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::query("UPDATE content_conflict_intents SET completed_generation=completed_generation+1 WHERE operation_id=?")
        .bind(&rig.operation).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(
        resolution(&mut store, &rig.binding, &rig.operation)
            .await
            .is_err()
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, selected.generation);
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
}

#[tokio::test]
async fn pending_state_reports_each_choice_kind_without_returning_source_bytes() {
    for (choice, expected) in [
        (
            ContentConflictChoice::KeepLocalCopy,
            ContentConflictChoiceKind::KeepLocalCopy,
        ),
        (
            ContentConflictChoice::ChooseRemote,
            ContentConflictChoiceKind::ChooseRemote,
        ),
        (
            ContentConflictChoice::SaveMerged {
                bytes: b"synthetic merged\n".to_vec(),
            },
            ContentConflictChoiceKind::SaveMerged,
        ),
    ] {
        let (rig, mut store) = rig().await;
        let mut request = rig.request.clone();
        request.choice = choice;
        let intent = record(&mut store, &rig, &request).await.unwrap();
        store
            .close_registered_document(&rig.binding, DOC, &rig.session)
            .await
            .unwrap();
        match resolution(&mut store, &rig.binding, &rig.operation)
            .await
            .unwrap()
        {
            ContentConflictResolutionState::Pending {
                version,
                generation,
                choice,
                needs_review,
                frozen,
            } => {
                assert_eq!(version, intent.version);
                assert_eq!(generation, intent.generation);
                assert_eq!(choice, expected);
                assert!(!needs_review);
                assert!(!frozen);
            }
            _ => panic!("expected pending choice"),
        }
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(intent)
        );
    }
}
