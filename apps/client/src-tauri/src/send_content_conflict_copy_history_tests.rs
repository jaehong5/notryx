use crate::conflict_snapshot_native::test_support::send_copy_history::*;

#[tokio::test]
async fn saved_copy_remains_saved_after_later_base_advances_before_ack() {
    let (rig, copy, first, later) = saved_copy_and_later_edit().await;
    let prepared = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .prepare_outbox(&rig.binding)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prepared.operation_id, later.operation_id);
    let receipt = rig
        .session
        .put_content(&ContentMutation {
            binding: rig.session.binding().clone(),
            operation_id: later.operation_id.parse().unwrap(),
            operation_seq: later.operation_seq,
            node_id: copy.node_id.parse().unwrap(),
            expected_revision: notryx_domain::ContentRevision::new(
                later.expected_revision.clone().unwrap(),
            )
            .unwrap(),
            bytes: later.bytes.clone(),
        })
        .await
        .unwrap();
    let OperationReceipt::Committed {
        content_rev,
        content_hash,
        ..
    } = receipt
    else {
        panic!("expected commit")
    };
    rig.runtime
        .0
        .lock()
        .await
        .store
        .apply_outbox_commit(
            &rig.binding,
            &notryx_client_store::OutboxCommit {
                operation_id: later.operation_id.clone(),
                operation_seq: later.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                document_id: copy.node_id.clone(),
                content_revision: content_rev.as_str().into(),
                content_hash,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_outbox_operation(&rig.binding, &first.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"newer local\n"
    );
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::Saved {
            operation_id: first.operation_id
        }
    );
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn saved_copy_remains_saved_after_confirmed_later_edit_retires_old_payload() {
    let (rig, copy, first, later) = saved_copy_and_later_edit().await;
    let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Committed);
    assert_eq!(
        result.operation_id.as_deref(),
        Some(later.operation_id.as_str())
    );
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
    )
    .await
    .unwrap();
    let retired: i64 =
        sqlx::query_scalar("SELECT payload_retired FROM client_outbox WHERE operation_id=?")
            .bind(&first.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(retired, 1);
    db.close().await.unwrap();
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::Saved {
            operation_id: first.operation_id
        }
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("copy.md")).unwrap(),
        b"later copy edit\n"
    );
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
