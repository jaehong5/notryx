include!("content_conflict_merge_queue.rs");

const SECOND_REMOTE: &[u8] = b"second remote\n";
const SECOND_MERGED: &[u8] = b"\xef\xbb\xbfsecond merged\n";

async fn merge_commit(store: &mut ClientStore, rig: &Rig, item: &OutboxOperation, revision: &str) {
    let sent = store
        .prepare_outbox_matching(&rig.binding, item)
        .await
        .unwrap()
        .unwrap();
    store
        .apply_outbox_commit(
            &rig.binding,
            &notryx_client_store::OutboxCommit {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                document_id: DOC.to_owned(),
                content_revision: revision.to_owned(),
                content_hash: sent.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
}

async fn assert_completed(store: &mut ClientStore, rig: &Rig, operation: &str, generation: u64) {
    assert!(
        matches!(store.read_content_conflict_resolution_state(&rig.binding, operation).await.unwrap(),
        notryx_client_store::ContentConflictResolutionState::Completed { generation: actual, frozen: false } if actual == generation)
    );
}

async fn consecutive_merge_test(finish_second: bool, corrupt_checkpoint: bool) {
    let (rig, mut store) = rig().await;
    let first_intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let first_merge = queue_merge(&mut store, &rig, first_intent.version)
        .await
        .unwrap();
    merge_commit(&mut store, &rig, &first_merge, "first-merge-rev").await;
    assert_completed(&mut store, &rig, &rig.operation, first_merge.generation).await;

    let opened = store
        .open_registered_document(&rig.binding, DOC, REMOTE, "current-rev")
        .await
        .unwrap();
    let new_edit = b"edit after first merge\n";
    let new_generation = first_merge.generation + 1;
    store
        .save_registered_checkpoint(
            &rig.binding,
            DOC,
            &opened.edit_session_id,
            new_generation,
            new_edit,
        )
        .await
        .unwrap();
    let next = store
        .queue_checkpoint(&rig.binding, DOC, new_generation)
        .await
        .unwrap();
    assert_eq!(next.expected_revision.as_deref(), Some("first-merge-rev"));
    let sent = store
        .prepare_outbox_matching(&rig.binding, &next)
        .await
        .unwrap()
        .unwrap();
    store
        .apply_outbox_conflict(
            &rig.binding,
            &OutboxConflict {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                current_revision: "second-remote-rev".to_owned(),
                current_etag: "\"second-remote-rev\"".to_owned(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    let (_, original) = remote_nodes();
    let mut value = serde_json::to_value(original).unwrap();
    value["last_change_seq"] = json!("3");
    value["content_rev"] = json!("second-remote-rev");
    value["content_etag"] = json!("\"second-remote-rev\"");
    value["node_etag"] = json!("\"second-node\"");
    value["content_hash"] = json!(digest(SECOND_REMOTE));
    value["size"] = json!(SECOND_REMOTE.len().to_string());
    let node: NodeState = serde_json::from_value(value).unwrap();
    store
        .apply_changes(
            &rig.binding,
            &ChangesPage {
                workspace_epoch: info(&rig.binding).workspace_epoch,
                from_exclusive: ChangeSeq::new(2),
                through_seq: ChangeSeq::new(3),
                covered_through: ChangeSeq::new(3),
                has_more: false,
                nodes: vec![node.clone()],
            },
            ChangeSeq::new(3),
        )
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&rig.binding, &node, SECOND_REMOTE)
            .await
            .unwrap()
    );
    let request = ContentConflictIntentRequest {
        generation: new_generation,
        remote_revision: "second-remote-rev".to_owned(),
        remote_hash: digest(SECOND_REMOTE),
        remote_bytes: SECOND_REMOTE.to_vec(),
        choice: ContentConflictChoice::SaveMerged {
            bytes: SECOND_MERGED.to_vec(),
        },
    };
    let second_intent = store
        .record_content_conflict_intent(&rig.binding, DOC, &sent.operation_id, &request)
        .await
        .unwrap();
    assert_eq!(second_intent.base_bytes, first_merge.bytes);
    assert_eq!(second_intent.local_bytes, new_edit);
    store
        .close_registered_document(&rig.binding, DOC, &opened.edit_session_id)
        .await
        .unwrap();
    let second_merge = store
        .queue_content_conflict_merge(&rig.binding, &sent.operation_id, second_intent.version)
        .await
        .unwrap();
    assert_eq!(
        second_merge.expected_revision.as_deref(),
        Some("second-remote-rev")
    );
    assert_eq!(second_merge.bytes, SECOND_MERGED);
    assert!(second_merge.operation_seq > sent.operation_seq);
    if corrupt_checkpoint {
        let mut db = connect(&rig.path).await;
        let changed = sqlx::query("UPDATE draft_checkpoints SET bytes=? WHERE document_id=(SELECT storage_id FROM registered_documents WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active') AND document_id=?)")
            .bind(b"tampered merge checkpoint\n".as_slice()).bind(DOC).execute(&mut db).await.unwrap().rows_affected();
        assert_eq!(changed, 1);
        db.close().await.unwrap();
        let result = store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await;
        let second_result = store
            .read_content_conflict_resolution_state(&rig.binding, &sent.operation_id)
            .await;
        let draft = store
            .read_registered_checkpoint(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, second_merge.generation);
        assert_eq!(draft.bytes, b"tampered merge checkpoint\n");
        assert_eq!(draft.base_bytes, SECOND_REMOTE);
        assert_eq!(
            store
                .read_outbox_operation(&rig.binding, &second_merge.operation_id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            SECOND_MERGED
        );
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &sent.operation_id)
                .await
                .unwrap(),
            Some(second_intent)
        );
        assert!(result.is_err());
        assert!(second_result.is_err());
        return;
    }
    if finish_second {
        merge_commit(&mut store, &rig, &second_merge, "second-merge-rev").await;
    }

    assert_completed(&mut store, &rig, &rig.operation, first_merge.generation).await;
    if finish_second {
        assert_completed(
            &mut store,
            &rig,
            &sent.operation_id,
            second_merge.generation,
        )
        .await;
        assert!(
            !store
                .document_outbox_state(&rig.binding, DOC)
                .await
                .unwrap()
                .has_conflict
        );
        assert!(
            store
                .first_content_conflict(&rig.binding, DOC)
                .await
                .unwrap()
                .is_none()
        );
    } else {
        assert!(matches!(
            store
                .read_content_conflict_resolution_state(&rig.binding, &sent.operation_id)
                .await
                .unwrap(),
            notryx_client_store::ContentConflictResolutionState::Pending {
                choice: notryx_client_store::ContentConflictChoiceKind::SaveMerged,
                ..
            }
        ));
        assert!(
            store
                .document_outbox_state(&rig.binding, DOC)
                .await
                .unwrap()
                .has_conflict
        );
        assert_eq!(
            store
                .first_content_conflict(&rig.binding, DOC)
                .await
                .unwrap()
                .unwrap()
                .operation_id,
            sent.operation_id
        );
    }
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, SECOND_MERGED);
    assert_eq!(draft.generation, second_merge.generation);
    if finish_second {
        assert_eq!(draft.base_bytes, SECOND_MERGED);
        assert_eq!(draft.base_revision, "second-merge-rev");
    } else {
        assert_eq!(draft.base_bytes, SECOND_REMOTE);
        assert_eq!(draft.base_revision, "second-remote-rev");
    }
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(first_intent)
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &sent.operation_id)
            .await
            .unwrap(),
        Some(second_intent)
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        LOCAL
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &sent.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        new_edit
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_completed(&mut reopened, &rig, &rig.operation, first_merge.generation).await;
    if finish_second {
        assert_completed(
            &mut reopened,
            &rig,
            &sent.operation_id,
            second_merge.generation,
        )
        .await;
    }
}

#[tokio::test]
async fn consecutive_merge_preserves_first_completion_while_second_is_pending() {
    consecutive_merge_test(false, false).await;
}

#[tokio::test]
async fn consecutive_merge_preserves_both_completions_after_second_ack() {
    consecutive_merge_test(true, false).await;
}

#[tokio::test]
async fn consecutive_merge_rejects_corrupt_second_checkpoint_at_approved_generation() {
    consecutive_merge_test(false, true).await;
}
