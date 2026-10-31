use crate::conflict_snapshot_native::test_support::continue_remote::*;

#[tokio::test]
async fn progress_reports_the_requested_followup_id_again_for_requested_or_sent_state() {
    for sent in [false, true] {
        let (rig, intent, followup_id) = continue_rig().await;
        let first = continue_choice(&rig, intent.version).await.unwrap();
        assert_eq!(first["status"], "pending");
        assert_eq!(first["version"], intent.version.to_string());
        assert_eq!(first["requested_operation_id"], followup_id);
        assert_ne!(followup_id, rig.operation.operation_id);
        if sent {
            let mut state = rig.runtime.0.lock().await;
            let marked = state
                .store
                .read_outbox_operation(&rig.binding, &followup_id)
                .await
                .unwrap()
                .unwrap();
            let prepared = state
                .store
                .prepare_outbox_matching(&rig.binding, &marked)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                prepared.abandon_state,
                notryx_client_store::OutboxAbandonState::Sent
            );
        }
        let retry = continue_choice(&rig, intent.version).await.unwrap();
        assert_eq!(retry["status"], "pending");
        assert_eq!(retry["version"], intent.version.to_string());
        assert_eq!(retry["requested_operation_id"], followup_id);
        let mut state = rig.runtime.0.lock().await;
        let failed = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        let followup = state
            .store
            .read_outbox_operation(&rig.binding, &followup_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(failed.bytes, LOCAL);
        assert_eq!(followup.operation_seq, failed.operation_seq + 1);
        assert_eq!(followup.bytes, b"newer local\n");
        assert_eq!(
            followup.abandon_state,
            if sent {
                notryx_client_store::OutboxAbandonState::Sent
            } else {
                notryx_client_store::OutboxAbandonState::Requested
            }
        );
        assert_eq!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            Some(intent)
        );
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
                .list_outbox(&rig.binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            2
        );
        drop(state);
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
            REMOTE
        );
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn progress_reports_null_if_there_is_no_followup_content() {
    let rig = rig().await;
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: 1,
        remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    let intent = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .record_content_conflict_intent(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &request,
        )
        .await
        .unwrap();
    let result = continue_choice(&rig, intent.version).await.unwrap();
    assert_eq!(result["status"], "pending");
    assert_eq!(result["version"], intent.version.to_string());
    assert!(result["requested_operation_id"].is_null());
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(intent)
    );
    let entries = state
        .store
        .list_outbox(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation_id, rig.operation.operation_id);
    assert_eq!(
        entries[0].status,
        notryx_client_store::OutboxStatus::Conflict
    );
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
