use crate::conflict_snapshot_native::test_support::merge_confirm::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct MergeFollowupReview {
    operation_id: String,
    operation_seq: u64,
    generation: u64,
    payload_hash: String,
}

async fn inspect_merge_followup(
    rig: &Rig,
    version: u64,
) -> Result<Option<MergeFollowupReview>, String> {
    Ok(
        crate::conflict_snapshot_native::inspect_merge_followup_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            version,
        )
        .await?
        .map(|item| MergeFollowupReview {
            operation_id: item.operation_id,
            operation_seq: item.operation_seq,
            generation: item.generation,
            payload_hash: item.payload_hash,
        }),
    )
}

async fn approve_merge_followup(
    rig: &Rig,
    version: u64,
    shown: &MergeFollowupReview,
) -> Result<String, String> {
    Ok(
        crate::conflict_snapshot_native::approve_merge_followup_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            &rig.operation.operation_id,
            version,
            &shown.operation_id,
            shown.operation_seq,
            shown.generation,
            &shown.payload_hash,
        )
        .await?
        .operation_id,
    )
}

async fn confirm_merge_original_ack(rig: &Rig) {
    let ack = rig
        .session
        .acknowledge(rig.operation.operation_seq)
        .await
        .unwrap();
    rig.runtime
        .0
        .lock()
        .await
        .store
        .confirm_receipt_ack(&rig.binding, &ack)
        .await
        .unwrap();
}

