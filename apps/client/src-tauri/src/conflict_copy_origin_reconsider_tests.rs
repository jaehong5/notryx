use crate::conflict_snapshot_native::test_support::copy_progress::*;

#[tokio::test]
async fn original_conflict_can_be_reconsidered_after_copy_is_saved_without_erasing_copy_evidence() {
    let rig = rig_with_followup(true).await;
    let confirmation = confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    let progress = progress_request(&rig, confirmation["version"].as_str().unwrap());
    let old = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    let remote = notryx_client_store::ContentConflictIntentRequest {
        generation: 2,
        remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .replace_pending_content_conflict_intent(
                &rig.binding,
                &rig.document,
                &rig.operation.operation_id,
                &old,
                &remote
            )
            .await
            .is_err()
    );
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
    let copy_before = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    let changed = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .replace_pending_content_conflict_intent(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &old,
            &remote,
        )
        .await
        .unwrap();
    assert_eq!(changed.version, old.version + 1);
    assert_eq!(
        changed.choice,
        notryx_client_store::ContentConflictChoice::ChooseRemote
    );
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_content_conflict_intent_history(
                &rig.binding,
                &rig.operation.operation_id,
                old.version
            )
            .await
            .unwrap(),
        Some(old)
    );
    assert_eq!(
        state
            .store
            .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(copy_before.clone())
    );
    assert!(matches!(
        state
            .store
            .read_content_conflict_copy_content_state(
                &rig.binding,
                &rig.operation.operation_id,
                copy_before.intent_version
            )
            .await
            .unwrap(),
        notryx_client_store::ContentConflictCopyContentState::Saved { .. }
    ));
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
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
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("my-copy.md")).unwrap(),
        b"newer local\n"
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
