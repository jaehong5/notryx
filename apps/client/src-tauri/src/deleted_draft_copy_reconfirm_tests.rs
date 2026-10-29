include!("deleted_draft_copy_recovery_tests.rs");

#[tokio::test]
async fn deleted_copy_name_collision_requires_ack_and_explicit_reconfirmation_with_fixed_retry_ids()
{
    let rig = deleted_copy_rig().await;
    let failed = {
        let mut state = rig.runtime.0.lock().await;
        let operation = state
            .store
            .queue_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap();
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &operation.operation_id,
                    &rig.intent.parent_id,
                    "second-copy.md"
                )
                .await
                .is_err()
        );
        assert!(matches!(
            state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap(),
            Some(PendingOutboxOperation::Create(_))
        ));
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &operation.operation_id,
                    &rig.intent.parent_id,
                    "second-copy.md"
                )
                .await
                .is_err()
        );
        operation
    };
    let peer = rig
        .session_b
        .create(&CreateMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            request: CreateNodeRequest {
                node_id: Uuid::now_v7().to_string().parse().unwrap(),
                parent_id: rig.intent.parent_id.parse().unwrap(),
                name: rig.intent.name.clone(),
                kind: NodeKind::File,
                upload_id: None,
            },
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Created { .. }));
    let receipt = rig
        .session_a
        .create(&CreateMutation {
            binding: rig.session_a.binding().clone(),
            operation_id: failed.operation_id.parse().unwrap(),
            operation_seq: failed.operation_seq,
            request: failed.request.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        receipt,
        OperationReceipt::DestinationExists { .. }
    ));
    {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .apply_create_receipt(&rig.binding, &receipt)
            .await
            .unwrap();
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &failed.operation_id,
                    &rig.intent.parent_id,
                    "second-copy.md"
                )
                .await
                .is_err()
        );
    }
    let ack = rig
        .session_a
        .acknowledge(failed.operation_seq)
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
    let fresh = {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &failed.operation_id,
                    &rig.intent.parent_id,
                    &rig.intent.name
                )
                .await
                .is_err()
        );
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &Uuid::now_v7().to_string(),
                    &rig.intent.parent_id,
                    "second-copy.md"
                )
                .await
                .is_err()
        );
        state
            .store
            .reconfirm_failed_deleted_draft_copy(
                &rig.binding,
                &rig.document,
                &failed.operation_id,
                &rig.intent.parent_id,
                "second-copy.md",
            )
            .await
            .unwrap()
    };
    assert_ne!(fresh.node_id, rig.intent.node_id);
    assert_ne!(fresh.create_operation_id, failed.operation_id);
    assert_eq!(fresh.local_bytes, CONFIRMED);
    assert_eq!(fresh.generation, rig.intent.generation);
    let rig = restart(rig).await;
    let queued = {
        let mut state = rig.runtime.0.lock().await;
        let retry = state
            .store
            .reconfirm_failed_deleted_draft_copy(
                &rig.binding,
                &rig.document,
                &failed.operation_id,
                &rig.intent.parent_id,
                "second-copy.md",
            )
            .await
            .unwrap();
        assert_eq!(retry, fresh);
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &failed.operation_id,
                    &rig.intent.parent_id,
                    "third-copy.md"
                )
                .await
                .is_err()
        );
        let queued = state
            .store
            .queue_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap();
        assert_eq!(queued.operation_id, fresh.create_operation_id);
        assert!(queued.operation_seq > failed.operation_seq);
        queued
    };
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(queued.operation_id.as_str())
    );
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    let node = rig
        .session_a
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == fresh.node_id)
        .unwrap();
    assert_eq!(node.name, "second-copy.md");
    assert_eq!(node.parent_id.unwrap().to_string(), fresh.parent_id);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("second-copy.md")).unwrap(),
        b""
    );
    let content = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy_content(&rig.binding, &rig.document)
        .await
        .unwrap();
    assert_eq!(content.document_id, fresh.node_id);
    assert_eq!(
        content.expected_revision.as_deref(),
        node.content_rev.as_ref().map(|rev| rev.as_str())
    );
    assert_eq!(content.bytes, CONFIRMED);
    assert!(content.operation_seq > queued.operation_seq);
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(content.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("second-copy.md")).unwrap(),
        CONFIRMED
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        b""
    );
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap(),
        content.operation_seq
    );
    assert!(
        state
            .store
            .deleted_draft_copy_content_is_saved(&rig.binding, &rig.document)
            .await
            .unwrap()
    );
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LATER);
    assert_eq!(draft.generation, 3);
    assert_eq!(
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap(),
        Some(fresh)
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}

