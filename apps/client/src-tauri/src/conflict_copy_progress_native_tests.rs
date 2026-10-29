include!("conflict_copy_confirm_native_tests.rs");

fn progress_request(
    rig: &Rig,
    version: &str,
) -> notryx_protocol::WorkspaceContentConflictProgressRequest {
    notryx_protocol::WorkspaceContentConflictProgressRequest {
        renderer_session_id: OWNER.into(),
        binding: crate::to_contract(rig.binding.clone()),
        document_id: rig.document.clone(),
        edit_session_id: rig.edit_session.clone(),
        operation_id: rig.operation.operation_id.clone(),
        expected_intent_version: version.into(),
    }
}

async fn progress_copy(
    rig: &Rig,
    request: notryx_protocol::WorkspaceContentConflictProgressRequest,
) -> Result<serde_json::Value, String> {
    let version = request.expected_intent_version.parse::<u64>().unwrap();
    let approved = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .inspect_content_conflict_copy_followup(&rig.binding, &request.operation_id, version)
        .await
        .ok()
        .flatten()
        .map(|item| item.operation_id);
    let result = crate::conflict_snapshot_native::progress_copy_with_approval_for_renderer(
        &rig.runtime,
        notryx_protocol::WorkspaceContentConflictCopyProgressRequest {
            progress: request,
            approved_followup_operation_id: approved,
        },
    )
    .await?;
    Ok(serde_json::to_value(result).unwrap())
}

#[tokio::test]
async fn copy_progress_preserves_fifo_and_original_conflict_through_server_save() {
    let rig = rig_with_followup(true).await;
    let confirmation = confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    let request = progress_request(&rig, confirmation["version"].as_str().unwrap());
    let pending = progress_copy(&rig, request.clone()).await.unwrap();
    assert_eq!(pending["status"], "pending");
    let followup_id = pending["requested_operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        progress_copy(&rig, request.clone()).await.unwrap()["requested_operation_id"],
        followup_id
    );
    let mut state = rig.runtime.0.lock().await;
    let followup = state
        .store
        .read_outbox_operation(&rig.binding, &followup_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        followup.status,
        notryx_client_store::OutboxStatus::Abandoning
    );
    assert!(
        state
            .store
            .list_create_operations(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    drop(state);
    let abandoned = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        abandoned.status,
        notryx_protocol::WorkspaceSendStatus::Abandoned
    );
    assert_eq!(
        abandoned.operation_id.as_deref(),
        Some(followup_id.as_str())
    );
    let pending = progress_copy(&rig, request.clone()).await.unwrap();
    assert_eq!(pending["status"], "pending");
    assert!(pending["requested_operation_id"].is_null());
    let create = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries[0]
        .operation
        .clone();
    assert_eq!(create.operation_id, confirmation["create_operation_id"]);
    assert_eq!(
        progress_copy(&rig, request.clone()).await.unwrap()["status"],
        "pending"
    );
    let sent_create = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        sent_create.status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("my-copy.md")).unwrap(),
        b""
    );
    let pending = progress_copy(&rig, request.clone()).await.unwrap();
    assert_eq!(pending["status"], "pending");
    assert!(pending["requested_operation_id"].is_null());
    let node_id = confirmation["node_id"].as_str().unwrap();
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, b"");
    drop(state);
    let sent_content = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(
        sent_content.status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    assert_eq!(
        progress_copy(&rig, request).await.unwrap()["status"],
        "saved"
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("my-copy.md")).unwrap(),
        b"newer local\n"
    );
    let mut state = rig.runtime.0.lock().await;
    let original = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.bytes, b"newer local\n");
    assert_eq!(original.base_bytes, BASE);
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        notryx_client_store::OutboxStatus::Conflict
    );
    assert!(matches!(
        state
            .store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending {
            choice: notryx_client_store::ContentConflictChoiceKind::KeepLocalCopy,
            ..
        }
    ));
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_progress_rejects_missing_confirmation_stale_version_and_wrong_session() {
    let rig = rig_with_followup(true).await;
    let request = progress_request(&rig, "0");
    assert!(progress_copy(&rig, request.clone()).await.is_err());
    confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    let mut stale = request.clone();
    stale.expected_intent_version = "1".into();
    assert!(progress_copy(&rig, stale).await.is_err());
    let mut wrong = request;
    wrong.edit_session_id = Uuid::new_v4().to_string();
    assert!(progress_copy(&rig, wrong).await.is_err());
    assert_copy_confirm_preserves_sources(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_progress_reports_name_conflict_without_changing_existing_file_or_reallocating() {
    let rig = rig_with_followup(true).await;
    let mut choice = copy_request(&rig, None);
    choice.name = "example.md".into();
    let confirmation = confirm_copy(&rig, choice).await.unwrap();
    let request = progress_request(&rig, confirmation["version"].as_str().unwrap());
    assert_eq!(
        progress_copy(&rig, request.clone()).await.unwrap()["status"],
        "pending"
    );
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Abandoned
    );
    assert_eq!(
        progress_copy(&rig, request.clone()).await.unwrap()["status"],
        "pending"
    );
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Failed
    );
    assert_eq!(
        progress_copy(&rig, request).await.unwrap()["status"],
        "needs_review"
    );
    let mut state = rig.runtime.0.lock().await;
    let entries = state
        .store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].operation.operation_id,
        confirmation["create_operation_id"]
    );
    assert!(
        state
            .store
            .read_registered_checkpoint(&rig.binding, confirmation["node_id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
