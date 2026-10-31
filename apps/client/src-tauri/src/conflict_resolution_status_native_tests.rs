use crate::conflict_snapshot_native::test_support::snapshot::*;

async fn seed_pending_status(rig: &Rig) -> notryx_client_store::ContentConflictIntent {
    let shown = prepare(rig).await.unwrap();
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: shown.generation,
        remote_revision: shown.remote_revision,
        remote_hash: rig.first_peer.content_hash.clone().unwrap(),
        remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::SaveMerged { bytes: b"merged bytes\n".to_vec() },
    };
    rig.runtime.0.lock().await.store.record_content_conflict_intent(
        &rig.binding, &rig.document, &rig.operation.operation_id, &request,
    ).await.unwrap()
}

async fn status_json(rig: &Rig) -> Result<serde_json::Value, String> {
    let response = crate::conflict_snapshot_native::read_resolution_for_renderer(
        &rig.runtime, OWNER, &rig.binding, &rig.document, &rig.operation.operation_id,
    ).await?;
    Ok(serde_json::to_value(response).unwrap())
}

fn spawn_delayed_status(rig: &Rig, ready: Arc<Notify>, resume: Arc<Notify>) -> tokio::task::JoinHandle<bool> {
    let runtime = Runtime(rig.runtime.0.clone());
    let binding = rig.binding.clone();
    let document = rig.document.clone();
    let operation = rig.operation.operation_id.clone();
    tokio::spawn(async move {
        crate::conflict_snapshot_native::read_resolution_for_renderer_with_hook(
            &runtime, OWNER, &binding, &document, &operation, ready, resume,
        ).await.is_err()
    })
}

#[tokio::test]
async fn resolution_status_is_metadata_only_and_checks_operation_document_and_binding() {
    let rig = rig().await;
    let absent = status_json(&rig).await.unwrap();
    assert_eq!(absent["state"], serde_json::json!({"status":"not_found"}));
    let intent = seed_pending_status(&rig).await;
    let before = rig.session.server_info().await.unwrap().head_seq;
    let response = status_json(&rig).await.unwrap();
    assert_eq!(response["binding"], serde_json::to_value(crate::to_contract(rig.binding.clone())).unwrap());
    assert_eq!(response["document_id"], rig.document);
    assert_eq!(response["operation_id"], rig.operation.operation_id);
    assert_eq!(response["state"], serde_json::json!({
        "status":"pending", "version":intent.version.to_string(),
        "generation":intent.generation.to_string(), "choice":"save_merged",
        "needs_review":false, "frozen":false,
    }));
    let rendered = response.to_string();
    for source in ["server source", "local draft", "peer first", "merged bytes", "base_bytes", "local_bytes", "remote_bytes", "merged_bytes"] {
        assert!(!rendered.contains(source), "metadata response leaked {source}");
    }
    let wrong_document = Uuid::now_v7().to_string();
    assert!(crate::conflict_snapshot_native::read_resolution_for_renderer(
        &rig.runtime, OWNER, &rig.binding, &wrong_document, &rig.operation.operation_id,
    ).await.is_err());
    let mut wrong_binding = rig.binding.clone();
    wrong_binding.device_stream_id = Uuid::new_v4().to_string();
    assert!(crate::conflict_snapshot_native::read_resolution_for_renderer(
        &rig.runtime, OWNER, &wrong_binding, &rig.document, &rig.operation.operation_id,
    ).await.is_err());
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, before);
    assert_preserved(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_resolution_status_is_metadata_only_without_a_live_editor() {
    let rig = rig().await;
    let shown = prepare(&rig).await.unwrap();
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation: shown.generation, remote_revision: shown.remote_revision,
        remote_hash: rig.first_peer.content_hash.clone().unwrap(), remote_bytes: REMOTE.to_vec(),
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    let ack = rig.session.acknowledge(rig.operation.operation_seq).await.unwrap();
    let selected = {
        let mut state = rig.runtime.0.lock().await;
        state.store.record_content_conflict_intent(&rig.binding, &rig.document, &rig.operation.operation_id, &request).await.unwrap();
        state.store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
        state.store.close_registered_document(&rig.binding, &rig.document, &rig.edit_session).await.unwrap();
        state.gate.close_session(&rig.edit_session).unwrap();
        state.product_sessions.remove(&rig.edit_session);
        state.store.complete_content_conflict_choose_remote(&rig.binding, &rig.operation.operation_id).await.unwrap()
    };
    let response = status_json(&rig).await.unwrap();
    assert_eq!(response["binding"], serde_json::to_value(crate::to_contract(rig.binding.clone())).unwrap());
    assert_eq!(response["document_id"], rig.document);
    assert_eq!(response["operation_id"], rig.operation.operation_id);
    assert_eq!(response["state"], serde_json::json!({
        "status":"completed", "generation":selected.generation.to_string(), "frozen":false,
    }));
    let rendered = response.to_string();
    for source in ["server source", "local draft", "peer first", "base_bytes", "local_bytes", "remote_bytes"] {
        assert!(!rendered.contains(source), "metadata response leaked {source}");
    }
    let mut state = rig.runtime.0.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn delayed_status_rejects_owner_registration_or_durable_state_change() {
    for change in ["owner", "registration", "checkpoint"] {
        let rig = rig().await;
        let intent = seed_pending_status(&rig).await;
        let ready = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let pending = spawn_delayed_status(&rig, ready.clone(), resume.clone());
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified()).await.unwrap();
        match change {
            "owner" => {
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                crate::search_native::replace_renderer(&mut state, "new-renderer".into());
            }
            "registration" => {
                let pairing = server_test_support::pairing_request(&rig.config).await.0;
                let replacement = Arc::new(Session::pair(&format!("http://{}", rig.server.local_addr()),
                    pairing.workspace_id, &pairing.pairing_secret).await.unwrap());
                let binding = registration_binding(replacement.binding());
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                state.store.activate_registration(&binding).await.unwrap();
                state.session = Some(replacement);
            }
            "checkpoint" => {
                rig.runtime.0.lock().await.store.save_registered_checkpoint(
                    &rig.binding, &rig.document, &rig.edit_session, 2, b"new local\n",
                ).await.unwrap();
            }
            _ => unreachable!(),
        }
        resume.notify_one();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10), pending).await.unwrap().unwrap(), "{change}");
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(state.store.read_content_conflict_intent(&rig.binding, &rig.operation.operation_id).await.unwrap(), Some(intent));
        let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
        assert_eq!(draft.bytes, if change == "checkpoint" { b"new local\n".as_slice() } else { LOCAL });
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap().bytes, LOCAL);
        drop(state);
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