#[tokio::test]
async fn merge_followup_requires_exact_explicit_approval_before_abandon_and_merge_send() {
    let rig = rig_with_followup(true).await;
    assert_eq!(
        confirm_merge(&rig.runtime, choice_request(&rig, 2, None))
            .await
            .unwrap(),
        "0"
    );
    let expected = {
        let mut state = rig.runtime.0.lock().await;
        let latest = state
            .store
            .document_outbox_state(&rig.binding, &rig.document)
            .await
            .unwrap()
            .latest
            .unwrap();
        state
            .store
            .read_outbox_operation(&rig.binding, &latest.operation_id)
            .await
            .unwrap()
            .unwrap()
    };
    assert!(
        crate::conflict_snapshot_native::queue_merge_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            &rig.operation.operation_id,
            0,
        )
        .await
        .is_err()
    );
    assert_unmodified_sources(&rig, b"newer local\n", false).await;
    confirm_merge_original_ack(&rig).await;
    let shown = inspect_merge_followup(&rig, 0).await.unwrap().unwrap();
    assert_eq!(shown.operation_id, expected.operation_id);
    assert_eq!(shown.operation_seq, expected.operation_seq);
    assert_eq!(shown.generation, expected.generation);
    assert_eq!(shown.payload_hash, expected.payload_hash);
    assert!(
        crate::conflict_snapshot_native::queue_merge_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            &rig.operation.operation_id,
            0,
        )
        .await
        .is_err()
    );
    for wrong in [
        MergeFollowupReview {
            operation_id: Uuid::now_v7().to_string(),
            ..shown.clone()
        },
        MergeFollowupReview {
            operation_seq: shown.operation_seq + 1,
            ..shown.clone()
        },
        MergeFollowupReview {
            generation: shown.generation + 1,
            ..shown.clone()
        },
        MergeFollowupReview {
            payload_hash: BASE_HASH.into(),
            ..shown.clone()
        },
    ] {
        assert!(approve_merge_followup(&rig, 0, &wrong).await.is_err());
    }
    assert_unmodified_sources(&rig, b"newer local\n", false).await;
    assert_eq!(
        approve_merge_followup(&rig, 0, &shown).await.unwrap(),
        shown.operation_id
    );
    assert_eq!(
        approve_merge_followup(&rig, 0, &shown).await.unwrap(),
        shown.operation_id
    );
    {
        let mut state = rig.runtime.0.lock().await;
        let selected = state
            .store
            .read_outbox_operation(&rig.binding, &shown.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            selected.abandon_state,
            notryx_client_store::OutboxAbandonState::Requested
        );
        assert_eq!(selected.bytes, b"newer local\n");
    }
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    let abandoned = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        abandoned.status,
        notryx_protocol::WorkspaceSendStatus::Abandoned
    );
    assert_eq!(
        abandoned.operation_id.as_deref(),
        Some(shown.operation_id.as_str())
    );
    {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap()
                >= shown.operation_seq
        );
    }
    let merged = crate::conflict_snapshot_native::queue_merge_for_renderer(
        &rig.runtime,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.edit_session,
        &rig.operation.operation_id,
        0,
    )
    .await
    .unwrap();
    assert_eq!(merged.bytes, b"newer local\n");
    assert_eq!(
        merged.expected_revision.as_deref(),
        Some(rig.first_peer.content_rev.as_ref().unwrap().as_str())
    );
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(merged.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        b"newer local\n"
    );
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap()
            >= merged.operation_seq
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_followup_changed_after_display_requires_reconfirmation_and_preserves_draft() {
    let rig = rig_with_followup(true).await;
    assert_eq!(
        confirm_merge(&rig.runtime, choice_request(&rig, 2, None))
            .await
            .unwrap(),
        "0"
    );
    confirm_merge_original_ack(&rig).await;
    let stale = inspect_merge_followup(&rig, 0).await.unwrap().unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.document,
                &rig.edit_session,
                3,
                b"later user\n",
            )
            .await
            .unwrap();
        let merged = state
            .store
            .queue_checkpoint(&rig.binding, &rig.document, 3)
            .await
            .unwrap();
        assert_eq!(merged.operation_id, stale.operation_id);
        assert_eq!(merged.generation, 3);
    }
    assert!(approve_merge_followup(&rig, 0, &stale).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later user\n");
    let saved = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.choice,
        notryx_client_store::ContentConflictChoice::SaveMerged {
            bytes: b"newer local\n".to_vec()
        }
    );
    let operation = state
        .store
        .read_outbox_operation(&rig.binding, &stale.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.bytes, b"later user\n");
    assert_eq!(
        operation.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    drop(state);
    let mut selection = choice_request(&rig, 3, Some(0));
    selection.local_hash =
        "sha256:33fa18816f743cffd1f50a2e8be0f8767a022f0f4af52154e50c8c43344417a7".into();
    assert_eq!(confirm_merge(&rig.runtime, selection).await.unwrap(), "1");
    let fresh = inspect_merge_followup(&rig, 1).await.unwrap().unwrap();
    assert_eq!(fresh.operation_id, stale.operation_id);
    assert_eq!(fresh.generation, 3);
    assert_ne!(fresh.payload_hash, stale.payload_hash);
    assert_eq!(
        approve_merge_followup(&rig, 1, &fresh).await.unwrap(),
        fresh.operation_id
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_followup_approval_response_retry_survives_ack_and_later_checkpoint() {
    let rig = rig_with_followup(true).await;
    assert_eq!(
        confirm_merge(&rig.runtime, choice_request(&rig, 2, None))
            .await
            .unwrap(),
        "0"
    );
    confirm_merge_original_ack(&rig).await;
    let shown = inspect_merge_followup(&rig, 0).await.unwrap().unwrap();
    let discarded_response = approve_merge_followup(&rig, 0, &shown).await.unwrap();
    assert_eq!(discarded_response, shown.operation_id);
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Abandoned);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(shown.operation_id.as_str())
    );
    {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap()
                >= shown.operation_seq
        );
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.document,
                &rig.edit_session,
                3,
                b"later user\n",
            )
            .await
            .unwrap();
    }
    assert_eq!(
        approve_merge_followup(&rig, 0, &shown).await.unwrap(),
        shown.operation_id
    );
    let mut state = rig.runtime.0.lock().await;
    let checkpoint = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.generation, 3);
    assert_eq!(checkpoint.bytes, b"later user\n");
    let saved = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.choice,
        notryx_client_store::ContentConflictChoice::SaveMerged {
            bytes: b"newer local\n".to_vec()
        }
    );
    let original = state
        .store
        .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.bytes, LOCAL);
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
