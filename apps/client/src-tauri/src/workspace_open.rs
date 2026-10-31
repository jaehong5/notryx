use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, check_caller, gate,
    registration_binding, send, sync_bootstrap, sync_catchup, to_contract,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_runtime::Session;
use notryx_client_store::{MAX_DOCUMENT_BYTES, RegistrationBinding, StoreError};
use notryx_protocol::{
    WorkspaceCacheWarning, WorkspaceOpenRequest, WorkspaceOpenedDocument, WorkspaceSyncState,
    WorkspaceSyncStatus,
};
use std::sync::Arc;
use tauri::{Emitter, Webview};
use uuid::Uuid;

#[tauri::command]
pub(super) async fn workspace_open(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    mut request: WorkspaceOpenRequest,
) -> Result<WorkspaceOpenedDocument, String> {
    let mut previous: Option<WorkspaceRemoteOpen> = None;
    for attempt in 0..2 {
        check_caller(&webview)?;
        let start = begin_workspace_open(&runtime, &request, previous.as_ref()).await?;
        let (remote, stale) = match start {
            WorkspaceOpenStart::Draft(opened) => return Ok(opened),
            WorkspaceOpenStart::Remote(remote) => (remote, false),
            WorkspaceOpenStart::Stale(remote) => (remote, true),
        };
        let result = if stale {
            Err("stale_content".into())
        } else {
            let bytes = remote.session.content(&request.node).await;
            let notify_sync = matches!(
                bytes,
                Err(notryx_client_runtime::Error::Unauthorized
                    | notryx_client_runtime::Error::Contract
                    | notryx_client_runtime::Error::EpochMismatch
                    | notryx_client_runtime::Error::Binding)
            );
            let revision = request
                .node
                .content_rev
                .as_ref()
                .ok_or("invalid_node")?
                .as_str()
                .to_owned();
            let binding = registration_binding(remote.session.binding());
            check_caller(&webview)?;
            let result = finish_workspace_open(
                &runtime,
                remote.clone(),
                binding,
                request.node.node_id.to_string(),
                revision,
                bytes,
            )
            .await;
            if notify_sync {
                let _ = webview.emit("workspace-sync-status-changed", ());
            }
            result
        };
        if attempt != 0
            || !matches!(&result, Err(error) if error == "Changed" || error == "stale_content")
        {
            return result;
        }
        check_caller(&webview)?;
        let latest = refresh_open_node(&runtime, &remote).await;
        check_caller(&webview)?;
        let metadata_changed = latest.is_ok();
        let refreshed = finish_open_refresh(&runtime, &remote, latest).await;
        let _ = webview.emit("workspace-sync-status-changed", ());
        if metadata_changed && refreshed.is_ok() {
            let _ = webview.emit("workspace-metadata-changed", ());
        }
        let node = match refreshed? {
            WorkspaceRefresh::Draft(opened) => return Ok(opened),
            WorkspaceRefresh::Node(node) => node,
            WorkspaceRefresh::Deleted => return Err("node_deleted".into()),
        };
        request.node = node;
        previous = Some(remote);
    }
    Err("stale_content".into())
}

pub(super) enum WorkspaceOpenStart {
    Draft(WorkspaceOpenedDocument),
    Remote(WorkspaceRemoteOpen),
    Stale(WorkspaceRemoteOpen),
}

#[derive(Clone)]
pub(super) struct WorkspaceRemoteOpen {
    snapshot: gate::RequestSnapshot,
    pub(super) session: Arc<Session>,
    pub(super) generation: u64,
    node: notryx_protocol::NodeState,
}

pub(super) async fn refresh_open_node(
    runtime: &Runtime,
    remote: &WorkspaceRemoteOpen,
) -> Result<Option<notryx_protocol::NodeState>, String> {
    let binding = registration_binding(remote.session.binding());
    // A concurrent catch-up can commit the page first. Retry only under the original identity and generation.
    for attempt in 0..2 {
        let state =
            sync_bootstrap::checked_state(runtime, &remote.snapshot, &remote.session).await?;
        if !state
            .transmission_gate
            .response_trusted(&binding, remote.generation)
        {
            return Err("stale_sync".into());
        }
        drop(state);
        let result = sync_catchup::catch_up_for_generation(
            runtime,
            Some(&remote.snapshot),
            &remote.session,
            Some(remote.generation),
        )
        .await;
        if attempt == 0 && matches!(result, Err(sync_catchup::CatchUpError::StaleSync)) {
            continue;
        }
        result.map_err(|error| error.message())?;
        break;
    }
    let mut state =
        sync_bootstrap::checked_state(runtime, &remote.snapshot, &remote.session).await?;
    if !state
        .transmission_gate
        .response_trusted(&binding, remote.generation)
    {
        return Err("stale_sync".into());
    }
    state
        .store
        .current_remote_file(&binding, remote.node.node_id)
        .await
        .map_err(|error| error.to_string())
}

