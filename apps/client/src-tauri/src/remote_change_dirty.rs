use crate::{
    ProductSessionKind, Runtime, check_caller, registration_binding, sync_bootstrap, sync_catchup,
};
use notryx_client_runtime::Error as NetworkError;
use notryx_client_runtime::Session;
use notryx_client_store::{RegistrationBinding, RemoteFileObservation};
use notryx_domain::NodeId;
use notryx_protocol::{
    WorkspaceDirtyRemoteRequest, WorkspaceDirtyRemoteState, WorkspaceSyncState, WorkspaceSyncStatus,
};
use std::sync::Arc;
use tauri::{Emitter, Webview};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DirtyRemoteState {
    Pending,
    Unchanged,
    Changed,
    Deleted,
}

#[cfg(test)]
#[path = "remote_delete_sent_observation_tests.rs"]
mod sent_observation_tests;

#[cfg(test)]
#[path = "remote_delete_sent_restore_tests.rs"]
mod sent_restore_tests;

#[cfg(test)]
#[path = "remote_delete_sent_race_tests.rs"]
mod sent_race_tests;

fn sync_observation(state: &crate::RuntimeState) -> (Option<WorkspaceSyncState>, Option<u64>) {
    let generation = state.session.as_ref().map(|session| {
        state
            .transmission_gate
            .response_generation(&registration_binding(session.binding()))
    });
    (state.sync_state.clone(), generation)
}

#[cfg(test)]
pub(crate) async fn inspect_dirty_remote(
    runtime: &Runtime,
    renderer_owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
) -> Result<DirtyRemoteState, String> {
    inspect_dirty_remote_with_progress(
        runtime,
        renderer_owner,
        document_id,
        edit_session_id,
        generation,
        &mut None,
    )
    .await
}

struct MetadataAdvance {
    snapshot: crate::gate::RequestSnapshot,
    session: Arc<Session>,
    binding: RegistrationBinding,
}

struct InspectionAttempt<'a> {
    progress: &'a mut Option<MetadataAdvance>,
    snapshot_changed: bool,
}

async fn inspect_dirty_remote_with_progress(
    runtime: &Runtime,
    renderer_owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    progress: &mut Option<MetadataAdvance>,
) -> Result<DirtyRemoteState, String> {
    for _ in 0..2 {
        let mut attempt = InspectionAttempt {
            progress: &mut *progress,
            snapshot_changed: false,
        };
        let result = inspect_dirty_remote_once(
            runtime,
            renderer_owner,
            document_id,
            edit_session_id,
            generation,
            &mut attempt,
        )
        .await?;
        if !attempt.snapshot_changed {
            return Ok(result);
        }
    }
    Ok(DirtyRemoteState::Pending)
}

