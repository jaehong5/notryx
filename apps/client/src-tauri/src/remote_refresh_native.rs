use crate::{
    ProductSessionKind, Runtime, check_caller, gate::RequestSnapshot, registration_binding,
    send::TransmissionBlock, sync_bootstrap, sync_catchup, sync_content,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_runtime::{Error as NetworkError, Session};
use notryx_client_store::{MAX_DOCUMENT_BYTES, RegistrationBinding};
use notryx_domain::NodeId;
use notryx_protocol::{
    NodeState, WorkspaceRemoteRefreshCandidate, WorkspaceRemoteRefreshCommitRequest,
    WorkspaceRemoteRefreshRequest, WorkspaceSyncState, WorkspaceSyncStatus,
};
use std::sync::Arc;
use tauri::{Emitter, Webview};
use uuid::Uuid;

pub(crate) struct Candidate {
    token: String,
    snapshot: RequestSnapshot,
    session: Arc<Session>,
    binding: RegistrationBinding,
    generation: u64,
    node: NodeState,
    base_revision: String,
}

fn product_session_matches(
    state: &crate::RuntimeState,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
) -> bool {
    state
        .product_sessions
        .get(edit_session_id)
        .is_some_and(|edit| {
            edit.document_id == document_id
                && edit.binding == *binding
                && edit.kind == ProductSessionKind::Active
        })
}

pub(crate) struct Preparation {
    snapshot: RequestSnapshot,
    pub(crate) session: Arc<Session>,
    binding: RegistrationBinding,
    generation: u64,
    pub(crate) node: NodeState,
    base_revision: String,
}

pub(crate) async fn begin_prepare(
    runtime: &Runtime,
    request: &WorkspaceRemoteRefreshRequest,
) -> Result<Option<Preparation>, String> {
    let node_id = request
        .document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    let (snapshot, session, binding, generation, node, base_revision) = {
        let mut state = runtime.0.lock().await;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        let binding = registration_binding(session.binding());
        if !product_session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        ) {
            return Err("stale_session".into());
        }
        state
            .remote_refresh_candidates
            .remove(&request.edit_session_id);
        let generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, generation)
        {
            return Err("stale_sync".into());
        }
        let Some(base_revision) = state
            .store
            .refreshable_registered_session_revision(
                &binding,
                &request.document_id,
                &request.edit_session_id,
            )
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let Some(node) = state
            .store
            .current_remote_file(&binding, node_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let Some(revision) = node.content_rev.as_ref() else {
            return Ok(None);
        };
        if revision.as_str() == base_revision
            || node
                .size
                .as_deref()
                .and_then(|size| size.parse::<u64>().ok())
                .is_none_or(|size| size > MAX_DOCUMENT_BYTES as u64)
        {
            return Ok(None);
        }
        (snapshot, session, binding, generation, node, base_revision)
    };

    Ok(Some(Preparation {
        snapshot,
        session,
        binding,
        generation,
        node,
        base_revision,
    }))
}

#[tauri::command]
pub(crate) async fn workspace_prepare_remote_refresh(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceRemoteRefreshRequest,
) -> Result<Option<WorkspaceRemoteRefreshCandidate>, String> {
    check_caller(&webview)?;
    let Some(preparation) = begin_prepare(&runtime, &request).await? else {
        return Ok(None);
    };
    let fetched =
        sync_content::download_and_cache(&runtime, &preparation.session, &preparation.node).await;
    check_caller(&webview)?;
    match fetched {
        Ok(true) => (),
        Ok(false) => return Ok(None),
        Err(sync_content::ContentFetchError::Network(NetworkError::Changed)) => {
            if sync_catchup::catch_up_for_generation(
                &runtime,
                Some(&preparation.snapshot),
                &preparation.session,
                Some(preparation.generation),
            )
            .await
            .is_ok()
            {
                let _ = webview.emit("workspace-metadata-changed", ());
            }
            return Err("stale_content".into());
        }
        Err(sync_content::ContentFetchError::Network(error)) => {
            let mut state = sync_bootstrap::checked_state(
                &runtime,
                &preparation.snapshot,
                &preparation.session,
            )
            .await?;
            if !state
                .transmission_gate
                .response_trusted(&preparation.binding, preparation.generation)
            {
                return Err("stale_sync".into());
            }
            let blocked = match error {
                NetworkError::Unauthorized => Some((
                    TransmissionBlock::AuthRequired,
                    WorkspaceSyncStatus::AuthRequired,
                )),
                NetworkError::Binding | NetworkError::EpochMismatch => Some((
                    TransmissionBlock::BindingMismatch,
                    WorkspaceSyncStatus::NeedsReview,
                )),
                NetworkError::Contract | NetworkError::TooLarge => Some((
                    TransmissionBlock::ProtocolMismatch,
                    WorkspaceSyncStatus::NeedsReview,
                )),
                NetworkError::ResyncRequired => Some((
                    TransmissionBlock::ResyncRequired,
                    WorkspaceSyncStatus::NeedsReview,
                )),
                _ => None,
            };
            if let Some((reason, status)) = blocked {
                state.transmission_gate.block(&preparation.binding, reason);
                state.sync_state = Some(WorkspaceSyncState {
                    binding: crate::to_contract(preparation.binding.clone()),
                    status,
                });
                drop(state);
                let _ = webview.emit("workspace-sync-status-changed", ());
            }
            return Err(format!("{error:?}"));
        }
        Err(sync_content::ContentFetchError::Store(error)) => return Err(error.to_string()),
        Err(sync_content::ContentFetchError::Guard(error)) => return Err(error),
    }

    finish_prepare(&runtime, &request, preparation).await
}

