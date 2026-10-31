use crate::conflict_snapshot_native::test_support::send_copy_content::*;

#[tokio::test]
async fn confirmed_copy_content_uses_created_revision_and_survives_lost_put_receipt() {
    for lost in [false, true] {
        let rig = rig_with_followup(true).await;
        let (copy, create, created) = created_copy(&rig).await;
        let payload = {
            let mut state = rig.runtime.0.lock().await;
            let item = state
                .store
                .queue_content_conflict_copy_content(
                    &rig.binding,
                    &rig.operation.operation_id,
                    copy.intent_version,
                )
                .await
                .unwrap();
            assert_eq!(item.document_id, copy.node_id);
            assert_eq!(item.bytes, b"newer local\n");
            assert_eq!(
                item.expected_revision.as_deref(),
                Some(created.content_rev.as_ref().unwrap().as_str())
            );
            assert_eq!(item.operation_seq, create.operation_seq + 1);
            assert_ne!(item.operation_id, create.operation_id);
            assert_eq!(
                state
                    .store
                    .queue_content_conflict_copy_content(
                        &rig.binding,
                        &rig.operation.operation_id,
                        copy.intent_version
                    )
                    .await
                    .unwrap(),
                item
            );
            let draft = state
                .store
                .read_registered_checkpoint(&rig.binding, &copy.node_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.bytes, b"newer local\n");
            assert_eq!(draft.base_bytes, b"");
            assert_eq!(
                draft.base_revision,
                created.content_rev.as_ref().unwrap().as_str()
            );
            item
        };
        preserve_original(&rig, &copy).await;
        let head = if lost {
            let sent = rig
                .runtime
                .0
                .lock()
                .await
                .store
                .prepare_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(sent.operation_id, payload.operation_id);
            let receipt = rig
                .session
                .put_content(&ContentMutation {
                    binding: rig.session.binding().clone(),
                    operation_id: sent.operation_id.parse().unwrap(),
                    operation_seq: sent.operation_seq,
                    node_id: created.node_id,
                    expected_revision: created.content_rev.clone().unwrap(),
                    bytes: sent.bytes.clone(),
                })
                .await
                .unwrap();
            assert!(matches!(receipt, OperationReceipt::Committed { .. }));
            Some(rig.session.server_info().await.unwrap().head_seq)
        } else {
            None
        };
        let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(payload.operation_id.as_str())
        );
        if let Some(head) = head {
            assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
        }
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("copy.md")).unwrap(),
            b"newer local\n"
        );
        assert_eq!(
            rig.runtime
                .0
                .lock()
                .await
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            payload.operation_seq
        );
        preserve_original(&rig, &copy).await;
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn copied_content_condition_rejects_peer_write_and_preserves_local_bytes() {
    let rig = rig_with_followup(true).await;
    let (copy, _, created) = created_copy(&rig).await;
    let payload = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_content_conflict_copy_content(
            &rig.binding,
            &rig.operation.operation_id,
            copy.intent_version,
        )
        .await
        .unwrap();
    let peer_result = rig
        .peer
        .put_content(&ContentMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: created.node_id,
            expected_revision: created.content_rev.clone().unwrap(),
            bytes: b"peer copy edit\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(peer_result, OperationReceipt::Committed { .. }));
    let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Conflict);
    assert_eq!(
        result.operation_id.as_deref(),
        Some(payload.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("copy.md")).unwrap(),
        b"peer copy edit\n"
    );
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &copy.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, b"");
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &payload.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"newer local\n"
    );
    drop(state);
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_content_cannot_be_queued_before_successful_create_receipt() {
    let rig = rig_with_followup(true).await;
    let (copy, _) = queue_confirmed_copy(&rig, "copy.md").await;
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .queue_content_conflict_copy_content(
                &rig.binding,
                &rig.operation.operation_id,
                copy.intent_version
            )
            .await
            .is_err()
    );
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