async fn inspect_dirty_remote_once(
    runtime: &Runtime,
    renderer_owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    attempt: &mut InspectionAttempt<'_>,
) -> Result<DirtyRemoteState, String> {
    let node_id = document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    let (snapshot, session, binding, response_generation, before, initial_cursor) = {
        let mut state = runtime.0.lock().await;
        let snapshot = state.gate.snapshot(renderer_owner).map_err(str::to_owned)?;
        state
            .gate
            .check_session(edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        let binding = registration_binding(session.binding());
        let valid_session = state
            .product_sessions
            .get(edit_session_id)
            .is_some_and(|edit| {
                edit.document_id == document_id
                    && edit.binding == binding
                    && edit.kind == ProductSessionKind::Active
            });
        if !valid_session {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Ok(DirtyRemoteState::Pending);
        }
        let Some(before) = state
            .store
            .dirty_registered_session_snapshot(&binding, document_id, edit_session_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Err("stale_session".into());
        };
        if before
            .checkpoint_generation
            .is_some_and(|saved| saved > generation)
        {
            return Ok(DirtyRemoteState::Pending);
        }
        let initial_cursor = state
            .store
            .sync_index(&binding)
            .await
            .ok()
            .flatten()
            .map(|(cursor, _)| cursor);
        (
            snapshot,
            session,
            binding,
            response_generation,
            before,
            initial_cursor,
        )
    };

    let mut attempts = 0;
    let caught_up = loop {
        attempts += 1;
        match sync_catchup::catch_up_for_generation(
            runtime,
            Some(&snapshot),
            &session,
            Some(response_generation),
        )
        .await
        {
            Err(sync_catchup::CatchUpError::StaleSync) if attempts < 3 => continue,
            result => break result,
        }
    };
    let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    let current_cursor = state
        .store
        .sync_index(&binding)
        .await
        .ok()
        .flatten()
        .map(|(cursor, _)| cursor);
    if initial_cursor
        .zip(current_cursor)
        .is_some_and(|(initial, current)| current > initial)
    {
        *attempt.progress = Some(MetadataAdvance {
            snapshot,
            session,
            binding: binding.clone(),
        });
    }
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
    {
        return Ok(DirtyRemoteState::Pending);
    }
    let valid_session = state
        .product_sessions
        .get(edit_session_id)
        .is_some_and(|edit| {
            edit.document_id == document_id
                && edit.binding == binding
                && edit.kind == ProductSessionKind::Active
        });
    if !valid_session {
        return Err("stale_session".into());
    }
    if let Err(error) = &caught_up {
        use sync_catchup::CatchUpError;
        let status = match error {
            CatchUpError::Network(NetworkError::Unauthorized, _) => {
                state
                    .transmission_gate
                    .block(&binding, crate::send::TransmissionBlock::AuthRequired);
                Some(WorkspaceSyncStatus::AuthRequired)
            }
            CatchUpError::Network(NetworkError::GatewayAuthRequired, _) => {
                Some(WorkspaceSyncStatus::GatewayAuthRequired)
            }
            CatchUpError::Network(
                NetworkError::Network | NetworkError::Unavailable | NetworkError::Response,
                _,
            ) => Some(WorkspaceSyncStatus::Offline),
            CatchUpError::Network(_, _)
            | CatchUpError::Store(_)
            | CatchUpError::BootstrapRequired
            | CatchUpError::HistoryRegression
            | CatchUpError::ResyncRequired
            | CatchUpError::BindingMismatch
            | CatchUpError::ProtocolMismatch => Some(WorkspaceSyncStatus::NeedsReview),
            CatchUpError::Guard(error) => return Err(error.clone()),
            CatchUpError::ServerNotReady(_) => Some(WorkspaceSyncStatus::Offline),
            CatchUpError::StaleSync => None,
        };
        if let Some(status) = status {
            state.sync_state = Some(WorkspaceSyncState {
                binding: crate::to_contract(binding.clone()),
                status,
            });
        }
        return Ok(DirtyRemoteState::Pending);
    }
    let Some(after) = state
        .store
        .dirty_registered_session_snapshot(&binding, document_id, edit_session_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Err("stale_session".into());
    };
    if after
        .checkpoint_generation
        .is_some_and(|saved| saved > generation)
    {
        return Ok(DirtyRemoteState::Pending);
    }
    if after != before {
        attempt.snapshot_changed = true;
        return Ok(DirtyRemoteState::Pending);
    }
    let observation = state
        .store
        .observe_remote_file(&binding, node_id)
        .await
        .map_err(|error| error.to_string())?;
    let node = match observation {
        RemoteFileObservation::Live(_) if after.has_sent => return Ok(DirtyRemoteState::Pending),
        RemoteFileObservation::Live(node) => node,
        RemoteFileObservation::Deleted(_) => return Ok(DirtyRemoteState::Deleted),
        RemoteFileObservation::Missing | RemoteFileObservation::Pending => {
            return Ok(DirtyRemoteState::Pending);
        }
    };
    let Some(revision) = node.content_rev.as_ref() else {
        return Ok(DirtyRemoteState::Pending);
    };
    Ok(if revision.as_str() == after.base_revision {
        DirtyRemoteState::Unchanged
    } else {
        DirtyRemoteState::Changed
    })
}

pub(crate) async fn inspect_dirty_remote_notifying(
    runtime: &Runtime,
    request: &WorkspaceDirtyRemoteRequest,
    generation: u64,
    verify_caller: impl FnOnce() -> Result<(), String>,
    mut notify: impl FnMut(&str),
) -> Result<DirtyRemoteState, String> {
    let before = {
        let state = runtime.0.lock().await;
        sync_observation(&state)
    };
    let mut progress = None;
    let result = inspect_dirty_remote_with_progress(
        runtime,
        &request.renderer_session_id,
        &request.document_id,
        &request.edit_session_id,
        generation,
        &mut progress,
    )
    .await;
    verify_caller()?;
    let (after, metadata_changed) = {
        let mut state = runtime.0.lock().await;
        let metadata_changed = if let Some(advance) = &progress {
            state.gate.validate(&advance.snapshot).is_ok()
                && state
                    .session
                    .as_ref()
                    .is_some_and(|session| Arc::ptr_eq(session, &advance.session))
                && state
                    .store
                    .current_registration()
                    .await
                    .ok()
                    .flatten()
                    .as_ref()
                    == Some(&advance.binding)
                && state
                    .product_sessions
                    .get(&request.edit_session_id)
                    .is_some_and(|edit| {
                        edit.document_id == request.document_id
                            && edit.binding == advance.binding
                            && edit.kind == ProductSessionKind::Active
                    })
        } else {
            false
        };
        (sync_observation(&state), metadata_changed)
    };
    if before != after {
        notify("workspace-sync-status-changed");
    }
    if metadata_changed {
        notify("workspace-metadata-changed");
    }
    result
}

#[tauri::command]
pub(crate) async fn workspace_inspect_dirty_remote(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDirtyRemoteRequest,
) -> Result<WorkspaceDirtyRemoteState, String> {
    check_caller(&webview)?;
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let result = inspect_dirty_remote_notifying(
        &runtime,
        &request,
        generation,
        || check_caller(&webview),
        |event| {
            let _ = webview.emit(event, ());
        },
    )
    .await?;
    Ok(match result {
        DirtyRemoteState::Pending => WorkspaceDirtyRemoteState::Pending,
        DirtyRemoteState::Unchanged => WorkspaceDirtyRemoteState::Same,
        DirtyRemoteState::Changed => WorkspaceDirtyRemoteState::Changed,
        DirtyRemoteState::Deleted => WorkspaceDirtyRemoteState::Deleted,
    })
}
