include!("conflict_complete_pending_remote_tests.rs");

use sqlx::Connection;

#[tokio::test]
async fn known_precompletion_failures_preserve_the_edit_session_and_allow_checkpoint() {
    for phase in [
        "unconfirmed",
        "stale_version",
        "remote_changed",
        "remote_deleted",
        "storage_failure",
    ] {
        let (rig, intent, followup) = if phase == "unconfirmed" {
            let result = continue_rig().await;
            assert_eq!(
                continue_choice(&result.0, result.1.version).await.unwrap()["status"],
                "pending"
            );
            result
        } else {
            ready_for_completion().await
        };
        if phase == "remote_changed" {
            let receipt = rig
                .peer
                .put_content(&ContentMutation {
                    binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                    operation_seq: 2,
                    node_id: rig.initial.node_id,
                    expected_revision: rig.first_peer.content_rev.clone().unwrap(),
                    bytes: b"later peer\n".to_vec(),
                })
                .await
                .unwrap();
            assert!(matches!(receipt, OperationReceipt::Committed { .. }));
        } else if phase == "remote_deleted" {
            let receipt = rig
                .peer
                .delete(&DeleteMutation {
                    binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                    operation_seq: 2,
                    node_id: rig.initial.node_id,
                    expected_node_etag: rig.first_peer.node_etag.clone(),
                })
                .await
                .unwrap();
            assert!(matches!(receipt, OperationReceipt::Deleted { .. }));
        } else if phase == "storage_failure" {
            let mut db = sqlx::SqliteConnection::connect_with(
                &sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(rig._directory.path().join("conflict-snapshot.sqlite")),
            )
            .await
            .unwrap();
            sqlx::raw_sql("CREATE TRIGGER reject_pending_completion BEFORE UPDATE OF completed_generation ON content_conflict_intents BEGIN SELECT RAISE(ABORT, 'synthetic completion failure'); END")
                .execute(&mut db).await.unwrap();
            db.close().await.unwrap();
        }
        let (before_draft, before_original, before_followup, confirmed) = {
            let mut state = rig.runtime.0.lock().await;
            let draft = state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap();
            let original = state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap();
            let followup = state
                .store
                .read_outbox_operation(&rig.binding, &followup)
                .await
                .unwrap()
                .unwrap();
            let confirmed = state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap();
            (draft, original, followup, confirmed)
        };
        assert_eq!(before_draft.generation, intent.generation);
        assert_eq!(before_draft.bytes, b"newer local\n");
        assert_eq!(before_draft.base_bytes, BASE);
        if phase == "unconfirmed" {
            assert_eq!(
                before_followup.status,
                notryx_client_store::OutboxStatus::Abandoning
            );
            assert!(confirmed < before_followup.operation_seq);
        } else {
            assert_eq!(
                before_followup.status,
                notryx_client_store::OutboxStatus::Abandoned
            );
            assert!(confirmed >= before_followup.operation_seq);
        }
        let expected_version = if phase == "stale_version" {
            intent.version + 1
        } else {
            intent.version
        };
        let error = complete_pending(&rig, expected_version).await.unwrap_err();
        if phase == "storage_failure" {
            assert_eq!(error, "Storage");
        }
        if phase == "stale_version" {
            assert_eq!(error, "stale_conflict");
        }

        let mut state = rig.runtime.0.lock().await;
        let product = state.product_sessions.get(&rig.edit_session).unwrap();
        assert_eq!(product.document_id, rig.document);
        assert_eq!(product.binding, rig.binding);
        assert!(product.kind == ProductSessionKind::Active);
        assert!(state.gate.check_session(&rig.edit_session).is_ok());
        let snapshot = state
            .store
            .dirty_registered_session_snapshot(&rig.binding, &rig.document, &rig.edit_session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.checkpoint_generation, Some(intent.generation));
        assert_eq!(snapshot.base_revision, intent.base_revision);
        assert_eq!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            Some(intent.clone())
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap(),
            before_draft
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            before_original
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &followup)
                .await
                .unwrap()
                .unwrap(),
            before_followup
        );
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.document,
                &rig.edit_session,
                intent.generation + 1,
                b"preserved next local edit\n",
            )
            .await
            .unwrap();
        let after = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.bytes, b"preserved next local edit\n");
        assert_eq!(after.base_bytes, before_draft.base_bytes);
        assert_eq!(after.base_revision, before_draft.base_revision);
        assert_eq!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            Some(intent)
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            before_original
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &followup)
                .await
                .unwrap()
                .unwrap(),
            before_followup
        );
        drop(state);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn local_abandon_receipt_without_server_ack_keeps_session_usable() {
    let (rig, intent, followup_id) = continue_rig().await;
    let original_ack = rig
        .session
        .acknowledge(rig.operation.operation_seq)
        .await
        .unwrap();
    rig.runtime
        .0
        .lock()
        .await
        .store
        .confirm_receipt_ack(&rig.binding, &original_ack)
        .await
        .unwrap();
    assert_eq!(
        continue_choice(&rig, intent.version).await.unwrap()["status"],
        "pending"
    );
    let sent = {
        let mut state = rig.runtime.0.lock().await;
        let marked = state
            .store
            .read_outbox_operation(&rig.binding, &followup_id)
            .await
            .unwrap()
            .unwrap();
        state
            .store
            .prepare_outbox_matching(&rig.binding, &marked)
            .await
            .unwrap()
            .unwrap()
    };
    let receipt = rig
        .session
        .abandon(&notryx_client_runtime::AbandonMutation {
            binding: rig.session.binding().clone(),
            operation_id: sent.operation_id.parse().unwrap(),
            operation_seq: sent.operation_seq,
            node_id: sent.document_id.parse().unwrap(),
            bytes: sent.bytes.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Abandoned { .. }));
    let (original, abandoned, before) = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .apply_outbox_abandoned(
                &rig.binding,
                &notryx_client_store::OutboxAbandoned {
                    operation_id: sent.operation_id.clone(),
                    operation_seq: sent.operation_seq,
                    workspace_epoch: rig.binding.workspace_epoch.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            sent.operation_seq
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
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            state
                .store
                .read_outbox_operation(&rig.binding, &followup_id)
                .await
                .unwrap()
                .unwrap(),
            state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap(),
        )
    };
    assert_eq!(
        abandoned.status,
        notryx_client_store::OutboxStatus::Abandoned
    );
    assert!(complete_pending(&rig, intent.version).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    let product = state.product_sessions.get(&rig.edit_session).unwrap();
    assert_eq!(product.binding, rig.binding);
    assert_eq!(product.document_id, rig.document);
    assert!(product.kind == ProductSessionKind::Active);
    state.gate.check_session(&rig.edit_session).unwrap();
    assert!(
        state
            .store
            .dirty_registered_session_snapshot(&rig.binding, &rig.document, &rig.edit_session)
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
        before
    );
    state
        .store
        .save_registered_checkpoint(
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            intent.generation + 1,
            b"edit after pending receipt\n",
        )
        .await
        .unwrap();
    let after = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.bytes, b"edit after pending receipt\n");
    assert_eq!(after.base_bytes, before.base_bytes);
    assert_eq!(after.base_revision, before.base_revision);
    assert_eq!(
        state
            .store
            .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &followup_id)
            .await
            .unwrap()
            .unwrap(),
        abandoned
    );
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