pub(super) enum WorkspaceRefresh {
    Draft(WorkspaceOpenedDocument),
    Node(notryx_protocol::NodeState),
    Deleted,
}

pub(super) async fn finish_open_refresh(
    runtime: &Runtime,
    remote: &WorkspaceRemoteOpen,
    result: Result<Option<notryx_protocol::NodeState>, String>,
) -> Result<WorkspaceRefresh, String> {
    let mut state =
        sync_bootstrap::checked_state(runtime, &remote.snapshot, &remote.session).await?;
    let binding = registration_binding(remote.session.binding());
    if let Some(opened) = state
        .try_open_local_draft(&binding, &remote.node.node_id.to_string())
        .await?
    {
        return Ok(WorkspaceRefresh::Draft(opened));
    }
    if !state
        .transmission_gate
        .response_trusted(&binding, remote.generation)
    {
        return Err("stale_sync".into());
    }
    Ok(match result? {
        Some(node) => WorkspaceRefresh::Node(node),
        None => WorkspaceRefresh::Deleted,
    })
}

async fn open_local_workspace(
    state: &mut RuntimeState,
    binding: RegistrationBinding,
    node: &notryx_protocol::NodeState,
) -> Result<WorkspaceOpenedDocument, String> {
    if state
        .store
        .sync_index(&binding)
        .await
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Err("not_connected".into());
    }
    let document_id = node.node_id.to_string();
    if let Some(opened) = open_confirmed_stale_cache(state, &binding, node, false).await? {
        return Ok(opened);
    }
    if let Some(opened) = state.try_open_local_draft(&binding, &document_id).await? {
        return Ok(opened);
    }
    if !state
        .store
        .remote_content_matches(&binding, node)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("stale_content".into());
    }
    let cached = state
        .store
        .read_cached_content(&binding, node.node_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Network")?;
    if !cached.current {
        return Err("Network".into());
    }
    let revision = node.content_rev.as_ref().ok_or("invalid_node")?.as_str();
    if cached.content_rev != revision {
        return Err("stale_content".into());
    }
    let readonly =
        cached.bytes.len() > MAX_DOCUMENT_BYTES || std::str::from_utf8(&cached.bytes).is_err();
    let (edit_session_id, generation, bytes) = if readonly {
        (Uuid::new_v4().to_string(), 0, cached.bytes)
    } else {
        let opened = state
            .store
            .open_registered_document(&binding, &document_id, &cached.bytes, revision)
            .await
            .map_err(|error| error.to_string())?;
        (opened.edit_session_id, opened.generation, opened.bytes)
    };
    if let Err(error) = state.gate.open_session(edit_session_id.clone()) {
        if !readonly {
            let _ = state
                .store
                .close_registered_document(&binding, &document_id, &edit_session_id)
                .await;
        }
        return Err(error.into());
    }
    state.product_sessions.insert(
        edit_session_id.clone(),
        ProductSession {
            document_id: document_id.clone(),
            binding: binding.clone(),
            kind: if readonly {
                ProductSessionKind::Readonly
            } else {
                ProductSessionKind::Active
            },
        },
    );
    Ok(WorkspaceOpenedDocument {
        binding: to_contract(binding),
        document_id,
        edit_session_id,
        generation: generation.to_string(),
        bytes_base64: STANDARD.encode(bytes),
        readonly,
        local_draft: false,
        has_session: false,
        cache_warning: None,
        open_notice: None,
    })
}