async fn collide_deleted_copy(
    rig: &DeletedCopyRig,
    peer_seq: u64,
) -> notryx_client_store::CreateOutboxOperation {
    let queued = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy(&rig.binding, &rig.document)
        .await
        .unwrap();
    let receipt = rig
        .session_b
        .create(&CreateMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: peer_seq,
            request: CreateNodeRequest {
                node_id: Uuid::now_v7().to_string().parse().unwrap(),
                parent_id: queued.request.parent_id,
                name: queued.request.name.clone(),
                kind: NodeKind::File,
                upload_id: None,
            },
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Created { .. }));
    let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Failed);
    assert_eq!(
        result.operation_id.as_deref(),
        Some(queued.operation_id.as_str())
    );
    queued
}

#[tokio::test]
async fn deleted_copy_reconfirmation_failure_rolls_back_attempt_and_preserves_receipt() {
    use sqlx::Connection;
    let rig = deleted_copy_rig().await;
    let failed = collide_deleted_copy(&rig, 2).await;
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(rig.directory.path().join("deleted-copy.sqlite")),
    )
    .await
    .unwrap();
    let prior_receipt: String =
        sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(&failed.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    sqlx::query("CREATE TRIGGER fail_deleted_copy_reconfirm BEFORE UPDATE ON deleted_draft_copy_intents BEGIN SELECT RAISE(ABORT, 'injected'); END").execute(&mut db).await.unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &failed.operation_id,
                    &rig.intent.parent_id,
                    "second-copy.md"
                )
                .await
                .is_err()
        );
        assert_eq!(
            state
                .store
                .read_deleted_draft_copy(&rig.binding, &rig.document)
                .await
                .unwrap(),
            Some(rig.intent.clone())
        );
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, LATER);
        assert_eq!(draft.generation, 3);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deleted_draft_copy_attempts")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let receipt: String =
        sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(&failed.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(receipt, prior_receipt);
    sqlx::query("DROP TRIGGER fail_deleted_copy_reconfirm")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let fresh = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .reconfirm_failed_deleted_draft_copy(
            &rig.binding,
            &rig.document,
            &failed.operation_id,
            &rig.intent.parent_id,
            "second-copy.md",
        )
        .await
        .unwrap();
    assert_eq!(fresh.local_bytes, CONFIRMED);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_repeated_collisions_preserve_attempt_history_and_only_retry_latest_selection()
{
    use sqlx::Connection;
    let rig = deleted_copy_rig().await;
    let first = collide_deleted_copy(&rig, 2).await;
    let second_intent = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .reconfirm_failed_deleted_draft_copy(
            &rig.binding,
            &rig.document,
            &first.operation_id,
            &rig.intent.parent_id,
            "second-copy.md",
        )
        .await
        .unwrap();
    let second = collide_deleted_copy(&rig, 3).await;
    assert_eq!(second.operation_id, second_intent.create_operation_id);
    let third = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .reconfirm_failed_deleted_draft_copy(
            &rig.binding,
            &rig.document,
            &second.operation_id,
            &rig.intent.parent_id,
            "third-copy.md",
        )
        .await
        .unwrap();
    let rig = restart(rig).await;
    {
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &first.operation_id,
                    &rig.intent.parent_id,
                    "second-copy.md"
                )
                .await
                .is_err()
        );
        assert_eq!(
            state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &rig.binding,
                    &rig.document,
                    &second.operation_id,
                    &rig.intent.parent_id,
                    "third-copy.md"
                )
                .await
                .unwrap(),
            third
        );
        assert_eq!(
            state
                .store
                .read_deleted_draft_copy(&rig.binding, &rig.document)
                .await
                .unwrap(),
            Some(third.clone())
        );
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, LATER);
        assert_eq!(third.local_bytes, CONFIRMED);
    }
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(rig.directory.path().join("deleted-copy.sqlite"))
            .read_only(true),
    )
    .await
    .unwrap();
    let attempts: Vec<(String, String, String)> = sqlx::query_as("SELECT old_create_operation_id,new_create_operation_id,new_name FROM deleted_draft_copy_attempts ORDER BY rowid").fetch_all(&mut db).await.unwrap();
    assert_eq!(
        attempts,
        vec![
            (
                first.operation_id.clone(),
                second.operation_id.clone(),
                "second-copy.md".into()
            ),
            (
                second.operation_id.clone(),
                third.create_operation_id.clone(),
                "third-copy.md".into()
            )
        ]
    );
    db.close().await.unwrap();
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        b""
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("second-copy.md")).unwrap(),
        b""
    );
    rig.server.shutdown().await.unwrap();
}
