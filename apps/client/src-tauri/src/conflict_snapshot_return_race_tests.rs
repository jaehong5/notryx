#[tokio::test]
async fn encoded_conflict_response_is_rejected_after_renderer_or_registration_replacement() {
    for replace_registration in [false, true] {
        let rig = rig().await;
        let request = notryx_protocol::WorkspaceContentConflictRequest {
            renderer_session_id: OWNER.into(),
            binding: crate::to_contract(rig.binding.clone()),
            document_id: rig.document.clone(),
            edit_session_id: rig.edit_session.clone(),
            generation: "1".into(),
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn({
            let runtime = Runtime(rig.runtime.0.clone());
            async move {
                super::prepare_content_conflict_for_renderer_with_hook(
                    &runtime, request, ready_tx, resume_rx,
                )
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), ready_rx)
            .await
            .unwrap()
            .unwrap();
        let replacement = if replace_registration {
            let pairing = server_test_support::pairing_request(&rig.config).await.0;
            Some(Arc::new(
                Session::pair(
                    &format!("http://{}", rig.server.local_addr()),
                    pairing.workspace_id,
                    &pairing.pairing_secret,
                )
                .await
                .unwrap(),
            ))
        } else {
            None
        };
        let new_binding = replacement
            .as_ref()
            .map(|session| registration_binding(session.binding()));
        {
            let mut state = rig.runtime.0.lock().await;
            state.store.reset_edit_sessions();
            state.product_sessions.clear();
            if let Some(session) = replacement {
                state
                    .store
                    .activate_registration(new_binding.as_ref().unwrap())
                    .await
                    .unwrap();
                state.session = Some(session);
            } else {
                state.gate.replace_owner("replacement-owner".into());
            }
        }
        resume_tx.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "encoded source must not be returned to the previous owner or registration"
        );
        assert_preserved(&rig).await;
        if let Some(binding) = new_binding {
            let mut state = rig.runtime.0.lock().await;
            assert!(
                state
                    .store
                    .list_outbox(&binding, 0, 100)
                    .await
                    .unwrap()
                    .entries
                    .is_empty()
            );
            assert!(
                state
                    .store
                    .read_registered_checkpoint(&binding, &rig.document)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
