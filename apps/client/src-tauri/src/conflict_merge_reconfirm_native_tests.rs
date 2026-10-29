include!("conflict_merge_confirm_native_tests.rs");

async fn initial_choice(
    rig: &Rig,
    choice: notryx_client_store::ContentConflictChoice,
) -> notryx_client_store::ContentConflictIntent {
    rig.runtime
        .0
        .lock()
        .await
        .store
        .record_content_conflict_intent(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &notryx_client_store::ContentConflictIntentRequest {
                generation: 2,
                remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
                remote_hash: rig.first_peer.content_hash.clone().unwrap(),
                remote_bytes: REMOTE.to_vec(),
                choice,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn merge_reconfirmation_preserves_old_choice_and_retries_with_the_same_version() {
    let rig = rig_with_followup(true).await;
    let old = initial_choice(
        &rig,
        notryx_client_store::ContentConflictChoice::ChooseRemote,
    )
    .await;
    let request = choice_request(&rig, 2, Some(old.version));
    let version = confirm_merge(&rig.runtime, request.clone()).await.unwrap();
    assert_eq!(version, "1");
    assert_eq!(confirm_merge(&rig.runtime, request).await.unwrap(), version);
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.version, 1);
    assert_eq!(
        current.choice,
        notryx_client_store::ContentConflictChoice::SaveMerged {
            bytes: b"newer local\n".to_vec()
        }
    );
    assert_eq!(current.merged_hash.as_deref(), Some(FOLLOWUP_HASH));
    assert_eq!(
        state
            .store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0)
            .await
            .unwrap(),
        Some(old)
    );
    drop(state);
    assert_unmodified_sources(&rig, b"newer local\n", false).await;
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_reconfirmation_uses_the_current_checkpoint_after_later_edit() {
    let rig = rig_with_followup(true).await;
    let old = initial_choice(
        &rig,
        notryx_client_store::ContentConflictChoice::ChooseRemote,
    )
    .await;
    rig.runtime
        .0
        .lock()
        .await
        .store
        .save_registered_checkpoint(
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            3,
            b"newer changed\n",
        )
        .await
        .unwrap();
    assert!(
        confirm_merge(&rig.runtime, choice_request(&rig, 2, Some(old.version)))
            .await
            .is_err()
    );
    let mut request = choice_request(&rig, 3, Some(old.version));
    request.local_hash =
        "sha256:c93b0f291e2bb45ca8fd90a6ce95a37924474f747bba2236407e3a3dc6373826".into();
    assert_eq!(confirm_merge(&rig.runtime, request).await.unwrap(), "1");
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.generation, 3);
    assert_eq!(current.local_bytes, b"newer changed\n");
    assert_eq!(
        current.choice,
        notryx_client_store::ContentConflictChoice::SaveMerged {
            bytes: b"newer changed\n".to_vec()
        }
    );
    assert_eq!(
        state
            .store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0)
            .await
            .unwrap(),
        Some(old)
    );
    drop(state);
    assert_unmodified_sources(&rig, b"newer changed\n", false).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_merge_requires_explicit_reconfirmation_and_preserves_unhashed_history() {
    use sqlx::Connection;
    let rig = rig_with_followup(true).await;
    let _ = initial_choice(
        &rig,
        notryx_client_store::ContentConflictChoice::SaveMerged {
            bytes: b"newer local\n".to_vec(),
        },
    )
    .await;
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE content_conflict_intents SET merged_hash=NULL WHERE operation_id=?")
        .bind(&rig.operation.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut state = rig.runtime.0.lock().await;
    let old = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.merged_hash, None);
    drop(state);
    assert!(
        confirm_merge(&rig.runtime, choice_request(&rig, 2, None))
            .await
            .is_err()
    );
    let request = choice_request(&rig, 2, Some(old.version));
    assert_eq!(
        confirm_merge(&rig.runtime, request.clone()).await.unwrap(),
        "1"
    );
    assert_eq!(confirm_merge(&rig.runtime, request).await.unwrap(), "1");
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.merged_hash.as_deref(), Some(FOLLOWUP_HASH));
    assert_eq!(
        state
            .store
            .read_content_conflict_intent_history(&rig.binding, &rig.operation.operation_id, 0)
            .await
            .unwrap(),
        Some(old)
    );
    drop(state);
    assert_unmodified_sources(&rig, b"newer local\n", false).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
