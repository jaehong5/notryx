include!("conflict_choose_remote_command_tests.rs");

#[tokio::test]
async fn pending_result_rejects_closed_or_replaced_edit_session() {
    for replace in [false, true] {
        let rig = rig_with_followup(true).await;
        let request = choice_request(&rig, 2, None);
        let ready = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let pending = tokio::spawn({
            let runtime = Runtime(rig.runtime.0.clone());
            let ready = ready.clone();
            let resume = resume.clone();
            async move {
                crate::conflict_snapshot_native::choose_remote_for_renderer_with_hook(
                    &runtime, request, ready, resume,
                )
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified())
            .await
            .unwrap();
        let (intent, draft, original, followup, other) = {
            let mut state = rig.runtime.0.lock().await;
            let intent = state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                intent.choice,
                notryx_client_store::ContentConflictChoice::ChooseRemote
            );
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
            let latest = state
                .store
                .document_outbox_state(&rig.binding, &rig.document)
                .await
                .unwrap()
                .latest
                .unwrap();
            let followup = state
                .store
                .read_outbox_operation(&rig.binding, &latest.operation_id)
                .await
                .unwrap()
                .unwrap();
            state
                .store
                .close_registered_document(&rig.binding, &rig.document, &rig.edit_session)
                .await
                .unwrap();
            state.gate.close_session(&rig.edit_session).unwrap();
            state.product_sessions.remove(&rig.edit_session);
            let other = if replace {
                let id = Uuid::now_v7().to_string();
                let opened = state
                    .store
                    .open_registered_document(&rig.binding, &id, b"other base\n", "other-rev")
                    .await
                    .unwrap();
                state
                    .gate
                    .open_session(opened.edit_session_id.clone())
                    .unwrap();
                state.product_sessions.insert(
                    opened.edit_session_id.clone(),
                    ProductSession {
                        document_id: id.clone(),
                        binding: rig.binding.clone(),
                        kind: ProductSessionKind::Active,
                    },
                );
                state
                    .store
                    .save_registered_checkpoint(
                        &rig.binding,
                        &id,
                        &opened.edit_session_id,
                        1,
                        b"other draft\n",
                    )
                    .await
                    .unwrap();
                Some((id, opened.edit_session_id))
            } else {
                None
            };
            (intent, draft, original, followup, other)
        };
        resume.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err(), "replace={replace}, result={result:?}");
        let mut state = rig.runtime.0.lock().await;
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
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap(),
            draft
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
                .read_outbox_operation(&rig.binding, &followup.operation_id)
                .await
                .unwrap()
                .unwrap(),
            followup
        );
        if let Some((id, session)) = other {
            assert!(state.product_sessions.contains_key(&session));
            assert!(state.gate.check_session(&session).is_ok());
            assert!(
                state
                    .store
                    .dirty_registered_session_snapshot(&rig.binding, &id, &session)
                    .await
                    .unwrap()
                    .is_some()
            );
            let other_draft = state
                .store
                .read_registered_checkpoint(&rig.binding, &id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(other_draft.bytes, b"other draft\n");
            assert_eq!(other_draft.base_bytes, b"other base\n");
        }
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
async fn completed_result_allows_its_expected_session_close() {
    let rig = rig().await;
    let request = choice_request(&rig, 1, None);
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
    let ready = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let pending = tokio::spawn({
        let runtime = Runtime(rig.runtime.0.clone());
        let ready = ready.clone();
        let resume = resume.clone();
        async move {
            crate::conflict_snapshot_native::choose_remote_for_renderer_with_hook(
                &runtime, request, ready, resume,
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified())
        .await
        .unwrap();
    {
        let state = rig.runtime.0.lock().await;
        assert!(!state.product_sessions.contains_key(&rig.edit_session));
        assert!(state.gate.check_session(&rig.edit_session).is_err());
    }
    resume.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Completed { .. }
    ));
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        LOCAL
    );
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
