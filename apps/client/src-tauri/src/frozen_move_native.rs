use crate::{
    Runtime, RuntimeState, check_caller, from_contract, registration_binding, to_contract,
};
use notryx_client_store::{FrozenMoveChoice, OutboxStatus, RegistrationBinding};
use notryx_protocol::{
    WorkspaceFrozenMoveChoice, WorkspaceFrozenMoveDecisionRequest, WorkspaceFrozenMoveEntry,
    WorkspaceFrozenMoveListRequest, WorkspaceFrozenMovePage, WorkspaceMoveStatus,
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn list_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: Option<&str>,
    limit: u16,
) -> Result<WorkspaceFrozenMovePage, String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    validate_binding(&mut state, binding).await?;
    let (_, nodes) = state
        .store
        .sync_index(binding)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("not_connected")?;
    let page = state
        .store
        .list_frozen_moves(binding, after, limit)
        .await
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::with_capacity(page.entries.len());
    for item in page.entries {
        let operation = item.operation;
        let status = match operation.status {
            OutboxStatus::Queued => WorkspaceMoveStatus::Queued,
            OutboxStatus::Sent => WorkspaceMoveStatus::Sent,
            OutboxStatus::Abandoning => WorkspaceMoveStatus::Abandoning,
            OutboxStatus::Failed => WorkspaceMoveStatus::Failed,
            _ => return Err("UnsupportedFormat".into()),
        };
        let current_node = nodes
            .iter()
            .find(|node| node.node_id == operation.node_id)
            .cloned();
        let current_destination = nodes
            .iter()
            .find(|node| node.node_id == operation.request.parent_id)
            .cloned();
        entries.push(WorkspaceFrozenMoveEntry {
            old_binding: to_contract(operation.binding),
            operation_id: operation.operation_id,
            operation_seq: operation.operation_seq.to_string(),
            node_id: operation.node_id,
            expected_meta_etag: operation.expected_meta_etag,
            request: operation.request,
            status,
            receipt: item.receipt,
            current_node,
            current_destination,
        });
    }
    Ok(WorkspaceFrozenMovePage {
        binding: to_contract(binding.clone()),
        entries,
        next_after_id: page.next_after_id,
    })
}

pub(crate) async fn decide_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    operation_id: &str,
    choice: &WorkspaceFrozenMoveChoice,
) -> Result<(), String> {
    let mut state = runtime.lock().await;
    state.gate.check_queue(owner).map_err(str::to_owned)?;
    validate_binding(&mut state, current).await?;
    let choice = match choice {
        WorkspaceFrozenMoveChoice::Apply {
            expected_meta_etag,
            request,
        } => FrozenMoveChoice::Apply {
            expected_meta_etag: expected_meta_etag.clone(),
            request: request.clone(),
        },
        WorkspaceFrozenMoveChoice::Dismiss => FrozenMoveChoice::Dismiss,
    };
    state
        .store
        .resolve_frozen_move(old, current, operation_id, &choice)
        .await
        .map_err(|error| error.to_string())
}

async fn validate_binding(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
) -> Result<(), String> {
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(binding)
        || state
            .session
            .as_ref()
            .is_some_and(|session| registration_binding(session.binding()) != *binding)
    {
        return Err("stale_registration".into());
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn workspace_frozen_moves(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenMoveListRequest,
) -> Result<WorkspaceFrozenMovePage, String> {
    check_caller(&webview)?;
    let page = list_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &from_contract(&request.binding),
        request.after_id.as_deref(),
        request.limit,
    )
    .await?;
    check_caller(&webview)?;
    Ok(page)
}

#[tauri::command]
pub(crate) async fn workspace_resolve_frozen_move(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenMoveDecisionRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    decide_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &from_contract(&request.binding),
        &from_contract(&request.old_binding),
        &request.operation_id,
        &request.choice,
    )
    .await
}

#[cfg(test)]
#[path = "frozen_move_native_tests.rs"]
mod tests;
