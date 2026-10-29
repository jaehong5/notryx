include!("conflict_snapshot_server_tests.rs");

#[tokio::test]
async fn unconfirmed_abandon_keeps_session_and_conflict_evidence() {
    let rig = rig_with_followup(true).await;
    let shown = prepare_conflict_snapshot(
        &rig.runtime,
        OWNER,
        &rig.document,
        &rig.edit_session,
        2,
        &rig.operation.operation_id,
    )
    .await
    .unwrap();
    let ack = rig
        .session
        .acknowledge(rig.operation.operation_seq)
        .await
        .unwrap();
    let followup = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .confirm_receipt_ack(&rig.binding, &ack)
            .await
            .unwrap();
        let request = notryx_client_store::ContentConflictIntentRequest {
            generation: 2,
            remote_revision: shown.remote_revision.clone(),
            remote_hash: rig.first_peer.content_hash.clone().unwrap(),
            remote_bytes: shown.remote_bytes.clone(),
            choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
        };
        state
            .store
            .record_content_conflict_intent(
                &rig.binding,
                &rig.document,
                &rig.operation.operation_id,
                &request,
            )
            .await
            .unwrap();
        let followup = state
            .store
            .request_content_conflict_followup_abandon(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        state
            .store
            .prepare_outbox_matching(&rig.binding, &followup)
            .await
            .unwrap()
            .unwrap()
    };
    let receipt = rig
        .session
        .abandon(&notryx_client_runtime::AbandonMutation {
            binding: rig.session.binding().clone(),
            operation_id: followup.operation_id.parse().unwrap(),
            operation_seq: followup.operation_seq,
            node_id: followup.document_id.parse().unwrap(),
            bytes: followup.bytes.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Abandoned { .. }));
    let (draft_before, failed_before, abandoned_before, intent_before) = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .apply_outbox_abandoned(
                &rig.binding,
                &notryx_client_store::OutboxAbandoned {
                    operation_id: followup.operation_id.clone(),
                    operation_seq: followup.operation_seq,
                    workspace_epoch: rig.binding.workspace_epoch.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            followup.operation_seq
        );
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            rig.operation.operation_seq
        );
        (
            state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap(),
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            state
                .store
                .read_outbox_operation(&rig.binding, &followup.operation_id)
                .await
                .unwrap()
                .unwrap(),
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
        )
    };
    assert_eq!(
        abandoned_before.status,
        notryx_client_store::OutboxStatus::Abandoned
    );
    assert_eq!(abandoned_before.bytes, b"newer local\n");
    assert_eq!(draft_before.bytes, b"newer local\n");
    assert_eq!(draft_before.base_bytes, BASE);
    assert_eq!(failed_before.bytes, LOCAL);

    let result = crate::conflict_snapshot_native::choose_remote_after_confirmation(
        &rig.runtime,
        OWNER,
        &rig.document,
        &rig.edit_session,
        2,
        &rig.operation.operation_id,
        &shown.remote_revision,
        rig.first_peer.content_hash.as_deref().unwrap(),
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573",
        "sha256:3617ea161bd9febb1c19b7411687c2c34c0f264b5efbd7339e0cbba453df8fe2",
        &shown.base_revision,
        &shown.conflict_revision,
    )
    .await;
    assert!(result.is_err());
    let mut state = rig.runtime.0.lock().await;
    assert!(state.product_sessions.contains_key(&rig.edit_session));
    assert!(
        state
            .store
            .dirty_registered_session_snapshot(&rig.binding, &rig.document, &rig.edit_session,)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap(),
        draft_before
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        failed_before
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &followup.operation_id)
            .await
            .unwrap()
            .unwrap(),
        abandoned_before
    );
    assert_eq!(
        state
            .store
            .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        intent_before
    );
    assert_eq!(
        state
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap(),
        rig.operation.operation_seq
    );
    state.gate.close_session(&rig.edit_session).unwrap();
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
