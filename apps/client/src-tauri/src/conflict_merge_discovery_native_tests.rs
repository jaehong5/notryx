use crate::conflict_snapshot_native::test_support::merge_queue::*;

#[derive(Debug, PartialEq, Eq)]
struct PendingMergeDiscovery {
    conflict_operation_id: String,
    conflict_operation_seq: u64,
    intent_version: u64,
    merge_operation_id: Option<String>,
    merge_operation_seq: Option<u64>,
}

async fn discover_pending_merge(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
) -> Result<Option<PendingMergeDiscovery>, String> {
    let response = {
        let mut state = runtime.0.lock().await;
        state.gate.snapshot(owner).map_err(str::to_owned)?;
        state.document_state(binding, document_id).await?
    };
    let raw = serde_json::to_value(response).map_err(|error| error.to_string())?;
    let first = raw
        .get("first_content_conflict")
        .ok_or("missing_first_content_conflict")?;
    if first.is_null() {
        return Ok(None);
    }
    let operation = first
        .get("operation_id")
        .and_then(|value| value.as_str())
        .ok_or("invalid_conflict")?;
    let sequence = first
        .get("operation_seq")
        .and_then(|value| value.as_str())
        .ok_or("invalid_conflict")?
        .parse::<u64>()
        .map_err(|_| "invalid_conflict")?;
    if sequence == 0 {
        return Err("invalid_conflict".into());
    }
    let resolution = crate::conflict_snapshot_native::read_resolution_for_renderer(
        runtime,
        owner,
        binding,
        document_id,
        operation,
    )
    .await?;
    let notryx_protocol::WorkspaceConflictResolutionState::Pending {
        version,
        choice: notryx_protocol::WorkspaceConflictChoice::SaveMerged,
        frozen: false,
        ..
    } = resolution.state
    else {
        return Ok(None);
    };
    let version = version.parse::<u64>().map_err(|_| "invalid_version")?;
    let merged = crate::conflict_snapshot_native::read_queued_merge_for_renderer(
        runtime,
        owner,
        binding,
        document_id,
        operation,
        version,
    )
    .await?;
    Ok(Some(PendingMergeDiscovery {
        conflict_operation_id: operation.to_owned(),
        conflict_operation_seq: sequence,
        intent_version: version,
        merge_operation_id: merged.as_ref().map(|item| item.operation_id.clone()),
        merge_operation_seq: merged.map(|item| item.operation_seq),
    }))
}

async fn reopen_runtime(rig: Rig) -> Rig {
    let Rig {
        _directory,
        server,
        runtime,
        session,
        peer,
        binding,
        document,
        edit_session,
        initial,
        first_peer,
        operation,
        armed,
        captured,
        release,
        proxy,
        config,
    } = rig;
    drop(runtime);
    let store = ClientStore::open(_directory.path().join("conflict-snapshot.sqlite"))
        .await
        .unwrap();
    let mut state = RuntimeState::new(store, OWNER.into());
    state.session = Some(session.clone());
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    Rig {
        _directory,
        server,
        runtime,
        session,
        peer,
        binding,
        document,
        edit_session,
        initial,
        first_peer,
        operation,
        armed,
        captured,
        release,
        proxy,
        config,
    }
}

#[tokio::test]
async fn merge_discovery_after_runtime_restart_finds_original_intent_and_same_operation() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let queued = queue_approved_merge(&rig, intent.version).await.unwrap();
    let rig = reopen_runtime(rig).await;
    let found = discover_pending_merge(&rig.runtime, OWNER, &rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.conflict_operation_id, rig.operation.operation_id);
    assert_eq!(found.conflict_operation_seq, rig.operation.operation_seq);
    assert_eq!(found.intent_version, intent.version);
    assert_eq!(
        found.merge_operation_id.as_deref(),
        Some(queued.operation_id.as_str())
    );
    assert_eq!(found.merge_operation_seq, Some(queued.operation_seq));
    let status = read_queued_merge_for_discovery(&rig, &found)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.operation_id, queued.operation_id);
    assert_eq!(status.bytes, MERGED);
    let (resolution, opened) =
        crate::conflict_snapshot_native::reopen_conflict_checkpoint_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &rig.edit_session,
        )
        .await
        .unwrap();
    assert!(matches!(
        resolution,
        ContentConflictResolutionState::Pending { .. }
    ));
    assert_eq!(opened.generation, queued.generation.to_string());
    assert_ne!(opened.edit_session_id, rig.edit_session);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, MERGED);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &queued.operation_id)
            .await
            .unwrap(),
        Some(queued.clone())
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(queued.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        MERGED
    );
    assert!(
        discover_pending_merge(&rig.runtime, OWNER, &rig.binding, &rig.document)
            .await
            .unwrap()
            .is_none()
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

async fn read_queued_merge_for_discovery(
    rig: &Rig,
    found: &PendingMergeDiscovery,
) -> Result<Option<OutboxOperation>, String> {
    crate::conflict_snapshot_native::read_queued_merge_for_renderer(
        &rig.runtime,
        OWNER,
        &rig.binding,
        &rig.document,
        &found.conflict_operation_id,
        found.intent_version,
    )
    .await
}

#[tokio::test]
async fn merge_discovery_before_queue_returns_intent_without_creating_operation() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    let found = discover_pending_merge(&rig.runtime, OWNER, &rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.conflict_operation_id, rig.operation.operation_id);
    assert_eq!(found.conflict_operation_seq, rig.operation.operation_seq);
    assert_eq!(found.intent_version, intent.version);
    assert_eq!(found.merge_operation_id, None);
    assert_eq!(found.merge_operation_seq, None);
    assert_merge_queue_sources(&rig).await;
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .product_sessions
            .contains_key(&rig.edit_session)
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_discovery_rejects_old_owner_and_mismatched_registration() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let queued = queue_approved_merge(&rig, intent.version).await.unwrap();
    let rig = reopen_runtime(rig).await;
    let mut wrong = rig.binding.clone();
    wrong.device_stream_id = Uuid::new_v4().to_string();
    assert!(
        discover_pending_merge(&rig.runtime, "other-owner", &rig.binding, &rig.document)
            .await
            .is_err()
    );
    assert!(
        discover_pending_merge(&rig.runtime, OWNER, &wrong, &rig.document)
            .await
            .is_err()
    );
    let found = discover_pending_merge(&rig.runtime, OWNER, &rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        found.merge_operation_id.as_deref(),
        Some(queued.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_discovery_frozen_registration_reports_no_active_reference_but_keeps_conflict() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let queued = queue_approved_merge(&rig, intent.version).await.unwrap();
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
    let replacement_binding = registration_binding(replacement.binding());
    let mut state = rig.runtime.0.lock().await;
    state
        .store
        .activate_registration(&replacement_binding)
        .await
        .unwrap();
    state.session = Some(replacement);
    let response = state
        .document_state(&rig.binding, &rig.document)
        .await
        .unwrap();
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(
        json.get("first_content_conflict"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        json.get("has_conflict"),
        Some(&serde_json::Value::Bool(true))
    );
    let original = state
        .store
        .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(original.frozen);
    assert_eq!(original.bytes, LOCAL);
    let saved = state
        .store
        .read_outbox_operation(&rig.binding, &queued.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(saved.frozen);
    assert_eq!(saved.bytes, MERGED);
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, MERGED);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
