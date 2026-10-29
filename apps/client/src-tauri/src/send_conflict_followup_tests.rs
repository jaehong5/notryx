#[tokio::test]
async fn content_conflict_followup_abandon_preserves_peer_and_unrelated_content_with_lost_receipt_retry()
 {
    for lost_response in [false, true] {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        assert_eq!(
            send_one(rig.state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Committed
        );
        let session = rig.state.lock().await.session.clone().unwrap();
        let runtime = crate::Runtime(rig.state.clone());
        let gate = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        crate::sync_bootstrap::bootstrap_metadata(&runtime, &gate, &session)
            .await
            .unwrap();
        let inventory = session.inventory(None).await.unwrap();
        let original = inventory
            .nodes
            .iter()
            .find(|node| node.name == "Other.md")
            .unwrap()
            .clone();
        let other = inventory
            .nodes
            .iter()
            .find(|node| node.name == "Restored.md")
            .unwrap()
            .clone();
        let document = original.node_id.to_string();
        let other_id = other.node_id.to_string();
        let (edit, first, followup, unrelated) = {
            let mut state = rig.state.lock().await;
            let edit = state
                .store
                .open_registered_document(
                    &rig.binding,
                    &document,
                    b"other base\n",
                    original.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &rig.binding,
                    &document,
                    &edit.edit_session_id,
                    1,
                    b"local sent\n",
                )
                .await
                .unwrap();
            let first = state
                .store
                .queue_checkpoint(&rig.binding, &document, 1)
                .await
                .unwrap();
            state
                .store
                .prepare_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &rig.binding,
                    &document,
                    &edit.edit_session_id,
                    2,
                    b"newer local\n",
                )
                .await
                .unwrap();
            let followup = state
                .store
                .queue_checkpoint(&rig.binding, &document, 2)
                .await
                .unwrap();
            let other_edit = state
                .store
                .open_registered_document(
                    &rig.binding,
                    &other_id,
                    b"deleted base\n",
                    other.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &rig.binding,
                    &other_id,
                    &other_edit.edit_session_id,
                    1,
                    b"unrelated edit\n",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&rig.binding, &other_id, &other_edit.edit_session_id)
                .await
                .unwrap();
            let unrelated = state
                .store
                .queue_checkpoint(&rig.binding, &other_id, 1)
                .await
                .unwrap();
            (edit, first, followup, unrelated)
        };
        assert_eq!(
            (
                first.operation_seq,
                followup.operation_seq,
                unrelated.operation_seq
            ),
            (2, 3, 4)
        );
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&rig.config)
            .await
            .0;
        let peer = Session::pair(
            &format!("http://{}", rig.server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap();
        let peer_receipt = peer
            .put_content(&notryx_client_runtime::ContentMutation {
                binding: peer.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: original.node_id,
                expected_revision: original.content_rev.clone().unwrap(),
                bytes: b"peer edit\n".to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(
            peer_receipt,
            notryx_protocol::OperationReceipt::Committed { .. }
        ));
        let conflict = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(conflict.status, WorkspaceSendStatus::Conflict);
        assert_eq!(
            conflict.operation_id.as_deref(),
            Some(first.operation_id.as_str())
        );
        assert_eq!(
            std::fs::read(rig.notes.join("Other.md")).unwrap(),
            b"peer edit\n"
        );
        let gate = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        crate::sync_catchup::catch_up_metadata(&runtime, &gate, &session)
            .await
            .unwrap();
        let remote = runtime
            .0
            .lock()
            .await
            .store
            .current_remote_file(&rig.binding, original.node_id)
            .await
            .unwrap()
            .unwrap();
        let remote_bytes = session.content(&remote).await.unwrap();
        assert_eq!(remote_bytes, b"peer edit\n");
        let conflict_ack = session.acknowledge(first.operation_seq).await.unwrap();
        let (intent, marked) = {
            let mut state = rig.state.lock().await;
            state
                .store
                .confirm_receipt_ack(&rig.binding, &conflict_ack)
                .await
                .unwrap();
            let request = notryx_client_store::ContentConflictIntentRequest {
                generation: 2,
                remote_revision: remote.content_rev.as_ref().unwrap().as_str().into(),
                remote_hash: remote.content_hash.clone().unwrap(),
                remote_bytes: remote_bytes.clone(),
                choice: notryx_client_store::ContentConflictChoice::SaveMerged {
                    bytes: b"merged\n".to_vec(),
                },
            };
            let intent = state
                .store
                .record_content_conflict_intent(
                    &rig.binding,
                    &document,
                    &first.operation_id,
                    &request,
                )
                .await
                .unwrap();
            let candidate = state
                .store
                .inspect_content_conflict_merge_followup(
                    &rig.binding,
                    &first.operation_id,
                    intent.version,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(candidate.operation_id, followup.operation_id);
            assert_eq!(candidate.operation_seq, followup.operation_seq);
            assert_eq!(candidate.generation, followup.generation);
            assert_eq!(candidate.payload_hash, followup.payload_hash);
            assert_eq!(candidate.bytes, followup.bytes);
            let marked = state
                .store
                .request_content_conflict_merge_followup(
                    &rig.binding,
                    &first.operation_id,
                    intent.version,
                    &candidate.operation_id,
                    candidate.operation_seq,
                    candidate.generation,
                    &candidate.payload_hash,
                )
                .await
                .unwrap();
            assert_eq!(marked.operation_id, followup.operation_id);
            assert_eq!(marked.operation_seq, followup.operation_seq);
            assert_eq!(
                marked.abandon_state,
                notryx_client_store::OutboxAbandonState::Requested
            );
            (intent, marked)
        };
        let expected_head = if lost_response {
            let prepared = rig
                .state
                .lock()
                .await
                .store
                .prepare_outbox_matching(&rig.binding, &marked)
                .await
                .unwrap()
                .unwrap();
            let request = abandon(&session, &prepared).unwrap();
            let receipt = session.abandon(&request).await.unwrap();
            assert!(matches!(
                receipt,
                notryx_protocol::OperationReceipt::Abandoned { .. }
            ));
            assert_eq!(
                rig.state
                    .lock()
                    .await
                    .store
                    .receipt_ack_through(&rig.binding)
                    .await
                    .unwrap(),
                first.operation_seq
            );
            Some(session.server_info().await.unwrap().head_seq)
        } else {
            None
        };
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Abandoned);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(followup.operation_id.as_str())
        );
        if let Some(head) = expected_head {
            assert_eq!(session.server_info().await.unwrap().head_seq, head);
        }
        {
            let mut state = rig.state.lock().await;
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
                followup.operation_seq
            );
            let draft = state
                .store
                .read_registered_checkpoint(&rig.binding, &document)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.bytes, b"newer local\n");
            assert_eq!(draft.base_bytes, b"other base\n");
            assert_eq!(
                draft.base_revision,
                original.content_rev.as_ref().unwrap().as_str()
            );
            let saved = state
                .store
                .read_outbox_operation(&rig.binding, &first.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved.status, OutboxStatus::Conflict);
            assert_eq!(saved.bytes, b"local sent\n");
            let cancelled = state
                .store
                .read_outbox_operation(&rig.binding, &followup.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(cancelled.status, OutboxStatus::Abandoned);
            assert_eq!(cancelled.bytes, b"newer local\n");
            assert_eq!(
                state
                    .store
                    .read_content_conflict_intent(&rig.binding, &first.operation_id)
                    .await
                    .unwrap(),
                Some(intent)
            );
        }
        assert_eq!(
            std::fs::read(rig.notes.join("Other.md")).unwrap(),
            b"peer edit\n"
        );
        let unrelated_result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(unrelated_result.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            unrelated_result.operation_id.as_deref(),
            Some(unrelated.operation_id.as_str())
        );
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"unrelated edit\n"
        );
        assert_eq!(
            std::fs::read(rig.notes.join("Other.md")).unwrap(),
            b"peer edit\n"
        );
        let mut state = rig.state.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"newer local\n");
        assert_eq!(draft.base_bytes, b"other base\n");
        state
            .store
            .close_registered_document(&rig.binding, &document, &edit.edit_session_id)
            .await
            .unwrap();
        drop(state);
        rig.server.shutdown().await.unwrap();
    }
}
