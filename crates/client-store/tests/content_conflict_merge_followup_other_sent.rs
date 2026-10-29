include!("content_conflict_intent.rs");

use notryx_client_store::OutboxStatus;

#[tokio::test]
async fn merge_followup_waits_for_other_document_sent_before_its_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let binding = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    let opened = store
        .open_registered_document(&binding, DOC, BASE, "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, DOC, &opened.edit_session_id, 1, SENT)
        .await
        .unwrap();
    let original = store.queue_checkpoint(&binding, DOC, 1).await.unwrap();
    let original_sent = store.prepare_outbox(&binding).await.unwrap().unwrap();
    assert_eq!(original_sent.operation_id, original.operation_id);

    let other = store
        .open_registered_document(&binding, OTHER, b"other base\n", "other-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            OTHER,
            &other.edit_session_id,
            1,
            b"other pending\n",
        )
        .await
        .unwrap();
    let other_queued = store.queue_checkpoint(&binding, OTHER, 1).await.unwrap();
    store
        .save_registered_checkpoint(&binding, DOC, &opened.edit_session_id, 2, LOCAL)
        .await
        .unwrap();
    let followup = store.queue_checkpoint(&binding, DOC, 2).await.unwrap();
    assert_eq!(original.operation_seq, 1);
    assert_eq!(other_queued.operation_seq, 2);
    assert_eq!(followup.operation_seq, 3);
    store
        .apply_outbox_conflict(
            &binding,
            &OutboxConflict {
                operation_id: original.operation_id.clone(),
                operation_seq: original.operation_seq,
                workspace_epoch: binding.workspace_epoch.clone(),
                current_revision: "peer-rev".into(),
                current_etag: "\"peer-rev\"".into(),
            },
        )
        .await
        .unwrap();
    let (root, remote) = remote_nodes();
    let bootstrap = store
        .begin_bootstrap(&binding, &info(&binding))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &binding,
            &bootstrap,
            &InventoryPage {
                workspace_epoch: info(&binding).workspace_epoch,
                nodes: vec![root, remote.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&binding, &bootstrap, &info(&binding))
        .await
        .unwrap();
    store
        .bootstrap_publish(&binding, &bootstrap, &info(&binding))
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&binding, &remote, REMOTE)
            .await
            .unwrap()
    );
    let request = ContentConflictIntentRequest {
        generation: 2,
        remote_revision: "current-rev".into(),
        remote_hash: digest(REMOTE),
        remote_bytes: REMOTE.to_vec(),
        choice: ContentConflictChoice::SaveMerged {
            bytes: b"\xef\xbb\xbfmerged\n".to_vec(),
        },
    };
    let intent = store
        .record_content_conflict_intent(&binding, DOC, &original.operation_id, &request)
        .await
        .unwrap();
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({
        "workspace_epoch":binding.workspace_epoch, "device_stream_id":binding.device_stream_id,
        "last_seq":"1", "retired_through":"1"
    }))
    .unwrap();
    store.confirm_receipt_ack(&binding, &ack).await.unwrap();
    let other_sent = store.prepare_outbox(&binding).await.unwrap().unwrap();
    assert_eq!(other_sent.operation_id, other_queued.operation_id);
    assert_eq!(other_sent.operation_seq, 2);
    assert_eq!(other_sent.status, OutboxStatus::Sent);
    let before_other = store
        .read_registered_checkpoint(&binding, OTHER)
        .await
        .unwrap()
        .unwrap();
    let before_original = store
        .read_registered_checkpoint(&binding, DOC)
        .await
        .unwrap()
        .unwrap();
    let before_followup = store
        .read_outbox_operation(&binding, &followup.operation_id)
        .await
        .unwrap()
        .unwrap();

    assert!(
        store
            .inspect_content_conflict_merge_followup(
                &binding,
                &original.operation_id,
                intent.version
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .read_outbox_operation(&binding, &other_sent.operation_id)
            .await
            .unwrap()
            == Some(other_sent)
    );
    assert!(
        store
            .read_outbox_operation(&binding, &followup.operation_id)
            .await
            .unwrap()
            == Some(before_followup)
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&binding, OTHER)
            .await
            .unwrap()
            .unwrap(),
        before_other
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&binding, DOC)
            .await
            .unwrap()
            .unwrap(),
        before_original
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&binding, &original.operation_id)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_eq!(store.receipt_ack_through(&binding).await.unwrap(), 1);
    assert_eq!(store.confirmed_receipt_through(&binding).await.unwrap(), 1);
    assert_eq!(
        store
            .read_outbox_operation(&binding, &followup.operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        OutboxStatus::Queued
    );
    store
        .save_registered_checkpoint(
            &binding,
            OTHER,
            &other.edit_session_id,
            2,
            b"other still editable\n",
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            DOC,
            &opened.edit_session_id,
            3,
            b"local still editable\n",
        )
        .await
        .unwrap();
}