async fn open_confirmed_stale_cache(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
    node: &notryx_protocol::NodeState,
    has_session: bool,
) -> Result<Option<WorkspaceOpenedDocument>, String> {
    let Some(revision) = node.content_rev.as_ref() else {
        return Ok(None);
    };
    let document_id = node.node_id.to_string();
    if !state
        .store
        .has_confirmed_older_checkpoint(binding, &document_id, revision.as_str())
        .await
        .map_err(|error| error.to_string())?
        || !state
            .store
            .remote_content_matches(binding, node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Ok(None);
    }
    let Some(cached) = state
        .store
        .read_cached_content(binding, node.node_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    if !cached.current || cached.content_rev != revision.as_str() {
        return Ok(None);
    }
    let readonly =
        cached.bytes.len() > MAX_DOCUMENT_BYTES || std::str::from_utf8(&cached.bytes).is_err();
    let (edit_session_id, generation, bytes) = if readonly {
        (Uuid::new_v4().to_string(), 0, cached.bytes)
    } else {
        let opened = state
            .store
            .open_registered_document(binding, &document_id, &cached.bytes, revision.as_str())
            .await
            .map_err(|error| error.to_string())?;
        let refreshed = async {
            let base_revision = state
                .store
                .refreshable_registered_session_revision(
                    binding,
                    &document_id,
                    &opened.edit_session_id,
                )
                .await?
                .ok_or(StoreError::StaleSession)?;
            state
                .store
                .refresh_clean_registered_session(
                    binding,
                    &document_id,
                    &opened.edit_session_id,
                    &base_revision,
                    node,
                )
                .await
        }
        .await;
        match refreshed {
            Ok(Some(bytes)) => (opened.edit_session_id, opened.generation, bytes),
            result => {
                state
                    .store
                    .close_registered_document(binding, &document_id, &opened.edit_session_id)
                    .await
                    .map_err(|error| error.to_string())?;
                return result.map(|_| None).map_err(|error| error.to_string());
            }
        }
    };
    if let Err(error) = state.gate.open_session(edit_session_id.clone()) {
        if !readonly {
            let _ = state
                .store
                .close_registered_document(binding, &document_id, &edit_session_id)
                .await;
        }
        return Err(error.into());
    }
    state.product_sessions.insert(
        edit_session_id.clone(),
        ProductSession {
            document_id: document_id.clone(),
            binding: binding.clone(),
            kind: if readonly {
                ProductSessionKind::Readonly
            } else {
                ProductSessionKind::Active
            },
        },
    );
    Ok(Some(WorkspaceOpenedDocument {
        binding: to_contract(binding.clone()),
        document_id,
        edit_session_id,
        generation: generation.to_string(),
        bytes_base64: STANDARD.encode(bytes),
        readonly,
        local_draft: false,
        has_session,
        cache_warning: None,
        open_notice: readonly.then_some(notryx_protocol::WorkspaceOpenNotice::PriorDraftPreserved),
    }))
}

pub(super) async fn begin_workspace_open(
    runtime: &Runtime,
    request: &WorkspaceOpenRequest,
    previous: Option<&WorkspaceRemoteOpen>,
) -> Result<WorkspaceOpenStart, String> {
    let mut state = runtime.0.lock().await;
    let snapshot = state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let Some(session) = state.session.clone() else {
        if previous.is_some() {
            return Err("stale_registration".into());
        }
        let binding = state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .ok_or("not_connected")?;
        return open_local_workspace(&mut state, binding, &request.node)
            .await
            .map(WorkspaceOpenStart::Draft);
    };
    if let Some(previous) = previous {
        state
            .gate
            .validate(&previous.snapshot)
            .map_err(str::to_owned)?;
        if !Arc::ptr_eq(&session, &previous.session) {
            return Err("stale_registration".into());
        }
    }
    let binding = registration_binding(session.binding());
    if state.transmission_gate.blocked_status(&binding)
        == notryx_protocol::WorkspaceSendStatus::AuthRequired
    {
        return open_local_workspace(&mut state, binding, &request.node)
            .await
            .map(WorkspaceOpenStart::Draft);
    }
    let document_id = request.node.node_id.to_string();
    if let Some(opened) =
        open_confirmed_stale_cache(&mut state, &binding, &request.node, true).await?
    {
        return Ok(WorkspaceOpenStart::Draft(opened));
    }
    if let Some(opened) = state.try_open_local_draft(&binding, &document_id).await? {
        return Ok(WorkspaceOpenStart::Draft(opened));
    }
    if previous.is_some_and(|previous| {
        !state
            .transmission_gate
            .response_trusted(&binding, previous.generation)
    }) {
        return Err("stale_sync".into());
    }
    let generation = state.transmission_gate.response_generation(&binding);
    if !state
        .transmission_gate
        .response_trusted(&binding, generation)
    {
        return Err("stale_sync".into());
    }
    let matches = state
        .store
        .remote_content_matches(&binding, &request.node)
        .await
        .map_err(|error| error.to_string())?;
    let remote = WorkspaceRemoteOpen {
        snapshot,
        session,
        generation,
        node: request.node.clone(),
    };
    Ok(if matches {
        WorkspaceOpenStart::Remote(remote)
    } else {
        WorkspaceOpenStart::Stale(remote)
    })
}

pub(super) async fn finish_workspace_open(
    runtime: &Runtime,
    remote: WorkspaceRemoteOpen,
    binding: RegistrationBinding,
    document_id: String,
    revision: String,
    bytes: Result<Vec<u8>, notryx_client_runtime::Error>,
) -> Result<WorkspaceOpenedDocument, String> {
    let mut state = runtime.0.lock().await;
    state
        .gate
        .validate(&remote.snapshot)
        .map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &remote.session))
    {
        return Err("stale_registration".into());
    }
    let trusted = state
        .transmission_gate
        .response_trusted(&binding, remote.generation);
    if trusted && matches!(bytes, Err(notryx_client_runtime::Error::Unauthorized)) {
        state
            .transmission_gate
            .block(&binding, send::TransmissionBlock::AuthRequired);
        state.sync_state = Some(WorkspaceSyncState {
            binding: to_contract(binding.clone()),
            status: WorkspaceSyncStatus::AuthRequired,
        });
    }
    if state.transmission_gate.blocked_status(&binding)
        == notryx_protocol::WorkspaceSendStatus::AuthRequired
    {
        if let Err(
            error @ (notryx_client_runtime::Error::Forbidden
            | notryx_client_runtime::Error::Contract
            | notryx_client_runtime::Error::EpochMismatch
            | notryx_client_runtime::Error::Binding
            | notryx_client_runtime::Error::TooLarge),
        ) = bytes
        {
            return Err(format!("{error:?}"));
        }
        return open_local_workspace(&mut state, binding, &remote.node).await;
    }
    if trusted {
        let reason = match bytes {
            Err(notryx_client_runtime::Error::Contract) => {
                Some(send::TransmissionBlock::ProtocolMismatch)
            }
            Err(notryx_client_runtime::Error::EpochMismatch) => {
                Some(send::TransmissionBlock::EpochMismatch)
            }
            Err(notryx_client_runtime::Error::Binding) => {
                Some(send::TransmissionBlock::BindingMismatch)
            }
            _ => None,
        };
        if let Some(reason) = reason {
            state.transmission_gate.block(&binding, reason);
            state.sync_state = Some(WorkspaceSyncState {
                binding: to_contract(binding.clone()),
                status: WorkspaceSyncStatus::NeedsReview,
            });
        }
    }
    if let Some(opened) = state.try_open_local_draft(&binding, &document_id).await? {
        return Ok(opened);
    }
    if !trusted {
        return Err("stale_sync".into());
    }
    let (bytes, from_cache) = match bytes {
        Ok(bytes) => (bytes, false),
        Err(notryx_client_runtime::Error::Network | notryx_client_runtime::Error::Unavailable) => {
            let cached = state
                .store
                .read_cached_content(&binding, remote.node.node_id)
                .await
                .map_err(|error| error.to_string())?;
            match cached {
                Some(cached) if cached.current && cached.content_rev == revision => {
                    (cached.bytes, true)
                }
                _ => return Err("Network".into()),
            }
        }
        Err(error) => return Err(format!("{error:?}")),
    };
    if !state
        .store
        .remote_content_matches(&binding, &remote.node)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("stale_content".into());
    }
    let readonly = bytes.len() > MAX_DOCUMENT_BYTES || std::str::from_utf8(&bytes).is_err();
    let (edit_session_id, generation, bytes) = if readonly {
        (Uuid::new_v4().to_string(), 0, bytes)
    } else {
        let opened = state
            .store
            .open_registered_document(&binding, &document_id, &bytes, &revision)
            .await
            .map_err(|error| error.to_string())?;
        (opened.edit_session_id, opened.generation, opened.bytes)
    };
    if let Err(error) = state.gate.open_session(edit_session_id.clone()) {
        if !readonly {
            let _ = state
                .store
                .close_registered_document(&binding, &document_id, &edit_session_id)
                .await;
        }
        return Err(error.into());
    }
    state.product_sessions.insert(
        edit_session_id.clone(),
        ProductSession {
            document_id: document_id.clone(),
            binding: binding.clone(),
            kind: if readonly {
                ProductSessionKind::Readonly
            } else {
                ProductSessionKind::Active
            },
        },
    );
    let cache_warning = if from_cache {
        None
    } else {
        match state
            .store
            .cache_content(&binding, &remote.node, &bytes)
            .await
        {
            Ok(true) => None,
            Ok(false) => Some(WorkspaceCacheWarning::Unexpected),
            Err(StoreError::UnsupportedFormat) => Some(WorkspaceCacheWarning::Damaged),
            Err(StoreError::Storage | StoreError::StoreLocked | StoreError::StorePathChanged) => {
                Some(WorkspaceCacheWarning::Storage)
            }
            Err(_) => Some(WorkspaceCacheWarning::Unexpected),
        }
    };
    Ok(WorkspaceOpenedDocument {
        binding: to_contract(binding),
        document_id,
        edit_session_id,
        generation: generation.to_string(),
        bytes_base64: STANDARD.encode(bytes),
        readonly,
        local_draft: false,
        has_session: true,
        cache_warning,
        open_notice: None,
    })
}
