include!("conflict_copy_origin_reconsider_tests.rs");

#[tokio::test]
async fn saved_copy_survives_repeated_original_reconsideration_and_remote_completion() {
    let rig = rig_with_followup(true).await;
    let confirmed = confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    let progress = progress_request(&rig, confirmed["version"].as_str().unwrap());
    progress_copy(&rig, progress.clone()).await.unwrap();
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Abandoned
    );
    progress_copy(&rig, progress.clone()).await.unwrap();
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    progress_copy(&rig, progress.clone()).await.unwrap();
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    assert_eq!(
        progress_copy(&rig, progress).await.unwrap()["status"],
        "saved"
    );
    let original_copy = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    let remote_request = choice_request(&rig, 2, Some(original_copy.intent_version));
    assert_eq!(
        choose(&rig, remote_request.clone()).await.unwrap()["status"],
        "pending"
    );
    assert_eq!(
        choose(&rig, remote_request).await.unwrap()["status"],
        "pending"
    );
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    let merge = notryx_client_store::ContentConflictIntentRequest {
        generation: current.generation,
        remote_revision: current.remote_revision.clone(),
        remote_hash: current.remote_hash.clone(),
        remote_bytes: current.remote_bytes.clone(),
        choice: notryx_client_store::ContentConflictChoice::SaveMerged {
            bytes: b"explicit merge\n".to_vec(),
        },
    };
    let merged = state
        .store
        .replace_pending_content_conflict_intent(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &current,
            &merge,
        )
        .await
        .unwrap();
    drop(state);
    let request = choice_request(&rig, 2, Some(merged.version));
    assert_eq!(choose(&rig, request).await.unwrap()["status"], "pending");
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.choice,
        notryx_client_store::ContentConflictChoice::ChooseRemote
    );
    assert_eq!(
        state
            .store
            .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(original_copy.clone())
    );
    assert!(matches!(
        state
            .store
            .read_content_conflict_copy_content_state(
                &rig.binding,
                &rig.operation.operation_id,
                original_copy.intent_version
            )
            .await
            .unwrap(),
        notryx_client_store::ContentConflictCopyContentState::Saved { .. }
    ));
    drop(state);
    let completed = crate::conflict_snapshot_native::complete_pending_remote_choice_for_renderer(
        &rig.runtime,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.edit_session,
        &rig.operation.operation_id,
        current.version,
    )
    .await
    .unwrap();
    assert!(matches!(
        completed,
        notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Completed { .. }
    ));
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(original_copy.clone())
    );
    assert!(matches!(
        state
            .store
            .read_content_conflict_copy_content_state(
                &rig.binding,
                &rig.operation.operation_id,
                original_copy.intent_version
            )
            .await
            .unwrap(),
        notryx_client_store::ContentConflictCopyContentState::Saved { .. }
    ));
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        REMOTE
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        LOCAL
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("my-copy.md")).unwrap(),
        b"newer local\n"
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