pub(crate) async fn finish_prepare(
    runtime: &Runtime,
    request: &WorkspaceRemoteRefreshRequest,
    preparation: Preparation,
) -> Result<Option<WorkspaceRemoteRefreshCandidate>, String> {
    let Preparation {
        snapshot,
        session,
        binding,
        generation,
        node,
        base_revision,
    } = preparation;
    let node_id = node.node_id;
    let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
    if !state
        .transmission_gate
        .response_trusted(&binding, generation)
        || !product_session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        )
    {
        return Err("stale_session".into());
    }
    if state
        .store
        .refreshable_registered_session_revision(
            &binding,
            &request.document_id,
            &request.edit_session_id,
        )
        .await
        .map_err(|error| error.to_string())?
        .as_deref()
        != Some(base_revision.as_str())
        || !state
            .store
            .remote_content_matches(&binding, &node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Ok(None);
    }
    let Some(cached) = state
        .store
        .read_cached_content(&binding, node_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    if !cached.current
        || cached.content_rev != node.content_rev.as_ref().ok_or("invalid_node")?.as_str()
        || cached.bytes.len() > MAX_DOCUMENT_BYTES
        || std::str::from_utf8(&cached.bytes).is_err()
    {
        return Ok(None);
    }
    let token = Uuid::new_v4().to_string();
    let bytes_base64 = STANDARD.encode(&cached.bytes);
    state.remote_refresh_candidates.insert(
        request.edit_session_id.clone(),
        Candidate {
            token: token.clone(),
            snapshot,
            session,
            binding,
            generation,
            node,
            base_revision,
        },
    );
    Ok(Some(WorkspaceRemoteRefreshCandidate {
        token,
        bytes_base64,
    }))
}

#[tauri::command]
pub(crate) async fn workspace_commit_remote_refresh(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceRemoteRefreshCommitRequest,
) -> Result<bool, String> {
    check_caller(&webview)?;
    commit_candidate(&runtime, request).await
}

pub(crate) async fn commit_candidate(
    runtime: &Runtime,
    request: WorkspaceRemoteRefreshCommitRequest,
) -> Result<bool, String> {
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let Some(candidate) = state
        .remote_refresh_candidates
        .get(&request.edit_session_id)
    else {
        return Ok(false);
    };
    if candidate.token != request.token || candidate.node.node_id.to_string() != request.document_id
    {
        return Ok(false);
    }
    let candidate = state
        .remote_refresh_candidates
        .remove(&request.edit_session_id)
        .ok_or("stale_session")?;
    state
        .gate
        .validate(&candidate.snapshot)
        .map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &candidate.session))
        || !state
            .transmission_gate
            .response_trusted(&candidate.binding, candidate.generation)
        || !product_session_matches(
            &state,
            &candidate.binding,
            &request.document_id,
            &request.edit_session_id,
        )
    {
        return Ok(false);
    }
    let replaced = state
        .store
        .refresh_clean_registered_session(
            &candidate.binding,
            &request.document_id,
            &request.edit_session_id,
            &candidate.base_revision,
            &candidate.node,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(replaced.is_some())
}

#[cfg(test)]
#[path = "remote_refresh_tests.rs"]
mod tests;
