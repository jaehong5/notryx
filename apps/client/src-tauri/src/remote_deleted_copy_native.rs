use crate::{
    ProductSessionKind, Runtime, from_contract, registration_binding, sync_bootstrap, sync_catchup,
};
use notryx_client_store::{
    ContentConflictCopyContentState, DeletedDraftCopyCreateState, DeletedDraftCopyEvidence,
    DeletedDraftCopyRequest, RemoteFileObservation,
};
use notryx_domain::{ChangeSeq, NodeId};
use notryx_protocol::{
    WorkspaceDeletedDraftCopyConfirmRequest, WorkspaceDeletedDraftCopyConfirmResult,
    WorkspaceDeletedDraftCopyConfirmStatus, WorkspaceDeletedDraftCopySnapshot,
    WorkspaceDeletedDraftCopySnapshotRequest, content_hash,
};
use tauri::Webview;

pub(crate) async fn reconfirm_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceDeletedDraftCopyReconfirmRequest,
) -> Result<WorkspaceDeletedDraftCopyConfirmResult, String> {
    let binding = from_contract(&request.binding);
    let (snapshot, session, response_generation) = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .check_queue(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        if state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&binding)
        {
            return Err("stale_registration".into());
        }
        let current = state
            .store
            .read_deleted_draft_copy(&binding, &request.document_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("missing_intent")?;
        if current.create_operation_id != request.expected_create_operation_id {
            let retry = state
                .store
                .reconfirm_failed_deleted_draft_copy(
                    &binding,
                    &request.document_id,
                    &request.expected_create_operation_id,
                    &request.parent_id,
                    &request.name,
                )
                .await
                .map_err(|error| error.to_string())?;
            return Ok(WorkspaceDeletedDraftCopyConfirmResult {
                status: WorkspaceDeletedDraftCopyConfirmStatus::Pending,
                node_id: retry.node_id,
                create_operation_id: retry.create_operation_id,
            });
        }
        let status = state
            .store
            .read_deleted_draft_copy_status(&binding, &request.document_id)
            .await
            .map_err(|error| error.to_string())?;
        if status.create
            != (DeletedDraftCopyCreateState::NeedsReview {
                destination_exists: true,
            })
        {
            return Err("stale_selection".into());
        }
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding {
            return Err("stale_registration".into());
        }
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        (snapshot, session, response_generation)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&snapshot),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
    state
        .gate
        .check_queue(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    let retry = state
        .store
        .reconfirm_failed_deleted_draft_copy(
            &binding,
            &request.document_id,
            &request.expected_create_operation_id,
            &request.parent_id,
            &request.name,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(WorkspaceDeletedDraftCopyConfirmResult {
        status: WorkspaceDeletedDraftCopyConfirmStatus::Pending,
        node_id: retry.node_id,
        create_operation_id: retry.create_operation_id,
    })
}

#[tauri::command]
pub(crate) async fn workspace_reconfirm_deleted_draft_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceDeletedDraftCopyReconfirmRequest,
) -> Result<WorkspaceDeletedDraftCopyConfirmResult, String> {
    crate::check_caller(&webview)?;
    let result = reconfirm_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

pub(crate) async fn request_followup_abandon_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceDeletedDraftCopyFollowupAbandonRequest,
) -> Result<notryx_protocol::WorkspaceDeletedDraftCopyFollowupAbandonResult, String> {
    let sequence = request
        .followup_operation_seq
        .parse::<u64>()
        .map_err(|_| "invalid_sequence")?;
    let generation = request
        .followup_generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    if sequence == 0 || generation == 0 {
        return Err("invalid_request".into());
    }
    let binding = from_contract(&request.binding);
    let mut state = runtime.0.lock().await;
    state
        .gate
        .check_queue(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(&binding)
    {
        return Err("stale_registration".into());
    }
    let intent = state
        .store
        .read_deleted_draft_copy(&binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_intent")?;
    if intent.node_id != request.node_id
        || intent.create_operation_id != request.create_operation_id
    {
        return Err("stale_selection".into());
    }
    let operation = state
        .store
        .request_deleted_draft_copy_followup_abandon(
            &binding,
            &request.document_id,
            &request.blocker_operation_id,
            &request.followup_operation_id,
            sequence,
            generation,
            &request.followup_hash,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(
        notryx_protocol::WorkspaceDeletedDraftCopyFollowupAbandonResult {
            operation_id: operation.operation_id,
        },
    )
}

#[tauri::command]
pub(crate) async fn workspace_request_deleted_draft_copy_followup_abandon(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceDeletedDraftCopyFollowupAbandonRequest,
) -> Result<notryx_protocol::WorkspaceDeletedDraftCopyFollowupAbandonResult, String> {
    crate::check_caller(&webview)?;
    let result = request_followup_abandon_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

pub(crate) async fn reopen_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceDeletedDraftCopyReopenRequest,
) -> Result<notryx_protocol::WorkspaceDeletedDraftCopyReopenResponse, String> {
    request
        .document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    let binding = from_contract(&request.binding);
    let mut state = runtime.0.lock().await;
    state
        .gate
        .check_queue(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(&binding)
    {
        return Err("stale_registration".into());
    }
    let status_request = notryx_protocol::WorkspaceDeletedDraftCopyStatusRequest {
        renderer_session_id: request.renderer_session_id,
        binding: request.binding,
        document_id: request.document_id.clone(),
    };
    let status = status_for_state(&mut state, &binding, status_request).await?;
    if state
        .store
        .read_registered_checkpoint(&binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Err("missing_draft".into());
    }
    if let Some(previous) = state
        .product_sessions
        .get(&request.previous_edit_session_id)
        .cloned()
    {
        if previous.binding != binding
            || previous.document_id != request.document_id
            || previous.kind != ProductSessionKind::Active
        {
            return Err("stale_session".into());
        }
        state
            .gate
            .check_session(&request.previous_edit_session_id)
            .map_err(str::to_owned)?;
        state
            .close_local_document(&notryx_protocol::M0CloseRequest {
                document_id: request.document_id.clone(),
                edit_session_id: request.previous_edit_session_id.clone(),
            })
            .await?;
    } else if state
        .gate
        .check_session(&request.previous_edit_session_id)
        .is_ok()
        || state
            .product_sessions
            .values()
            .any(|other| other.binding == binding && other.document_id == request.document_id)
    {
        return Err("stale_session".into());
    }
    let document = state
        .try_open_local_draft(&binding, &request.document_id)
        .await?
        .ok_or("missing_draft")?;
    Ok(notryx_protocol::WorkspaceDeletedDraftCopyReopenResponse {
        state: status,
        document,
    })
}

#[tauri::command]
pub(crate) async fn workspace_reopen_deleted_draft_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceDeletedDraftCopyReopenRequest,
) -> Result<notryx_protocol::WorkspaceDeletedDraftCopyReopenResponse, String> {
    crate::check_caller(&webview)?;
    let result = reopen_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_prepare_deleted_draft_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDeletedDraftCopySnapshotRequest,
) -> Result<WorkspaceDeletedDraftCopySnapshot, String> {
    crate::check_caller(&webview)?;
    let result = prepare_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_confirm_deleted_draft_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDeletedDraftCopyConfirmRequest,
) -> Result<WorkspaceDeletedDraftCopyConfirmResult, String> {
    crate::check_caller(&webview)?;
    let result = confirm_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_deleted_draft_copy_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceDeletedDraftCopyStatusRequest,
) -> Result<Option<notryx_protocol::WorkspaceDeletedDraftCopyStatus>, String> {
    crate::check_caller(&webview)?;
    let result = status_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_progress_deleted_draft_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceDeletedDraftCopyProgressRequest,
) -> Result<notryx_protocol::WorkspaceDeletedDraftCopyProgressResult, String> {
    crate::check_caller(&webview)?;
    let result = progress_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

pub(crate) async fn status_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceDeletedDraftCopyStatusRequest,
) -> Result<Option<notryx_protocol::WorkspaceDeletedDraftCopyStatus>, String> {
    let binding = from_contract(&request.binding);
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(&binding)
    {
        return Err("stale_registration".into());
    }
    status_for_state(&mut state, &binding, request).await
}

async fn status_for_state(
    state: &mut crate::RuntimeState,
    binding: &notryx_client_store::RegistrationBinding,
    request: notryx_protocol::WorkspaceDeletedDraftCopyStatusRequest,
) -> Result<Option<notryx_protocol::WorkspaceDeletedDraftCopyStatus>, String> {
    use notryx_protocol::{
        WorkspaceContentConflictCopyContentStatus as ContentStatus,
        WorkspaceContentConflictCopyCreateFailure as CreateFailure,
        WorkspaceContentConflictCopyCreateStatus as CreateStatus,
    };
    let Some(intent) = state
        .store
        .read_deleted_draft_copy(binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let status = state
        .store
        .read_deleted_draft_copy_status(binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?;
    let blocking_followup = state
        .store
        .read_deleted_draft_copy_blocking_followup(binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?
        .map(
            |item| notryx_protocol::WorkspaceDeletedDraftCopyBlockingFollowup {
                blocker_operation_id: item.blocker_operation_id,
                followup_operation_id: item.followup_operation_id,
                followup_operation_seq: item.followup_operation_seq.to_string(),
                followup_generation: item.followup_generation.to_string(),
                followup_hash: item.followup_hash,
            },
        );
    let (create_status, create_failure) = match status.create {
        DeletedDraftCopyCreateState::NotQueued => (CreateStatus::NotQueued, None),
        DeletedDraftCopyCreateState::Pending => (CreateStatus::Pending, None),
        DeletedDraftCopyCreateState::Created => (CreateStatus::Created, None),
        DeletedDraftCopyCreateState::NeedsReview { destination_exists } => (
            CreateStatus::NeedsReview,
            Some(if destination_exists {
                CreateFailure::DestinationExists
            } else {
                CreateFailure::Other
            }),
        ),
    };
    let (content_status, content_operation_id) = match status.content {
        ContentConflictCopyContentState::NotQueued => (ContentStatus::NotQueued, None),
        ContentConflictCopyContentState::Pending { operation_id } => {
            (ContentStatus::Pending, Some(operation_id))
        }
        ContentConflictCopyContentState::Saved { operation_id } => {
            (ContentStatus::Saved, Some(operation_id))
        }
        ContentConflictCopyContentState::NeedsReview { operation_id } => {
            (ContentStatus::NeedsReview, Some(operation_id))
        }
    };
    let (tombstone_seq, terminal_operation_id) = match intent.evidence {
        DeletedDraftCopyEvidence::Tombstone { last_change_seq } => {
            (Some(last_change_seq.get().to_string()), None)
        }
        DeletedDraftCopyEvidence::NodeDeleted { operation_id } => (None, Some(operation_id)),
    };
    Ok(Some(notryx_protocol::WorkspaceDeletedDraftCopyStatus {
        binding: request.binding,
        document_id: request.document_id,
        generation: intent.generation.to_string(),
        local_hash: content_hash(&intent.local_bytes),
        tombstone_seq,
        terminal_operation_id,
        parent_id: intent.parent_id,
        name: intent.name,
        node_id: intent.node_id,
        create_operation_id: intent.create_operation_id,
        create_status,
        create_failure,
        content_status,
        content_operation_id,
        blocking_followup,
    }))
}

pub(crate) async fn progress_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceDeletedDraftCopyProgressRequest,
) -> Result<notryx_protocol::WorkspaceDeletedDraftCopyProgressResult, String> {
    let binding = from_contract(&request.binding);
    let mut state = runtime.0.lock().await;
    state
        .gate
        .check_queue(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(&binding)
    {
        return Err("stale_registration".into());
    }
    let intent = state
        .store
        .read_deleted_draft_copy(&binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_intent")?;
    if intent.node_id != request.node_id
        || intent.create_operation_id != request.create_operation_id
    {
        return Err("stale_selection".into());
    }
    let status = state
        .store
        .read_deleted_draft_copy_status(&binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?;
    let operation_id = match status.create {
        DeletedDraftCopyCreateState::NotQueued | DeletedDraftCopyCreateState::Pending => {
            state
                .store
                .queue_deleted_draft_copy(&binding, &request.document_id)
                .await
                .map_err(|error| error.to_string())?
                .operation_id
        }
        DeletedDraftCopyCreateState::Created => match status.content {
            ContentConflictCopyContentState::NotQueued
            | ContentConflictCopyContentState::Pending { .. } => {
                state
                    .store
                    .queue_deleted_draft_copy_content(&binding, &request.document_id)
                    .await
                    .map_err(|error| error.to_string())?
                    .operation_id
            }
            ContentConflictCopyContentState::Saved { operation_id } => operation_id,
            ContentConflictCopyContentState::NeedsReview { .. } => {
                return Err("needs_review".into());
            }
        },
        DeletedDraftCopyCreateState::NeedsReview { .. } => return Err("needs_review".into()),
    };
    Ok(notryx_protocol::WorkspaceDeletedDraftCopyProgressResult { operation_id })
}

pub(crate) async fn prepare_for_renderer(
    runtime: &Runtime,
    request: WorkspaceDeletedDraftCopySnapshotRequest,
) -> Result<WorkspaceDeletedDraftCopySnapshot, String> {
    let document_id = request
        .document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    if generation == 0 {
        return Err("invalid_generation".into());
    }
    let binding = from_contract(&request.binding);
    let (snapshot, session, response_generation, before) = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .check_queue(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding
            || state
                .store
                .current_registration()
                .await
                .map_err(|error| error.to_string())?
                .as_ref()
                != Some(&binding)
        {
            return Err("stale_registration".into());
        }
        if !state
            .product_sessions
            .get(&request.edit_session_id)
            .is_some_and(|edit| {
                edit.document_id == request.document_id
                    && edit.binding == binding
                    && edit.kind == ProductSessionKind::Active
            })
            || state
                .store
                .dirty_registered_session_snapshot(
                    &binding,
                    &request.document_id,
                    &request.edit_session_id,
                )
                .await
                .map_err(|error| error.to_string())?
                .is_none()
        {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &request.document_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("missing_draft")?;
        if draft.generation != generation {
            return Err("stale_checkpoint".into());
        }
        (snapshot, session, response_generation, draft)
    };

    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&snapshot),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
    state
        .gate
        .check_queue(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    state
        .gate
        .check_session(&request.edit_session_id)
        .map_err(str::to_owned)?;
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    if !state
        .product_sessions
        .get(&request.edit_session_id)
        .is_some_and(|edit| {
            edit.document_id == request.document_id
                && edit.binding == binding
                && edit.kind == ProductSessionKind::Active
        })
        || state
            .store
            .dirty_registered_session_snapshot(
                &binding,
                &request.document_id,
                &request.edit_session_id,
            )
            .await
            .map_err(|error| error.to_string())?
            .is_none()
    {
        return Err("stale_session".into());
    }
    let draft = state
        .store
        .read_registered_checkpoint(&binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_draft")?;
    if draft != before {
        return Err("stale_checkpoint".into());
    }
    let tombstone_seq = match state
        .store
        .observe_remote_file(&binding, document_id)
        .await
        .map_err(|error| error.to_string())?
    {
        RemoteFileObservation::Deleted(node) if node.last_change_seq.get() > 0 => {
            node.last_change_seq
        }
        _ => return Err("stale_tombstone".into()),
    };
    Ok(WorkspaceDeletedDraftCopySnapshot {
        binding: request.binding,
        document_id: request.document_id,
        edit_session_id: request.edit_session_id,
        generation: request.generation,
        local_hash: content_hash(&draft.bytes),
        tombstone_seq: tombstone_seq.get().to_string(),
    })
}

pub(crate) async fn confirm_for_renderer(
    runtime: &Runtime,
    request: WorkspaceDeletedDraftCopyConfirmRequest,
) -> Result<WorkspaceDeletedDraftCopyConfirmResult, String> {
    let document_id = request
        .document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    if generation == 0 {
        return Err("invalid_generation".into());
    }
    let tombstone_seq =
        ChangeSeq::try_from(request.tombstone_seq.clone()).map_err(|_| "invalid_tombstone")?;
    if tombstone_seq.get() == 0 {
        return Err("invalid_tombstone".into());
    }
    let requested_binding = from_contract(&request.binding);
    let (snapshot, session, response_generation) = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .check_queue(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != requested_binding
            || state
                .store
                .current_registration()
                .await
                .map_err(|error| error.to_string())?
                .as_ref()
                != Some(&requested_binding)
        {
            return Err("stale_registration".into());
        }
        if !state
            .product_sessions
            .get(&request.edit_session_id)
            .is_some_and(|edit| {
                edit.document_id == request.document_id
                    && edit.binding == requested_binding
                    && edit.kind == ProductSessionKind::Active
            })
        {
            return Err("stale_session".into());
        }
        if state
            .store
            .dirty_registered_session_snapshot(
                &requested_binding,
                &request.document_id,
                &request.edit_session_id,
            )
            .await
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err("stale_session".into());
        }
        if let Some(intent) = state
            .store
            .read_deleted_draft_copy(&requested_binding, &request.document_id)
            .await
            .map_err(|error| error.to_string())?
        {
            if intent.generation != generation
                || content_hash(&intent.local_bytes) != request.local_hash
                || intent.parent_id != request.parent_id
                || intent.name != request.name
                || intent.evidence
                    != (DeletedDraftCopyEvidence::Tombstone {
                        last_change_seq: tombstone_seq,
                    })
            {
                return Err("stale_selection".into());
            }
            return Ok(WorkspaceDeletedDraftCopyConfirmResult {
                status: WorkspaceDeletedDraftCopyConfirmStatus::Pending,
                node_id: intent.node_id,
                create_operation_id: intent.create_operation_id,
            });
        }
        let response_generation = state
            .transmission_gate
            .response_generation(&requested_binding);
        if !state
            .transmission_gate
            .response_trusted(&requested_binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let draft = state
            .store
            .read_registered_checkpoint(&requested_binding, &request.document_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("missing_draft")?;
        if draft.generation != generation || content_hash(&draft.bytes) != request.local_hash {
            return Err("stale_checkpoint".into());
        }
        (snapshot, session, response_generation)
    };

    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&snapshot),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
    state
        .gate
        .check_queue(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    state
        .gate
        .check_session(&request.edit_session_id)
        .map_err(str::to_owned)?;
    if !state
        .transmission_gate
        .response_trusted(&requested_binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    if !state
        .product_sessions
        .get(&request.edit_session_id)
        .is_some_and(|edit| {
            edit.document_id == request.document_id
                && edit.binding == requested_binding
                && edit.kind == ProductSessionKind::Active
        })
    {
        return Err("stale_session".into());
    }
    let draft = state
        .store
        .read_registered_checkpoint(&requested_binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_draft")?;
    if draft.generation != generation || content_hash(&draft.bytes) != request.local_hash {
        return Err("stale_checkpoint".into());
    }
    match state
        .store
        .observe_remote_file(&requested_binding, document_id)
        .await
        .map_err(|error| error.to_string())?
    {
        RemoteFileObservation::Deleted(node) if node.last_change_seq == tombstone_seq => {}
        _ => return Err("stale_tombstone".into()),
    }
    let intent = state
        .store
        .record_deleted_draft_copy(
            &requested_binding,
            &request.document_id,
            &DeletedDraftCopyRequest {
                edit_session_id: request.edit_session_id,
                generation,
                local_bytes: draft.bytes,
                parent_id: request.parent_id,
                name: request.name,
                evidence: DeletedDraftCopyEvidence::Tombstone {
                    last_change_seq: tombstone_seq,
                },
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(WorkspaceDeletedDraftCopyConfirmResult {
        status: WorkspaceDeletedDraftCopyConfirmStatus::Pending,
        node_id: intent.node_id,
        create_operation_id: intent.create_operation_id,
    })
}

#[cfg(test)]
#[path = "remote_deleted_copy_confirm_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "remote_deleted_copy_snapshot_tests.rs"]
mod snapshot_tests;

#[cfg(test)]
#[path = "deleted_draft_copy_status_progress_tests.rs"]
mod status_progress_tests;

#[cfg(test)]
#[path = "remote_deleted_copy_reopen_tests.rs"]
mod reopen_tests;

#[cfg(test)]
#[path = "deleted_draft_copy_followup_native_tests.rs"]
mod followup_tests;

#[cfg(test)]
#[path = "deleted_copy_reconfirm_native_tests.rs"]
mod reconfirm_tests;

#[cfg(test)]
#[path = "deleted_copy_reconfirm_race_tests.rs"]
mod reconfirm_race_tests;

#[cfg(test)]
#[path = "deleted_copy_later_followup_native_tests.rs"]
mod later_followup_tests;
