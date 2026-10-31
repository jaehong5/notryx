use crate::conflict_snapshot_native::test_support::copy_status::*;

#[tokio::test]
async fn copy_status_recovers_target_and_states_across_all_confirmed_server_steps() {
    let rig = rig_with_followup(true).await;
    let request = copy_status_request(&rig);
    assert!(copy_status(&rig, request.clone()).await.unwrap()["copy"].is_null());
    let confirmed = confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    let initial = copy_status(&rig, request.clone()).await.unwrap();
    assert_eq!(
        initial["binding"],
        serde_json::to_value(crate::to_contract(rig.binding.clone())).unwrap()
    );
    assert_eq!(initial["document_id"], rig.document);
    assert_eq!(initial["operation_id"], rig.operation.operation_id);
    assert_eq!(initial["copy"]["version"], confirmed["version"]);
    assert_eq!(
        initial["copy"]["parent_id"],
        rig.initial.parent_id.unwrap().to_string()
    );
    assert_eq!(initial["copy"]["name"], "my-copy.md");
    assert_eq!(initial["copy"]["node_id"], confirmed["node_id"]);
    assert_eq!(
        initial["copy"]["create_operation_id"],
        confirmed["create_operation_id"]
    );
    assert_eq!(initial["copy"]["create_status"], "not_queued");
    assert_eq!(initial["copy"]["content_status"], "not_queued");
    let progress = progress_request(&rig, confirmed["version"].as_str().unwrap());
    progress_copy(&rig, progress.clone()).await.unwrap();
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Abandoned
    );
    progress_copy(&rig, progress.clone()).await.unwrap();
    let queued = copy_status(&rig, request.clone()).await.unwrap();
    assert_eq!(queued["copy"]["create_status"], "pending");
    assert_eq!(queued["copy"]["content_status"], "not_queued");
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    let created = copy_status(&rig, request.clone()).await.unwrap();
    assert_eq!(created["copy"]["create_status"], "created");
    assert_eq!(created["copy"]["content_status"], "not_queued");
    progress_copy(&rig, progress.clone()).await.unwrap();
    assert_eq!(
        copy_status(&rig, request.clone()).await.unwrap()["copy"]["content_status"],
        "pending"
    );
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    let saved = copy_status(&rig, request).await.unwrap();
    assert_eq!(saved["copy"]["content_status"], "saved");
    assert_eq!(saved["copy"]["node_id"], confirmed["node_id"]);
    let state = rig.runtime.0.lock().await;
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("my-copy.md")).unwrap(),
        b"newer local\n"
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_status_distinguishes_name_conflict_and_never_reports_it_saved() {
    let rig = rig_with_followup(true).await;
    let mut choice = copy_request(&rig, None);
    choice.name = "example.md".into();
    let confirmed = confirm_copy(&rig, choice).await.unwrap();
    let progress = progress_request(&rig, confirmed["version"].as_str().unwrap());
    progress_copy(&rig, progress.clone()).await.unwrap();
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Abandoned
    );
    progress_copy(&rig, progress).await.unwrap();
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Failed
    );
    let current = copy_status(&rig, copy_status_request(&rig)).await.unwrap();
    assert_eq!(current["copy"]["create_status"], "needs_review");
    assert_eq!(current["copy"]["content_status"], "not_queued");
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_status_rejects_other_binding_document_or_renderer() {
    let rig = rig_with_followup(true).await;
    confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    for kind in ["binding", "document", "renderer"] {
        let mut request = copy_status_request(&rig);
        match kind {
            "binding" => request.binding.device_stream_id = Uuid::new_v4().to_string(),
            "document" => request.document_id = rig.initial.parent_id.unwrap().to_string(),
            "renderer" => request.renderer_session_id = "old-owner".into(),
            _ => unreachable!(),
        }
        assert!(copy_status(&rig, request).await.is_err(), "{kind}");
    }
    assert_copy_confirm_preserves_sources(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_status_rejects_owner_or_registration_replacement_during_query() {
    for change in ["owner", "registration"] {
        let rig = rig_with_followup(true).await;
        confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
        let ready = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let runtime = Runtime(rig.runtime.0.clone());
        let request = copy_status_request(&rig);
        let task = tokio::spawn({
            let ready = ready.clone();
            let resume = resume.clone();
            async move {
                crate::conflict_snapshot_native::copy_status_for_renderer_with_hook(
                    &runtime, request, ready, resume,
                )
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified())
            .await
            .unwrap();
        if change == "owner" {
            let mut state = rig.runtime.0.lock().await;
            state.store.reset_edit_sessions();
            state.product_sessions.clear();
            crate::search_native::replace_renderer(&mut state, "new-owner".into());
        } else {
            let pairing = server_test_support::pairing_request(&rig.config).await.0;
            let replacement = Arc::new(
                Session::pair(
                    &format!("http://{}", rig.server.local_addr()),
                    pairing.workspace_id,
                    &pairing.pairing_secret,
                )
                .await
                .unwrap(),
            );
            let binding = registration_binding(replacement.binding());
            let mut state = rig.runtime.0.lock().await;
            state.store.reset_edit_sessions();
            state.product_sessions.clear();
            state.store.activate_registration(&binding).await.unwrap();
            state.session = Some(replacement);
        }
        resume.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
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
                .unwrap()
                .bytes,
            b"newer local\n"
        );
        drop(state);
        assert!(!rig.config.workspace.notes_root.join("my-copy.md").exists());
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
