#[cfg(test)]
#[path = "conflict_test_support.rs"]
pub(crate) mod test_support;

use crate::{
    ProductSessionKind, Runtime, RuntimeState, registration_binding, sync_bootstrap, sync_catchup,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_store::{ContentConflictSnapshot, MAX_DOCUMENT_BYTES, RegistrationBinding};
use notryx_domain::NodeId;
use notryx_protocol::{
    NodeState, WorkspaceContentConflictRequest, WorkspaceContentConflictSnapshot,
};
use tauri::Webview;

pub(crate) struct ConflictSnapshot {
    pub(crate) binding: RegistrationBinding,
    pub(crate) node_id: NodeId,
    pub(crate) operation_id: String,
    pub(crate) operation_seq: u64,
    pub(crate) generation: u64,
    pub(crate) base_revision: String,
    pub(crate) base_bytes: Vec<u8>,
    pub(crate) local_bytes: Vec<u8>,
    pub(crate) conflict_revision: String,
    pub(crate) remote_revision: String,
    pub(crate) remote_bytes: Vec<u8>,
    remote_node: NodeState,
}

fn session_matches(
    state: &RuntimeState,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
) -> bool {
    state
        .product_sessions
        .get(edit_session_id)
        .is_some_and(|session| {
            session.binding == *binding
                && session.document_id == document_id
                && session.kind == ProductSessionKind::Active
        })
}

struct CompletionSessionGuard<'a> {
    state: &'a mut RuntimeState,
    edit_session_id: &'a str,
    armed: bool,
}

fn matches_displayed_hash(expected: &str, bytes: &[u8]) -> bool {
    let Some(hex) = expected.strip_prefix("sha256:") else {
        return false;
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |byte: u8| {
            if byte.is_ascii_digit() {
                byte - b'0'
            } else {
                byte - b'a' + 10
            }
        };
        digest[index] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    let header = format!("sha-256=:{}:", STANDARD.encode(digest));
    notryx_protocol::verify_content_digest(&header, bytes).is_ok()
}

impl Drop for CompletionSessionGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.state
                .store
                .invalidate_edit_session(self.edit_session_id);
            self.state.product_sessions.remove(self.edit_session_id);
            let _ = self.state.gate.close_session(self.edit_session_id);
        }
    }
}

async fn checked_local(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    operation_id: &str,
) -> Result<ContentConflictSnapshot, String> {
    if !session_matches(state, binding, document_id, edit_session_id) {
        return Err("stale_session".into());
    }
    let edit = state
        .store
        .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_session")?;
    let conflict = state
        .store
        .inspect_content_conflict(binding, document_id, operation_id)
        .await
        .map_err(|error| error.to_string())?;
    if conflict.generation != generation
        || edit.checkpoint_generation != Some(generation)
        || edit.base_revision != conflict.base_revision
        || edit.has_sent
    {
        return Err("stale_checkpoint".into());
    }
    if std::str::from_utf8(&conflict.base).is_err() || std::str::from_utf8(&conflict.local).is_err()
    {
        return Err("unsupported_content".into());
    }
    Ok(conflict)
}

pub(crate) async fn prepare_conflict_snapshot(
    runtime: &Runtime,
    owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    operation_id: &str,
) -> Result<ConflictSnapshot, String> {
    let node_id = document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    let (snapshot, session, binding, response_generation, local) = {
        let mut state = runtime.0.lock().await;
        let snapshot = state.gate.snapshot(owner).map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        let binding = registration_binding(session.binding());
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let local = checked_local(
            &mut state,
            &binding,
            document_id,
            edit_session_id,
            generation,
            operation_id,
        )
        .await?;
        (snapshot, session, binding, response_generation, local)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&snapshot),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let node = {
        let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
            || checked_local(
                &mut state,
                &binding,
                document_id,
                edit_session_id,
                generation,
                operation_id,
            )
            .await?
                != local
        {
            return Err("stale_conflict".into());
        }
        let node = state
            .store
            .current_remote_file(&binding, node_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("remote_not_available")?;
        let size = node
            .size
            .as_deref()
            .ok_or("invalid_node")?
            .parse::<u64>()
            .map_err(|_| "invalid_node")?;
        if size > MAX_DOCUMENT_BYTES as u64 {
            return Err("unsupported_content".into());
        }
        node
    };
    let bytes = session
        .content(&node)
        .await
        .map_err(|error| format!("{error:?}"))?;
    {
        let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
            || checked_local(
                &mut state,
                &binding,
                document_id,
                edit_session_id,
                generation,
                operation_id,
            )
            .await?
                != local
            || !state
                .store
                .remote_content_matches(&binding, &node)
                .await
                .map_err(|error| error.to_string())?
        {
            return Err("stale_conflict".into());
        }
        if !state
            .store
            .cache_content(&binding, &node, &bytes)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err("stale_content".into());
        }
    }
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&snapshot),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &snapshot, &session).await?;
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
        || checked_local(
            &mut state,
            &binding,
            document_id,
            edit_session_id,
            generation,
            operation_id,
        )
        .await?
            != local
        || !state
            .store
            .remote_content_matches(&binding, &node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err("stale_conflict".into());
    }
    let cached = state
        .store
        .read_cached_content(&binding, node_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("remote_not_available")?;
    let remote_revision = node
        .content_rev
        .as_ref()
        .ok_or("invalid_node")?
        .as_str()
        .to_owned();
    if !cached.current
        || cached.content_rev != remote_revision
        || cached.bytes.len() > MAX_DOCUMENT_BYTES
        || std::str::from_utf8(&cached.bytes).is_err()
    {
        return Err("unsupported_content".into());
    }
    Ok(ConflictSnapshot {
        binding,
        node_id,
        operation_id: operation_id.to_owned(),
        operation_seq: local.operation_seq,
        generation,
        conflict_revision: local.conflict_revision,
        base_revision: local.base_revision,
        base_bytes: local.base,
        local_bytes: local.local,
        remote_revision,
        remote_bytes: cached.bytes,
        remote_node: node,
    })
}

pub(crate) async fn prepare_first_conflict_snapshot(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
) -> Result<Option<ConflictSnapshot>, String> {
    document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    let operation_id = {
        let mut state = runtime.0.lock().await;
        state.gate.snapshot(owner).map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *binding
            || !session_matches(&state, binding, document_id, edit_session_id)
        {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(binding);
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let current = state
            .store
            .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_session")?;
        if current.checkpoint_generation.unwrap_or(0) != generation {
            return Err("stale_checkpoint".into());
        }
        state
            .store
            .first_content_conflict(binding, document_id)
            .await
            .map_err(|error| error.to_string())?
            .map(|entry| entry.operation_id)
    };
    let Some(operation_id) = operation_id else {
        return Ok(None);
    };
    let snapshot = prepare_conflict_snapshot(
        runtime,
        owner,
        document_id,
        edit_session_id,
        generation,
        &operation_id,
    )
    .await?;
    if snapshot.binding != *binding || snapshot.operation_id != operation_id {
        return Err("stale_session".into());
    }
    Ok(Some(snapshot))
}

async fn prepare_content_conflict_for_renderer_inner(
    runtime: &Runtime,
    request: WorkspaceContentConflictRequest,
    #[cfg(test)] hook: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<Option<WorkspaceContentConflictSnapshot>, String> {
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let binding = crate::from_contract(&request.binding);
    let snapshot = prepare_first_conflict_snapshot(
        runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.edit_session_id,
        generation,
    )
    .await?;
    let (gate, session, response_generation) = {
        let state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding
            || !session_matches(
                &state,
                &binding,
                &request.document_id,
                &request.edit_session_id,
            )
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
        (gate, session, response_generation)
    };
    let result = snapshot
        .as_ref()
        .map(|snapshot| WorkspaceContentConflictSnapshot {
            binding: crate::to_contract(snapshot.binding.clone()),
            document_id: snapshot.node_id.to_string(),
            operation_id: snapshot.operation_id.clone(),
            operation_seq: snapshot.operation_seq.to_string(),
            generation: snapshot.generation.to_string(),
            base_revision: snapshot.base_revision.clone(),
            base_bytes_base64: STANDARD.encode(&snapshot.base_bytes),
            base_hash: notryx_protocol::content_hash(&snapshot.base_bytes),
            local_bytes_base64: STANDARD.encode(&snapshot.local_bytes),
            local_hash: notryx_protocol::content_hash(&snapshot.local_bytes),
            conflict_revision: snapshot.conflict_revision.clone(),
            remote_revision: snapshot.remote_revision.clone(),
            remote_bytes_base64: STANDARD.encode(&snapshot.remote_bytes),
            remote_hash: notryx_protocol::content_hash(&snapshot.remote_bytes),
        });
    #[cfg(test)]
    if let Some((ready, resume)) = hook {
        let _ = ready.send(());
        resume.await.map_err(|_| "test_hook_closed")?;
    }
    let mut state = sync_bootstrap::checked_state(runtime, &gate, &session).await?;
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
        || !session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        )
    {
        return Err("stale_session".into());
    }
    let edit = state
        .store
        .dirty_registered_session_snapshot(&binding, &request.document_id, &request.edit_session_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_session")?;
    if edit.checkpoint_generation.unwrap_or(0) != generation {
        return Err("stale_checkpoint".into());
    }
    let first = state
        .store
        .first_content_conflict(&binding, &request.document_id)
        .await
        .map_err(|error| error.to_string())?;
    match (&snapshot, first) {
        (None, None) => {}
        (Some(snapshot), Some(first))
            if first.operation_id == snapshot.operation_id
                && first.operation_seq == snapshot.operation_seq =>
        {
            let local = checked_local(
                &mut state,
                &binding,
                &request.document_id,
                &request.edit_session_id,
                generation,
                &snapshot.operation_id,
            )
            .await?;
            if local.base_revision != snapshot.base_revision
                || local.base != snapshot.base_bytes
                || local.local != snapshot.local_bytes
                || local.conflict_revision != snapshot.conflict_revision
                || !state
                    .store
                    .remote_content_matches(&binding, &snapshot.remote_node)
                    .await
                    .map_err(|error| error.to_string())?
            {
                return Err("stale_conflict".into());
            }
        }
        _ => return Err("stale_conflict".into()),
    }
    Ok(result)
}

#[cfg(test)]
async fn prepare_content_conflict_for_renderer_with_hook(
    runtime: &Runtime,
    request: WorkspaceContentConflictRequest,
    ready: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
) -> Result<Option<WorkspaceContentConflictSnapshot>, String> {
    prepare_content_conflict_for_renderer_inner(runtime, request, Some((ready, resume))).await
}

#[tauri::command]
pub(crate) async fn workspace_prepare_content_conflict(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceContentConflictRequest,
) -> Result<Option<WorkspaceContentConflictSnapshot>, String> {
    crate::check_caller(&webview)?;
    let result = prepare_content_conflict_for_renderer_inner(
        &runtime,
        request,
        #[cfg(test)]
        None,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[allow(dead_code)]
#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
pub(crate) async fn choose_remote_after_confirmation(
    runtime: &Runtime,
    owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    operation_id: &str,
    expected_remote_revision: &str,
    expected_remote_hash: &str,
    expected_base_hash: &str,
    expected_local_hash: &str,
    expected_base_revision: &str,
    expected_conflict_revision: &str,
) -> Result<(), String> {
    choose_remote_after_confirmation_inner(
        runtime,
        owner,
        document_id,
        edit_session_id,
        generation,
        operation_id,
        expected_remote_revision,
        expected_remote_hash,
        expected_base_hash,
        expected_local_hash,
        expected_base_revision,
        expected_conflict_revision,
        #[cfg(test)]
        None,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn choose_remote_after_confirmation_inner(
    runtime: &Runtime,
    owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    operation_id: &str,
    expected_remote_revision: &str,
    expected_remote_hash: &str,
    expected_base_hash: &str,
    expected_local_hash: &str,
    expected_base_revision: &str,
    expected_conflict_revision: &str,
    #[cfg(test)] hook: Option<(
        &str,
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<(), String> {
    let (gate, binding, response_generation) = {
        let state = runtime.0.lock().await;
        let gate = state.gate.snapshot(owner).map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        let binding = registration_binding(session.binding());
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        (gate, binding, response_generation)
    };
    let selected = prepare_conflict_snapshot(
        runtime,
        owner,
        document_id,
        edit_session_id,
        generation,
        operation_id,
    )
    .await?;
    if selected.conflict_revision != expected_conflict_revision
        || selected.base_revision != expected_base_revision
        || !matches_displayed_hash(expected_base_hash, &selected.base_bytes)
        || !matches_displayed_hash(expected_local_hash, &selected.local_bytes)
        || selected.remote_revision != expected_remote_revision
        || selected.remote_node.content_hash.as_deref() != Some(expected_remote_hash)
    {
        return Err("stale_conflict".into());
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != selected.binding || binding != selected.binding {
        return Err("stale_registration".into());
    }
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    let local = checked_local(
        &mut state,
        &selected.binding,
        document_id,
        edit_session_id,
        generation,
        operation_id,
    )
    .await?;
    if local.operation_seq != selected.operation_seq
        || local.local != selected.local_bytes
        || local.base != selected.base_bytes
        || local.base_revision != selected.base_revision
        || local.conflict_revision != selected.conflict_revision
        || !state
            .store
            .remote_content_matches(&selected.binding, &selected.remote_node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err("stale_conflict".into());
    }
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation,
        remote_revision: selected.remote_revision,
        remote_hash: expected_remote_hash.to_owned(),
        remote_bytes: selected.remote_bytes,
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    state
        .store
        .record_content_conflict_intent(&selected.binding, document_id, operation_id, &request)
        .await
        .map_err(|error| error.to_string())?;
    let outbox = state
        .store
        .document_outbox_state(&selected.binding, document_id)
        .await
        .map_err(|error| error.to_string())?;
    let confirmed = state
        .store
        .confirmed_receipt_through(&selected.binding)
        .await
        .map_err(|error| error.to_string())?;
    if outbox.latest.as_ref().is_none_or(|item| {
        confirmed < item.operation_seq
            || (item.operation_id != operation_id
                && item.status != notryx_client_store::OutboxStatus::Abandoned)
    }) {
        return Err("pending_conflict_resolution".into());
    }
    let mut closing = CompletionSessionGuard {
        state: &mut state,
        edit_session_id,
        armed: true,
    };
    #[cfg(test)]
    if let Some(("before_store_completion", ready, resume)) = &hook {
        ready.notify_one();
        resume.notified().await;
    }
    let completed = closing
        .state
        .store
        .complete_content_conflict_choose_remote_closing_session(
            &selected.binding,
            document_id,
            edit_session_id,
            operation_id,
        )
        .await;
    if let Err(error) = completed {
        if closing
            .state
            .store
            .dirty_registered_session_snapshot(&selected.binding, document_id, edit_session_id)
            .await
            .is_ok_and(|session| session.is_some())
        {
            closing.armed = false;
        }
        return Err(error.to_string());
    }
    #[cfg(test)]
    if let Some(("after_store_completion", ready, resume)) = &hook {
        ready.notify_one();
        resume.notified().await;
    }
    closing.state.product_sessions.remove(edit_session_id);
    closing
        .state
        .gate
        .close_session(edit_session_id)
        .map_err(str::to_owned)?;
    closing.armed = false;
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn choose_remote_after_confirmation_with_hook(
    runtime: &Runtime,
    owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    operation_id: &str,
    expected_remote_revision: &str,
    expected_remote_hash: &str,
    expected_base_hash: &str,
    expected_local_hash: &str,
    expected_base_revision: &str,
    expected_conflict_revision: &str,
    phase: &str,
    ready: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
) -> Result<(), String> {
    choose_remote_after_confirmation_inner(
        runtime,
        owner,
        document_id,
        edit_session_id,
        generation,
        operation_id,
        expected_remote_revision,
        expected_remote_hash,
        expected_base_hash,
        expected_local_hash,
        expected_base_revision,
        expected_conflict_revision,
        Some((phase, ready, resume)),
    )
    .await
}

#[cfg(test)]
mod confirmation_tests {
    include!("choose_remote_confirmation_server_tests.rs");
}

#[allow(dead_code)]
pub(crate) async fn read_pending_content_conflict_intent(
    runtime: &Runtime,
    owner: &str,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
) -> Result<Option<notryx_client_store::ContentConflictIntent>, String> {
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    let binding = registration_binding(session.binding());
    if !session_matches(&state, &binding, document_id, edit_session_id) {
        return Err("stale_session".into());
    }
    let first = state
        .store
        .first_content_conflict(&binding, document_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if first.operation_id != operation_id {
        return Err("stale_conflict".into());
    }
    let intent = state
        .store
        .read_content_conflict_intent(&binding, operation_id)
        .await
        .map_err(|error| error.to_string())?;
    if intent
        .as_ref()
        .is_some_and(|value| value.document_id != document_id || value.binding != binding)
    {
        return Err("stale_conflict".into());
    }
    Ok(intent)
}

#[allow(dead_code)]
#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
pub(crate) async fn choose_remote_after_reconsideration(
    runtime: &Runtime,
    owner: &str,
    document_id: &str,
    edit_session_id: &str,
    generation: u64,
    operation_id: &str,
    expected_intent_version: u64,
    expected_remote_revision: &str,
    expected_remote_hash: &str,
    expected_base_hash: &str,
    expected_local_hash: &str,
    expected_base_revision: &str,
    expected_conflict_revision: &str,
) -> Result<(), String> {
    let (gate, binding, response_generation, expected) = {
        let mut state = runtime.0.lock().await;
        let gate = state.gate.snapshot(owner).map_err(str::to_owned)?;
        state
            .gate
            .check_session(edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        let binding = registration_binding(session.binding());
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        checked_local(
            &mut state,
            &binding,
            document_id,
            edit_session_id,
            generation,
            operation_id,
        )
        .await?;
        let first = state
            .store
            .first_content_conflict(&binding, document_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_conflict")?;
        if first.operation_id != operation_id {
            return Err("stale_conflict".into());
        }
        let current = state
            .store
            .read_content_conflict_intent(&binding, operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_conflict")?;
        let expected = if current.version == expected_intent_version {
            current
        } else if expected_intent_version.checked_add(1) == Some(current.version) {
            state
                .store
                .read_content_conflict_intent_history(
                    &binding,
                    operation_id,
                    expected_intent_version,
                )
                .await
                .map_err(|error| error.to_string())?
                .ok_or("stale_conflict")?
        } else {
            return Err("stale_conflict".into());
        };
        (gate, binding, response_generation, expected)
    };
    let selected = prepare_conflict_snapshot(
        runtime,
        owner,
        document_id,
        edit_session_id,
        generation,
        operation_id,
    )
    .await?;
    if selected.conflict_revision != expected_conflict_revision
        || selected.base_revision != expected_base_revision
        || !matches_displayed_hash(expected_base_hash, &selected.base_bytes)
        || !matches_displayed_hash(expected_local_hash, &selected.local_bytes)
        || selected.binding != binding
        || selected.remote_revision != expected_remote_revision
        || selected.remote_node.content_hash.as_deref() != Some(expected_remote_hash)
    {
        return Err("stale_conflict".into());
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != binding
        || !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    let local = checked_local(
        &mut state,
        &binding,
        document_id,
        edit_session_id,
        generation,
        operation_id,
    )
    .await?;
    if local.operation_seq != selected.operation_seq
        || local.local != selected.local_bytes
        || local.base != selected.base_bytes
        || local.base_revision != selected.base_revision
        || local.conflict_revision != selected.conflict_revision
        || !state
            .store
            .remote_content_matches(&binding, &selected.remote_node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err("stale_conflict".into());
    }
    let request = notryx_client_store::ContentConflictIntentRequest {
        generation,
        remote_revision: selected.remote_revision,
        remote_hash: expected_remote_hash.to_owned(),
        remote_bytes: selected.remote_bytes,
        choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
    };
    state
        .store
        .replace_pending_content_conflict_intent(
            &binding,
            document_id,
            operation_id,
            &expected,
            &request,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod reconsideration_tests {
    include!("conflict_reconsider_server_tests.rs");
}

#[cfg(test)]
mod pending_history_corrupt_tests {
    include!("conflict_pending_history_corrupt_tests.rs");
}

#[cfg(test)]
mod snapshot_identity_tests {
    include!("conflict_snapshot_identity_tests.rs");
}

#[cfg(test)]
mod base_revision_identity_tests {
    include!("conflict_base_revision_identity_tests.rs");
}

#[cfg(test)]
mod failed_revision_identity_tests {
    include!("conflict_failed_revision_identity_tests.rs");
}

#[allow(dead_code)]
async fn read_resolution_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
) -> Result<notryx_protocol::WorkspaceConflictResolutionResponse, String> {
    read_resolution_for_renderer_inner(
        runtime,
        owner,
        binding,
        document_id,
        operation_id,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
async fn read_resolution_for_renderer_with_hook(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    ready: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
) -> Result<notryx_protocol::WorkspaceConflictResolutionResponse, String> {
    read_resolution_for_renderer_inner(
        runtime,
        owner,
        binding,
        document_id,
        operation_id,
        Some((ready, resume)),
    )
    .await
}

async fn read_resolution_for_renderer_inner(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    #[cfg(test)] hook: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<notryx_protocol::WorkspaceConflictResolutionResponse, String> {
    document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    operation_id
        .parse::<notryx_domain::OperationId>()
        .map_err(|_| "invalid_operation")?;
    let (gate, first) = {
        let mut state = runtime.0.lock().await;
        let gate = state.gate.snapshot(owner).map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *binding {
            return Err("stale_registration".into());
        }
        let first =
            resolution_state_for_document(&mut state, binding, document_id, operation_id).await?;
        (gate, first)
    };
    #[cfg(test)]
    if let Some((ready, resume)) = hook {
        ready.notify_one();
        resume.notified().await;
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != *binding {
        return Err("stale_registration".into());
    }
    let current =
        resolution_state_for_document(&mut state, binding, document_id, operation_id).await?;
    if current != first {
        return Err("stale_conflict".into());
    }
    let status = resolution_to_contract(current);
    Ok(notryx_protocol::WorkspaceConflictResolutionResponse {
        binding: crate::to_contract(binding.clone()),
        document_id: document_id.to_owned(),
        operation_id: operation_id.to_owned(),
        state: status,
    })
}

fn resolution_to_contract(
    current: notryx_client_store::ContentConflictResolutionState,
) -> notryx_protocol::WorkspaceConflictResolutionState {
    match current {
        notryx_client_store::ContentConflictResolutionState::NotFound => {
            notryx_protocol::WorkspaceConflictResolutionState::NotFound
        }
        notryx_client_store::ContentConflictResolutionState::Pending {
            version,
            generation,
            choice,
            needs_review,
            frozen,
        } => {
            let choice = match choice {
                notryx_client_store::ContentConflictChoiceKind::KeepLocalCopy => {
                    notryx_protocol::WorkspaceConflictChoice::KeepLocalCopy
                }
                notryx_client_store::ContentConflictChoiceKind::ChooseRemote => {
                    notryx_protocol::WorkspaceConflictChoice::ChooseRemote
                }
                notryx_client_store::ContentConflictChoiceKind::SaveMerged => {
                    notryx_protocol::WorkspaceConflictChoice::SaveMerged
                }
            };
            notryx_protocol::WorkspaceConflictResolutionState::Pending {
                version: version.to_string(),
                generation: generation.to_string(),
                choice,
                needs_review,
                frozen,
            }
        }
        notryx_client_store::ContentConflictResolutionState::Completed { generation, frozen } => {
            notryx_protocol::WorkspaceConflictResolutionState::Completed {
                generation: generation.to_string(),
                frozen,
            }
        }
    }
}

async fn resolution_state_for_document(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
) -> Result<notryx_client_store::ContentConflictResolutionState, String> {
    let operation = state
        .store
        .read_outbox_operation(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if operation.document_id != document_id
        || operation.frozen
        || operation.status != notryx_client_store::OutboxStatus::Conflict
    {
        return Err("stale_conflict".into());
    }
    let result = state
        .store
        .read_content_conflict_resolution_state(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?;
    if matches!(
        result,
        notryx_client_store::ContentConflictResolutionState::Pending { frozen: true, .. }
            | notryx_client_store::ContentConflictResolutionState::Completed { frozen: true, .. }
    ) {
        return Err("stale_registration".into());
    }
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_content_conflict_resolution_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceConflictResolutionRequest,
) -> Result<notryx_protocol::WorkspaceConflictResolutionResponse, String> {
    crate::check_caller(&webview)?;
    let binding = crate::from_contract(&request.binding);
    let result = read_resolution_for_renderer(
        &runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.operation_id,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[cfg(test)]
mod resolution_status_tests {
    include!("conflict_resolution_status_native_tests.rs");
}

#[allow(dead_code)]
async fn choose_remote_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictChooseRemoteRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    choose_remote_for_renderer_inner(
        runtime,
        request,
        #[cfg(test)]
        None,
    )
    .await
}

#[tauri::command]
pub(crate) async fn workspace_choose_remote_content_conflict(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictChooseRemoteRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    crate::check_caller(&webview)?;
    let result = choose_remote_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

async fn choose_remote_for_renderer_inner(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictChooseRemoteRequest,
    #[cfg(test)] hook: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    if let Some(version) = request.expected_intent_version.as_deref() {
        let version = version
            .parse::<u64>()
            .map_err(|_| "invalid_intent_version")?;
        return reconsider_remote_for_renderer(runtime, request, version).await;
    }
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let binding = crate::from_contract(&request.binding);
    let gate = {
        let mut state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding {
            return Err("stale_registration".into());
        }
        if !session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        ) {
            return Err("stale_session".into());
        }
        let current = state
            .store
            .read_content_conflict_intent(&binding, &request.operation_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(intent) = current
            && (intent.version != 0
                || intent.document_id != request.document_id
                || intent.generation != generation
                || intent.choice != notryx_client_store::ContentConflictChoice::ChooseRemote
                || intent.base_revision != request.base_revision
                || intent.remote_revision != request.remote_revision
                || intent.remote_hash != request.remote_hash
                || !matches_displayed_hash(&request.base_hash, &intent.base_bytes)
                || !matches_displayed_hash(&request.local_hash, &intent.local_bytes))
        {
            return Err("stale_conflict".into());
        }
        gate
    };
    let result = choose_remote_after_confirmation(
        runtime,
        &request.renderer_session_id,
        &request.document_id,
        &request.edit_session_id,
        generation,
        &request.operation_id,
        &request.remote_revision,
        &request.remote_hash,
        &request.base_hash,
        &request.local_hash,
        &request.base_revision,
        &request.conflict_revision,
    )
    .await;
    let completed = match result {
        Ok(()) => true,
        Err(error) if error == "pending_conflict_resolution" => false,
        Err(error) => return Err(error),
    };
    #[cfg(test)]
    if let Some((ready, resume)) = hook {
        ready.notify_one();
        resume.notified().await;
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != binding {
        return Err("stale_registration".into());
    }
    if !completed {
        state
            .gate
            .check_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        if !session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        ) {
            return Err("stale_session".into());
        }
    }
    let intent = state
        .store
        .read_content_conflict_intent(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if intent.version != 0
        || intent.document_id != request.document_id
        || intent.generation != generation
        || intent.choice != notryx_client_store::ContentConflictChoice::ChooseRemote
        || intent.base_revision != request.base_revision
        || intent.remote_revision != request.remote_revision
        || intent.remote_hash != request.remote_hash
        || !matches_displayed_hash(&request.base_hash, &intent.base_bytes)
        || !matches_displayed_hash(&request.local_hash, &intent.local_bytes)
    {
        return Err("stale_conflict".into());
    }
    let original = state
        .store
        .read_outbox_operation(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if original.document_id != request.document_id
        || original.conflict_revision.as_deref() != Some(request.conflict_revision.as_str())
    {
        return Err("stale_conflict".into());
    }
    let stored = state
        .store
        .read_content_conflict_resolution_state(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())?;
    match stored {
        notryx_client_store::ContentConflictResolutionState::Pending {
            version: 0,
            generation: stored_generation,
            choice: notryx_client_store::ContentConflictChoiceKind::ChooseRemote,
            needs_review: false,
            frozen: false,
        } if !completed && stored_generation == generation => Ok(
            notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Pending {
                version: "0".into(),
            },
        ),
        notryx_client_store::ContentConflictResolutionState::Completed {
            generation: stored_generation,
            frozen: false,
        } if completed && generation.checked_add(1) == Some(stored_generation) => Ok(
            notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Completed {
                generation: stored_generation.to_string(),
            },
        ),
        _ => Err("stale_conflict".into()),
    }
}

async fn reconsider_remote_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictChooseRemoteRequest,
    expected_version: u64,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let binding = crate::from_contract(&request.binding);
    let gate = {
        let state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding {
            return Err("stale_registration".into());
        }
        if !session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        ) {
            return Err("stale_session".into());
        }
        gate
    };
    choose_remote_after_reconsideration(
        runtime,
        &request.renderer_session_id,
        &request.document_id,
        &request.edit_session_id,
        generation,
        &request.operation_id,
        expected_version,
        &request.remote_revision,
        &request.remote_hash,
        &request.base_hash,
        &request.local_hash,
        &request.base_revision,
        &request.conflict_revision,
    )
    .await?;
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    state
        .gate
        .check_session(&request.edit_session_id)
        .map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != binding {
        return Err("stale_registration".into());
    }
    if !session_matches(
        &state,
        &binding,
        &request.document_id,
        &request.edit_session_id,
    ) {
        return Err("stale_session".into());
    }
    let intent = state
        .store
        .read_content_conflict_intent(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if !(intent.version == expected_version
        || expected_version.checked_add(1) == Some(intent.version))
        || intent.document_id != request.document_id
        || intent.generation != generation
        || intent.choice != notryx_client_store::ContentConflictChoice::ChooseRemote
        || intent.base_revision != request.base_revision
        || intent.remote_revision != request.remote_revision
        || intent.remote_hash != request.remote_hash
        || !matches_displayed_hash(&request.base_hash, &intent.base_bytes)
        || !matches_displayed_hash(&request.local_hash, &intent.local_bytes)
    {
        return Err("stale_conflict".into());
    }
    let original = state
        .store
        .read_outbox_operation(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if original.document_id != request.document_id
        || original.conflict_revision.as_deref() != Some(request.conflict_revision.as_str())
    {
        return Err("stale_conflict".into());
    }
    let status = state
        .store
        .read_content_conflict_resolution_state(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())?;
    match status {
        notryx_client_store::ContentConflictResolutionState::Pending {
            version,
            generation: stored_generation,
            choice: notryx_client_store::ContentConflictChoiceKind::ChooseRemote,
            needs_review: false,
            frozen: false,
        } if version == intent.version && stored_generation == generation => Ok(
            notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Pending {
                version: version.to_string(),
            },
        ),
        _ => Err("stale_conflict".into()),
    }
}

#[cfg(test)]
async fn choose_remote_for_renderer_with_hook(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictChooseRemoteRequest,
    ready: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    choose_remote_for_renderer_inner(runtime, request, Some((ready, resume))).await
}

#[cfg(test)]
mod choose_remote_command_tests {
    include!("conflict_choose_remote_command_tests.rs");
}

#[cfg(test)]
mod choose_remote_return_session_tests {
    include!("conflict_choose_remote_return_session_tests.rs");
}

#[cfg(test)]
mod choose_remote_first_aba_tests {
    include!("conflict_choose_remote_first_aba_tests.rs");
}

#[cfg(test)]
mod choose_remote_reconsideration_tests {
    include!("conflict_choose_remote_reconsideration_tests.rs");
}

#[allow(dead_code)]
async fn continue_remote_choice_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    expected_version: u64,
) -> Result<notryx_protocol::WorkspaceContentConflictProgressResponse, String> {
    continue_remote_choice_inner(
        runtime,
        owner,
        binding,
        document_id,
        edit_session_id,
        operation_id,
        expected_version,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn continue_remote_choice_for_renderer_with_hook(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    expected_version: u64,
    ready: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
) -> Result<notryx_protocol::WorkspaceContentConflictProgressResponse, String> {
    continue_remote_choice_inner(
        runtime,
        owner,
        binding,
        document_id,
        edit_session_id,
        operation_id,
        expected_version,
        Some((ready, resume)),
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn continue_remote_choice_inner(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    expected_version: u64,
    #[cfg(test)] hook: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<notryx_protocol::WorkspaceContentConflictProgressResponse, String> {
    let (gate, session, response_generation, intent) = {
        let mut state = runtime.0.lock().await;
        let gate = state.gate.snapshot(owner).map_err(str::to_owned)?;
        state
            .gate
            .check_session(edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *binding {
            return Err("stale_registration".into());
        }
        if !session_matches(&state, binding, document_id, edit_session_id) {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(binding);
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let intent = state
            .store
            .read_content_conflict_intent(binding, operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_conflict")?;
        if intent.document_id != document_id
            || intent.version != expected_version
            || intent.choice != notryx_client_store::ContentConflictChoice::ChooseRemote
        {
            return Err("stale_conflict".into());
        }
        let local = state
            .store
            .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_session")?;
        if local.checkpoint_generation != Some(intent.generation)
            || local.base_revision != intent.base_revision
        {
            return Err("stale_checkpoint".into());
        }
        (gate, session, response_generation, intent)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&gate),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    #[cfg(test)]
    if let Some((ready, resume)) = hook {
        ready.notify_one();
        resume.notified().await;
    }
    let mut state = sync_bootstrap::checked_state(runtime, &gate, &session).await?;
    if registration_binding(session.binding()) != *binding
        || !state
            .transmission_gate
            .response_trusted(binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    if !session_matches(&state, binding, document_id, edit_session_id) {
        return Err("stale_session".into());
    }
    let current = state
        .store
        .read_content_conflict_intent(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if current != intent {
        return Err("stale_conflict".into());
    }
    let local = state
        .store
        .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_session")?;
    if local.checkpoint_generation != Some(intent.generation)
        || local.base_revision != intent.base_revision
    {
        return Err("stale_checkpoint".into());
    }
    let requested = state
        .store
        .request_content_conflict_followup_abandon(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(
        notryx_protocol::WorkspaceContentConflictProgressResponse::Pending {
            version: expected_version.to_string(),
            requested_operation_id: requested.map(|item| item.operation_id),
        },
    )
}

#[tauri::command]
pub(crate) async fn workspace_confirm_content_conflict_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictCopyRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyResponse, String> {
    crate::check_caller(&webview)?;
    let result = confirm_copy_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

async fn confirm_copy_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictCopyRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyResponse, String> {
    let selection = request.selection;
    let generation = selection
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let expected_version = selection
        .expected_intent_version
        .as_deref()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&selection.binding);
    let (gate, response_generation) = {
        let state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&selection.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&selection.edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding {
            return Err("stale_registration".into());
        }
        if !session_matches(
            &state,
            &binding,
            &selection.document_id,
            &selection.edit_session_id,
        ) {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        (gate, response_generation)
    };
    let selected = prepare_conflict_snapshot(
        runtime,
        &selection.renderer_session_id,
        &selection.document_id,
        &selection.edit_session_id,
        generation,
        &selection.operation_id,
    )
    .await?;
    if selected.binding != binding
        || selected.conflict_revision != selection.conflict_revision
        || selected.base_revision != selection.base_revision
        || !matches_displayed_hash(&selection.base_hash, &selected.base_bytes)
        || !matches_displayed_hash(&selection.local_hash, &selected.local_bytes)
        || selected.remote_revision != selection.remote_revision
        || selected.remote_node.content_hash.as_deref() != Some(selection.remote_hash.as_str())
    {
        return Err("stale_conflict".into());
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    state
        .gate
        .check_session(&selection.edit_session_id)
        .map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != binding
        || !session_matches(
            &state,
            &binding,
            &selection.document_id,
            &selection.edit_session_id,
        )
    {
        return Err("stale_session".into());
    }
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    let local = checked_local(
        &mut state,
        &binding,
        &selection.document_id,
        &selection.edit_session_id,
        generation,
        &selection.operation_id,
    )
    .await?;
    if local.operation_seq != selected.operation_seq
        || local.local != selected.local_bytes
        || local.base != selected.base_bytes
        || local.base_revision != selected.base_revision
        || local.conflict_revision != selected.conflict_revision
        || !state
            .store
            .remote_content_matches(&binding, &selected.remote_node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err("stale_conflict".into());
    }
    let intent_request = notryx_client_store::ContentConflictIntentRequest {
        generation,
        remote_revision: selected.remote_revision,
        remote_hash: selection.remote_hash,
        remote_bytes: selected.remote_bytes,
        choice: notryx_client_store::ContentConflictChoice::KeepLocalCopy,
    };
    let copy = state
        .store
        .record_content_conflict_copy(
            &binding,
            &selection.document_id,
            &selection.operation_id,
            &intent_request,
            &request.parent_id,
            &request.name,
            expected_version,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(
        notryx_protocol::WorkspaceContentConflictCopyResponse::Pending {
            version: copy.intent_version.to_string(),
            node_id: copy.node_id,
            create_operation_id: copy.create_operation_id,
        },
    )
}

#[tauri::command]
pub(crate) async fn workspace_confirm_content_conflict_merge(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictChooseRemoteRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    crate::check_caller(&webview)?;
    let result = confirm_merge_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

async fn confirm_merge_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictChooseRemoteRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    use notryx_client_store::{ContentConflictChoice, ContentConflictIntentRequest};
    use notryx_protocol::WorkspaceContentConflictChooseRemoteResponse as Response;

    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let expected_version = request
        .expected_intent_version
        .as_deref()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&request.binding);
    let (gate, response_generation, expected) = {
        let mut state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding
            || !session_matches(
                &state,
                &binding,
                &request.document_id,
                &request.edit_session_id,
            )
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
        let current = state
            .store
            .read_content_conflict_intent(&binding, &request.operation_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(intent) = &current {
            let operation = state
                .store
                .read_outbox_operation(&binding, &request.operation_id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or("stale_conflict")?;
            let same = intent.document_id == request.document_id
                && intent.generation == generation
                && intent.base_revision == request.base_revision
                && intent.remote_revision == request.remote_revision
                && intent.remote_hash == request.remote_hash
                && operation.document_id == request.document_id
                && operation.conflict_revision.as_deref()
                    == Some(request.conflict_revision.as_str())
                && matches_displayed_hash(&request.base_hash, &intent.base_bytes)
                && matches_displayed_hash(&request.local_hash, &intent.local_bytes)
                && matches_displayed_hash(&request.remote_hash, &intent.remote_bytes)
                && intent.merged_hash.as_deref() == Some(request.local_hash.as_str())
                && matches!(&intent.choice, ContentConflictChoice::SaveMerged { bytes } if *bytes == intent.local_bytes)
                && !intent.local_bytes.contains(&b'\r');
            let retry_version = match expected_version {
                None => intent.version == 0,
                Some(version) if intent.version == version => true,
                Some(version) if version.checked_add(1) == Some(intent.version) => state
                    .store
                    .read_content_conflict_intent_history(&binding, &request.operation_id, version)
                    .await
                    .map_err(|error| error.to_string())?
                    .is_some(),
                Some(_) => false,
            };
            if same && retry_version {
                let resolution = state
                    .store
                    .read_content_conflict_resolution_state(&binding, &request.operation_id)
                    .await
                    .map_err(|error| error.to_string())?;
                return match resolution {
                    notryx_client_store::ContentConflictResolutionState::Pending {
                        version,
                        ..
                    } => Ok(Response::Pending {
                        version: version.to_string(),
                    }),
                    notryx_client_store::ContentConflictResolutionState::Completed {
                        generation,
                        ..
                    } => Ok(Response::Completed {
                        generation: generation.to_string(),
                    }),
                    _ => Err("stale_conflict".into()),
                };
            }
        }
        let expected = match (expected_version, current) {
            (None, None) => None,
            (None, Some(_)) | (Some(_), None) => return Err("stale_conflict".into()),
            (Some(version), Some(current)) if current.version == version => Some(current),
            (Some(version), Some(current)) if version.checked_add(1) == Some(current.version) => {
                Some(
                    state
                        .store
                        .read_content_conflict_intent_history(
                            &binding,
                            &request.operation_id,
                            version,
                        )
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or("stale_conflict")?,
                )
            }
            (Some(_), Some(_)) => return Err("stale_conflict".into()),
        };
        (gate, response_generation, expected)
    };

    let selected = prepare_conflict_snapshot(
        runtime,
        &request.renderer_session_id,
        &request.document_id,
        &request.edit_session_id,
        generation,
        &request.operation_id,
    )
    .await?;
    if selected.binding != binding
        || selected.conflict_revision != request.conflict_revision
        || selected.base_revision != request.base_revision
        || !matches_displayed_hash(&request.base_hash, &selected.base_bytes)
        || !matches_displayed_hash(&request.local_hash, &selected.local_bytes)
        || selected.local_bytes.contains(&b'\r')
        || selected.remote_revision != request.remote_revision
        || selected.remote_node.content_hash.as_deref() != Some(request.remote_hash.as_str())
    {
        return Err("stale_conflict".into());
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    state
        .gate
        .check_session(&request.edit_session_id)
        .map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != binding
        || !session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        )
    {
        return Err("stale_session".into());
    }
    if !state
        .transmission_gate
        .response_trusted(&binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    let local = checked_local(
        &mut state,
        &binding,
        &request.document_id,
        &request.edit_session_id,
        generation,
        &request.operation_id,
    )
    .await?;
    if local.operation_seq != selected.operation_seq
        || local.local != selected.local_bytes
        || local.base != selected.base_bytes
        || local.base_revision != selected.base_revision
        || local.conflict_revision != selected.conflict_revision
        || !state
            .store
            .remote_content_matches(&binding, &selected.remote_node)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err("stale_conflict".into());
    }
    let intent_request = ContentConflictIntentRequest {
        generation,
        remote_revision: selected.remote_revision,
        remote_hash: request.remote_hash,
        remote_bytes: selected.remote_bytes,
        choice: ContentConflictChoice::SaveMerged {
            bytes: selected.local_bytes,
        },
    };
    let intent = match expected {
        None => {
            state
                .store
                .record_content_conflict_intent(
                    &binding,
                    &request.document_id,
                    &request.operation_id,
                    &intent_request,
                )
                .await
        }
        Some(previous) => {
            state
                .store
                .replace_pending_content_conflict_intent(
                    &binding,
                    &request.document_id,
                    &request.operation_id,
                    &previous,
                    &intent_request,
                )
                .await
        }
    }
    .map_err(|error| error.to_string())?;
    Ok(Response::Pending {
        version: intent.version.to_string(),
    })
}

#[tauri::command]
pub(crate) async fn workspace_queue_content_conflict_merge(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictProgressRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictProgressResponse, String> {
    crate::check_caller(&webview)?;
    let version = request
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&request.binding);
    let queued = queue_merge_for_renderer(
        &runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.edit_session_id,
        &request.operation_id,
        version,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(
        notryx_protocol::WorkspaceContentConflictProgressResponse::Pending {
            version: version.to_string(),
            requested_operation_id: Some(queued.operation_id),
        },
    )
}

#[tauri::command]
pub(crate) async fn workspace_content_conflict_merge_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictMergeStatusRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictMergeStatusResponse, String> {
    crate::check_caller(&webview)?;
    let version = request
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&request.binding);
    let queued = read_queued_merge_for_renderer(
        &runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.operation_id,
        version,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(
        notryx_protocol::WorkspaceContentConflictMergeStatusResponse {
            binding: request.binding,
            document_id: request.document_id,
            operation_id: request.operation_id,
            intent_version: version.to_string(),
            merge_operation: queued.map(|item| {
                notryx_protocol::WorkspaceContentConflictMergeOperation {
                    operation_id: item.operation_id,
                    operation_seq: item.operation_seq.to_string(),
                }
            }),
        },
    )
}

async fn read_queued_merge_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    expected_version: u64,
) -> Result<Option<notryx_client_store::OutboxOperation>, String> {
    use notryx_client_store::ContentConflictChoice;
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(binding)
    {
        return Err("stale_registration".into());
    }
    let original = state
        .store
        .read_outbox_operation(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if original.document_id != document_id
        || original.frozen
        || original.status != notryx_client_store::OutboxStatus::Conflict
    {
        return Err("stale_conflict".into());
    }
    let intent = state
        .store
        .read_content_conflict_intent(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if intent.document_id != document_id
        || intent.version != expected_version
        || !matches!(&intent.choice, ContentConflictChoice::SaveMerged { bytes } if *bytes == intent.local_bytes)
        || !intent
            .merged_hash
            .as_deref()
            .is_some_and(|hash| matches_displayed_hash(hash, &intent.local_bytes))
        || intent.local_bytes.contains(&b'\r')
    {
        return Err("stale_conflict".into());
    }
    let queued = state
        .store
        .read_queued_content_conflict_merge(binding, operation_id, expected_version)
        .await
        .map_err(|error| error.to_string())?;
    if queued
        .as_ref()
        .is_some_and(|item| item.frozen || item.document_id != document_id)
    {
        return Err("stale_conflict".into());
    }
    Ok(queued)
}

#[tauri::command]
pub(crate) async fn workspace_content_conflict_merge_followup_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictMergeStatusRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictMergeFollowupStatusResponse, String> {
    crate::check_caller(&webview)?;
    let version = request
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&request.binding);
    let followup = inspect_merge_followup_for_renderer(
        &runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.operation_id,
        version,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(
        notryx_protocol::WorkspaceContentConflictMergeFollowupStatusResponse {
            binding: request.binding,
            document_id: request.document_id,
            operation_id: request.operation_id,
            intent_version: version.to_string(),
            followup: followup.map(
                |item| notryx_protocol::WorkspaceContentConflictMergeFollowup {
                    operation_id: item.operation_id,
                    operation_seq: item.operation_seq.to_string(),
                    generation: item.generation.to_string(),
                    payload_hash: item.payload_hash,
                },
            ),
        },
    )
}

async fn inspect_merge_followup_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    version: u64,
) -> Result<Option<notryx_client_store::OutboxOperation>, String> {
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(binding)
    {
        return Err("stale_registration".into());
    }
    let intent = state
        .store
        .read_content_conflict_intent(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if intent.document_id != document_id
        || intent.version != version
        || !matches!(&intent.choice, notryx_client_store::ContentConflictChoice::SaveMerged { bytes } if *bytes == intent.local_bytes)
    {
        return Err("stale_conflict".into());
    }
    let item = state
        .store
        .inspect_content_conflict_merge_followup(binding, operation_id, version)
        .await
        .map_err(|error| error.to_string())?;
    if item
        .as_ref()
        .is_some_and(|item| item.frozen || item.document_id != document_id)
    {
        return Err("stale_conflict".into());
    }
    Ok(item)
}

#[tauri::command]
pub(crate) async fn workspace_request_content_conflict_merge_followup_abandon(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictMergeFollowupApprovalRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictProgressResponse, String> {
    crate::check_caller(&webview)?;
    let progress = request.progress;
    let version = progress
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    if version > i64::MAX as u64 || progress.expected_intent_version != version.to_string() {
        return Err("invalid_intent_version".into());
    }
    let sequence = request
        .followup
        .operation_seq
        .parse::<u64>()
        .map_err(|_| "invalid_operation_seq")?;
    let generation = request
        .followup
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let binding = crate::from_contract(&progress.binding);
    let requested = approve_merge_followup_for_renderer(
        &runtime,
        &progress.renderer_session_id,
        &binding,
        &progress.document_id,
        &progress.edit_session_id,
        &progress.operation_id,
        version,
        &request.followup.operation_id,
        sequence,
        generation,
        &request.followup.payload_hash,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(
        notryx_protocol::WorkspaceContentConflictProgressResponse::Pending {
            version: version.to_string(),
            requested_operation_id: Some(requested.operation_id),
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn approve_merge_followup_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    version: u64,
    followup_id: &str,
    followup_seq: u64,
    followup_generation: u64,
    followup_hash: &str,
) -> Result<notryx_client_store::OutboxOperation, String> {
    let (gate, session, response_generation, intent) = {
        let mut state = runtime.0.lock().await;
        let gate = state.gate.snapshot(owner).map_err(str::to_owned)?;
        state
            .gate
            .check_session(edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *binding
            || !session_matches(&state, binding, document_id, edit_session_id)
        {
            return Err("stale_session".into());
        }
        let intent = state
            .store
            .read_content_conflict_intent(binding, operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_conflict")?;
        if intent.document_id != document_id
            || intent.version != version
            || !matches!(&intent.choice, notryx_client_store::ContentConflictChoice::SaveMerged { bytes } if *bytes == intent.local_bytes)
        {
            return Err("stale_conflict".into());
        }
        let prior = state
            .store
            .read_outbox_operation(binding, followup_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_followup")?;
        if prior.document_id != document_id
            || prior.operation_seq != followup_seq
            || prior.generation != followup_generation
            || prior.payload_hash != followup_hash
            || prior.frozen
        {
            return Err("stale_followup".into());
        }
        if prior.abandon_state != notryx_client_store::OutboxAbandonState::None
            || prior.status == notryx_client_store::OutboxStatus::Abandoned
        {
            return state
                .store
                .request_content_conflict_merge_followup(
                    binding,
                    operation_id,
                    version,
                    followup_id,
                    followup_seq,
                    followup_generation,
                    followup_hash,
                )
                .await
                .map_err(|error| error.to_string());
        }
        let response_generation = state.transmission_gate.response_generation(binding);
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        (gate, session, response_generation, intent)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&gate),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &gate, &session).await?;
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    if registration_binding(session.binding()) != *binding
        || !session_matches(&state, binding, document_id, edit_session_id)
        || !state
            .transmission_gate
            .response_trusted(binding, response_generation)
    {
        return Err("stale_session".into());
    }
    let current = state
        .store
        .read_content_conflict_intent(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if current != intent {
        return Err("stale_conflict".into());
    }
    state
        .store
        .request_content_conflict_merge_followup(
            binding,
            operation_id,
            version,
            followup_id,
            followup_seq,
            followup_generation,
            followup_hash,
        )
        .await
        .map_err(|error| error.to_string())
}

async fn queue_merge_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    expected_version: u64,
) -> Result<notryx_client_store::OutboxOperation, String> {
    use notryx_client_store::ContentConflictChoice;
    let (gate, session, response_generation, intent) = {
        let mut state = runtime.0.lock().await;
        let gate = state.gate.snapshot(owner).map_err(str::to_owned)?;
        state
            .gate
            .check_session(edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *binding {
            return Err("stale_registration".into());
        }
        if !session_matches(&state, binding, document_id, edit_session_id) {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(binding);
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let intent = state
            .store
            .read_content_conflict_intent(binding, operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_conflict")?;
        if intent.document_id != document_id
            || intent.version != expected_version
            || !matches!(&intent.choice, ContentConflictChoice::SaveMerged { bytes } if *bytes == intent.local_bytes)
            || !intent
                .merged_hash
                .as_deref()
                .is_some_and(|hash| matches_displayed_hash(hash, &intent.local_bytes))
            || intent.local_bytes.contains(&b'\r')
        {
            return Err("stale_conflict".into());
        }
        let local = checked_local(
            &mut state,
            binding,
            document_id,
            edit_session_id,
            intent.generation,
            operation_id,
        )
        .await?;
        if local.local != intent.local_bytes
            || local.base != intent.base_bytes
            || local.base_revision != intent.base_revision
        {
            return Err("stale_checkpoint".into());
        }
        (gate, session, response_generation, intent)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&gate),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &gate, &session).await?;
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    if registration_binding(session.binding()) != *binding
        || !session_matches(&state, binding, document_id, edit_session_id)
        || !state
            .transmission_gate
            .response_trusted(binding, response_generation)
    {
        return Err("stale_session".into());
    }
    let current = state
        .store
        .read_content_conflict_intent(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if current != intent {
        return Err("stale_conflict".into());
    }
    let local = checked_local(
        &mut state,
        binding,
        document_id,
        edit_session_id,
        intent.generation,
        operation_id,
    )
    .await?;
    if local.local != intent.local_bytes
        || local.base != intent.base_bytes
        || local.base_revision != intent.base_revision
    {
        return Err("stale_checkpoint".into());
    }
    let mut closing = CompletionSessionGuard {
        state: &mut state,
        edit_session_id,
        armed: true,
    };
    let result = closing
        .state
        .store
        .queue_content_conflict_merge_closing_session(
            binding,
            document_id,
            edit_session_id,
            operation_id,
            expected_version,
        )
        .await;
    let queued = match result {
        Ok(queued) => queued,
        Err(error) => {
            if closing
                .state
                .store
                .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
                .await
                .is_ok_and(|session| session.is_some())
            {
                closing.armed = false;
            }
            return Err(error.to_string());
        }
    };
    closing.state.product_sessions.remove(edit_session_id);
    closing
        .state
        .gate
        .close_session(edit_session_id)
        .map_err(str::to_owned)?;
    closing.armed = false;
    Ok(queued)
}

#[cfg(test)]
mod copy_confirm_tests {
    include!("conflict_copy_confirm_native_tests.rs");
}

#[cfg(test)]
mod merge_confirm_tests {
    include!("conflict_merge_confirm_native_tests.rs");
}

#[cfg(test)]
mod merge_reconfirm_tests {
    include!("conflict_merge_reconfirm_native_tests.rs");
}

#[cfg(test)]
mod merge_queue_tests {
    include!("conflict_merge_queue_native_tests.rs");
}

#[cfg(test)]
mod merge_recovery_tests {
    include!("conflict_merge_recovery_native_tests.rs");
}

#[cfg(test)]
mod merge_discovery_tests {
    include!("conflict_merge_discovery_native_tests.rs");
}

#[cfg(test)]
mod merge_followup_tests {
    include!("conflict_merge_followup_native_tests.rs");
}

#[cfg(test)]
mod merge_status_additional_tests {
    include!("conflict_merge_status_additional_native_tests.rs");
}

#[tauri::command]
pub(crate) async fn workspace_progress_content_conflict_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictCopyProgressRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyProgressResponse, String> {
    crate::check_caller(&webview)?;
    let result = progress_copy_with_approval_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

async fn progress_copy_with_approval_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictCopyProgressRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyProgressResponse, String> {
    use notryx_protocol::WorkspaceContentConflictCopyProgressResponse as Response;
    let approved = request.approved_followup_operation_id;
    let request = request.progress;
    let expected_version = request
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&request.binding);
    let (gate, session, response_generation) = {
        let state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding {
            return Err("stale_registration".into());
        }
        if !session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        ) {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        (gate, session, response_generation)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&gate),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &gate, &session).await?;
    state
        .gate
        .check_session(&request.edit_session_id)
        .map_err(str::to_owned)?;
    if registration_binding(session.binding()) != binding
        || !session_matches(
            &state,
            &binding,
            &request.document_id,
            &request.edit_session_id,
        )
        || !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
    {
        return Err("stale_session".into());
    }
    let copy = state
        .store
        .read_content_conflict_copy(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if copy.document_id != request.document_id || copy.intent_version != expected_version {
        return Err("stale_conflict".into());
    }
    match state
        .store
        .read_content_conflict_copy_content_state(&binding, &request.operation_id, expected_version)
        .await
        .map_err(|error| error.to_string())?
    {
        notryx_client_store::ContentConflictCopyContentState::Saved { .. } => {
            if approved.is_some() {
                return Err("stale_conflict".into());
            }
            return Ok(Response::Saved {});
        }
        notryx_client_store::ContentConflictCopyContentState::NeedsReview { .. } => {
            if approved.is_some() {
                return Err("stale_conflict".into());
            }
            return Ok(Response::NeedsReview {});
        }
        notryx_client_store::ContentConflictCopyContentState::Pending { .. } => {
            if approved.is_some() {
                return Err("stale_conflict".into());
            }
            return Ok(Response::Pending {
                requested_operation_id: None,
            });
        }
        notryx_client_store::ContentConflictCopyContentState::NotQueued => {}
    }
    let followup = state
        .store
        .inspect_content_conflict_copy_followup(&binding, &request.operation_id, expected_version)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(item) = followup {
        let Some(approved) = approved else {
            return Ok(Response::ApprovalRequired {
                followup_operation_id: item.operation_id,
            });
        };
        if approved != item.operation_id {
            return Err("stale_conflict".into());
        }
        let item = state
            .store
            .request_content_conflict_copy_followup(
                &binding,
                &request.operation_id,
                expected_version,
                &approved,
            )
            .await
            .map_err(|error| error.to_string())?;
        return Ok(Response::Pending {
            requested_operation_id: Some(item.operation_id),
        });
    }
    if approved.is_some() {
        return Err("stale_conflict".into());
    }
    let create = state
        .store
        .queue_content_conflict_copy(&binding, &request.operation_id, expected_version)
        .await
        .map_err(|error| error.to_string())?;
    match create.status {
        notryx_client_store::OutboxStatus::Queued | notryx_client_store::OutboxStatus::Sent => {
            Ok(Response::Pending {
                requested_operation_id: None,
            })
        }
        notryx_client_store::OutboxStatus::Committed => {
            let confirmed = state
                .store
                .confirmed_receipt_through(&binding)
                .await
                .map_err(|error| error.to_string())?;
            if confirmed >= create.operation_seq {
                state
                    .store
                    .queue_content_conflict_copy_content(
                        &binding,
                        &request.operation_id,
                        expected_version,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(Response::Pending {
                requested_operation_id: None,
            })
        }
        notryx_client_store::OutboxStatus::Conflict
        | notryx_client_store::OutboxStatus::Failed
        | notryx_client_store::OutboxStatus::Abandoned
        | notryx_client_store::OutboxStatus::Abandoning => Ok(Response::NeedsReview {}),
    }
}

#[cfg(test)]
mod copy_progress_tests {
    include!("conflict_copy_progress_native_tests.rs");
}

#[tauri::command]
pub(crate) async fn workspace_content_conflict_copy_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceConflictResolutionRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyStatusResponse, String> {
    crate::check_caller(&webview)?;
    let result = copy_status_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

async fn copy_status_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceConflictResolutionRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyStatusResponse, String> {
    copy_status_inner(
        runtime,
        request,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
async fn copy_status_for_renderer_with_hook(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceConflictResolutionRequest,
    ready: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyStatusResponse, String> {
    copy_status_inner(runtime, request, Some((ready, resume))).await
}

async fn copy_status_inner(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceConflictResolutionRequest,
    #[cfg(test)] hook: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyStatusResponse, String> {
    let binding = crate::from_contract(&request.binding);
    let (gate, first) = {
        let mut state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding {
            return Err("stale_registration".into());
        }
        let first = copy_status_from_state(
            &mut state,
            &binding,
            &request.document_id,
            &request.operation_id,
        )
        .await?;
        (gate, first)
    };
    #[cfg(test)]
    if let Some((ready, resume)) = hook {
        ready.notify_one();
        resume.notified().await;
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != binding {
        return Err("stale_registration".into());
    }
    let current = copy_status_from_state(
        &mut state,
        &binding,
        &request.document_id,
        &request.operation_id,
    )
    .await?;
    if current != first {
        return Err("stale_conflict".into());
    }
    Ok(
        notryx_protocol::WorkspaceContentConflictCopyStatusResponse {
            binding: crate::to_contract(binding),
            document_id: request.document_id,
            operation_id: request.operation_id,
            copy: current,
        },
    )
}

async fn copy_status_from_state(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
) -> Result<Option<notryx_protocol::WorkspaceContentConflictCopyStatus>, String> {
    use notryx_protocol::{
        WorkspaceContentConflictCopyContentStatus as Content,
        WorkspaceContentConflictCopyCreateStatus as Create,
    };
    let resolution =
        resolution_state_for_document(state, binding, document_id, operation_id).await?;
    let Some(copy) = state
        .store
        .read_content_conflict_copy(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    if copy.document_id != document_id {
        return Err("stale_conflict".into());
    }
    let content = match state
        .store
        .read_content_conflict_copy_content_state(binding, operation_id, copy.intent_version)
        .await
        .map_err(|error| error.to_string())?
    {
        notryx_client_store::ContentConflictCopyContentState::NotQueued => Content::NotQueued,
        notryx_client_store::ContentConflictCopyContentState::Pending { .. } => Content::Pending,
        notryx_client_store::ContentConflictCopyContentState::Saved { .. } => Content::Saved,
        notryx_client_store::ContentConflictCopyContentState::NeedsReview { .. } => {
            Content::NeedsReview
        }
    };
    let mut after = 0;
    let mut found = None;
    loop {
        let page = state
            .store
            .list_create_operations(binding, after, 100)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(entry) = page
            .entries
            .into_iter()
            .find(|entry| entry.operation.operation_id == copy.create_operation_id)
        {
            found = Some(entry);
            break;
        }
        match page.next_after_seq {
            Some(next) if next > after => after = next,
            Some(_) => return Err("invalid_sequence".into()),
            None => break,
        }
    }
    let (create, create_failure) = if let Some(entry) = found {
        let item = entry.operation;
        if item.request.node_id.to_string() != copy.node_id
            || item.request.parent_id.to_string() != copy.parent_id
            || item.request.name != copy.name
            || item.request.kind != notryx_protocol::NodeKind::File
        {
            return Err("stale_conflict".into());
        }
        let status = match item.status {
            notryx_client_store::OutboxStatus::Queued | notryx_client_store::OutboxStatus::Sent => {
                Create::Pending
            }
            notryx_client_store::OutboxStatus::Committed => {
                if state
                    .store
                    .confirmed_receipt_through(binding)
                    .await
                    .map_err(|error| error.to_string())?
                    >= item.operation_seq
                {
                    Create::Created
                } else {
                    Create::Pending
                }
            }
            notryx_client_store::OutboxStatus::Failed
            | notryx_client_store::OutboxStatus::Abandoned => {
                let ack = state
                    .store
                    .receipt_ack_through(binding)
                    .await
                    .map_err(|error| error.to_string())?;
                let confirmed = state
                    .store
                    .confirmed_receipt_through(binding)
                    .await
                    .map_err(|error| error.to_string())?;
                if ack < item.operation_seq {
                    return Err("stale_conflict".into());
                }
                if confirmed < item.operation_seq {
                    Create::Pending
                } else {
                    Create::NeedsReview
                }
            }
            _ => Create::NeedsReview,
        };
        let failure = if status == Create::NeedsReview {
            Some(
                if matches!(
                    entry.receipt,
                    Some(notryx_protocol::OperationReceipt::DestinationExists { .. })
                ) {
                    notryx_protocol::WorkspaceContentConflictCopyCreateFailure::DestinationExists
                } else {
                    notryx_protocol::WorkspaceContentConflictCopyCreateFailure::Other
                },
            )
        } else {
            None
        };
        (status, failure)
    } else {
        (Create::NotQueued, None)
    };
    if content != Content::NotQueued && create != Create::Created {
        return Err("stale_conflict".into());
    }
    let followup = if content == Content::NotQueued
        && matches!(resolution,
        notryx_client_store::ContentConflictResolutionState::Pending { version, choice: notryx_client_store::ContentConflictChoiceKind::KeepLocalCopy, needs_review: false, frozen: false, .. } if version == copy.intent_version)
    {
        state
            .store
            .inspect_content_conflict_copy_followup(binding, operation_id, copy.intent_version)
            .await
            .map_err(|error| error.to_string())?
            .map(|item| item.operation_id)
    } else {
        None
    };
    Ok(Some(notryx_protocol::WorkspaceContentConflictCopyStatus {
        version: copy.intent_version.to_string(),
        parent_id: copy.parent_id,
        name: copy.name,
        node_id: copy.node_id,
        create_operation_id: copy.create_operation_id,
        followup_operation_id: followup,
        create_status: create,
        create_failure,
        content_status: content,
    }))
}

#[cfg(test)]
mod copy_status_tests {
    include!("conflict_copy_status_native_tests.rs");
}

#[cfg(test)]
mod copy_origin_reconsider_tests {
    include!("conflict_copy_origin_reconsider_tests.rs");
}

#[cfg(test)]
mod copy_origin_repeat_tests {
    include!("conflict_copy_origin_repeat_tests.rs");
}

#[tauri::command]
pub(crate) async fn workspace_retry_content_conflict_copy(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictCopyRetryRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyResponse, String> {
    crate::check_caller(&webview)?;
    let result = retry_copy_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

async fn retry_copy_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceContentConflictCopyRetryRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictCopyResponse, String> {
    let progress = request.progress;
    let expected_version = progress
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&progress.binding);
    let (gate, session, response_generation) = {
        let state = runtime.0.lock().await;
        let gate = state
            .gate
            .snapshot(&progress.renderer_session_id)
            .map_err(str::to_owned)?;
        state
            .gate
            .check_session(&progress.edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != binding {
            return Err("stale_registration".into());
        }
        if !session_matches(
            &state,
            &binding,
            &progress.document_id,
            &progress.edit_session_id,
        ) {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        (gate, session, response_generation)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&gate),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    let mut state = sync_bootstrap::checked_state(runtime, &gate, &session).await?;
    state
        .gate
        .check_session(&progress.edit_session_id)
        .map_err(str::to_owned)?;
    if registration_binding(session.binding()) != binding
        || !session_matches(
            &state,
            &binding,
            &progress.document_id,
            &progress.edit_session_id,
        )
        || !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
    {
        return Err("stale_session".into());
    }
    let current = resolution_state_for_document(
        &mut state,
        &binding,
        &progress.document_id,
        &progress.operation_id,
    )
    .await?;
    if !matches!(current, notryx_client_store::ContentConflictResolutionState::Pending { version, choice: notryx_client_store::ContentConflictChoiceKind::KeepLocalCopy, needs_review: false, frozen: false, .. } if version == expected_version)
    {
        return Err("stale_conflict".into());
    }
    let copy = state
        .store
        .reconfirm_failed_content_conflict_copy(
            &binding,
            &progress.operation_id,
            expected_version,
            &request.expected_create_operation_id,
            &request.parent_id,
            &request.name,
        )
        .await
        .map_err(|error| error.to_string())?;
    if copy.document_id != progress.document_id {
        return Err("stale_conflict".into());
    }
    Ok(
        notryx_protocol::WorkspaceContentConflictCopyResponse::Pending {
            version: copy.intent_version.to_string(),
            node_id: copy.node_id,
            create_operation_id: copy.create_operation_id,
        },
    )
}

#[cfg(test)]
mod copy_retry_tests {
    include!("conflict_copy_retry_native_tests.rs");
}

#[cfg(test)]
mod copy_retry_history_corrupt_tests {
    include!("conflict_copy_retry_history_corrupt_tests.rs");
}

#[cfg(test)]
mod copy_retry_current_target_corrupt_tests {
    include!("conflict_copy_retry_current_target_corrupt_tests.rs");
}

#[cfg(test)]
mod copy_failure_reason_tests {
    include!("conflict_copy_failure_reason_tests.rs");
}

#[cfg(test)]
mod copy_approval_tests {
    include!("conflict_copy_approval_native_tests.rs");
}

#[cfg(test)]
mod continue_remote_tests {
    include!("conflict_continue_remote_tests.rs");
}

#[cfg(test)]
mod progress_operation_id_tests {
    include!("conflict_progress_operation_id_tests.rs");
}

#[allow(dead_code)]
async fn complete_pending_remote_choice_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    expected_version: u64,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    complete_pending_remote_choice_inner(
        runtime,
        owner,
        binding,
        document_id,
        edit_session_id,
        operation_id,
        expected_version,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn complete_pending_remote_choice_for_renderer_with_hook(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    expected_version: u64,
    phase: &str,
    ready: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    complete_pending_remote_choice_inner(
        runtime,
        owner,
        binding,
        document_id,
        edit_session_id,
        operation_id,
        expected_version,
        Some((phase, ready, resume)),
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn complete_pending_remote_choice_inner(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    edit_session_id: &str,
    operation_id: &str,
    expected_version: u64,
    #[cfg(test)] hook: Option<(
        &str,
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    let (gate, session, response_generation, intent) = {
        let mut state = runtime.0.lock().await;
        let gate = state.gate.snapshot(owner).map_err(str::to_owned)?;
        state
            .gate
            .check_session(edit_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *binding {
            return Err("stale_registration".into());
        }
        if !session_matches(&state, binding, document_id, edit_session_id) {
            return Err("stale_session".into());
        }
        let response_generation = state.transmission_gate.response_generation(binding);
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Err("stale_sync".into());
        }
        let intent = state
            .store
            .read_content_conflict_intent(binding, operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_conflict")?;
        if intent.document_id != document_id
            || intent.version != expected_version
            || intent.choice != notryx_client_store::ContentConflictChoice::ChooseRemote
        {
            return Err("stale_conflict".into());
        }
        let local = state
            .store
            .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("stale_session")?;
        if local.checkpoint_generation != Some(intent.generation)
            || local.base_revision != intent.base_revision
        {
            return Err("stale_checkpoint".into());
        }
        (gate, session, response_generation, intent)
    };
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&gate),
        &session,
        Some(response_generation),
    )
    .await
    .map_err(sync_catchup::CatchUpError::message)?;
    #[cfg(test)]
    if let Some(("before_store_completion", ready, resume)) = &hook {
        ready.notify_one();
        resume.notified().await;
    }
    let mut state = sync_bootstrap::checked_state(runtime, &gate, &session).await?;
    if registration_binding(session.binding()) != *binding
        || !state
            .transmission_gate
            .response_trusted(binding, response_generation)
    {
        return Err("stale_sync".into());
    }
    state
        .gate
        .check_session(edit_session_id)
        .map_err(str::to_owned)?;
    if !session_matches(&state, binding, document_id, edit_session_id) {
        return Err("stale_session".into());
    }
    let current = state
        .store
        .read_content_conflict_intent(binding, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_conflict")?;
    if current != intent {
        return Err("stale_conflict".into());
    }
    let local = state
        .store
        .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("stale_session")?;
    if local.checkpoint_generation != Some(intent.generation)
        || local.base_revision != intent.base_revision
    {
        return Err("stale_checkpoint".into());
    }
    let outbox = state
        .store
        .document_outbox_state(binding, document_id)
        .await
        .map_err(|error| error.to_string())?;
    let confirmed = state
        .store
        .confirmed_receipt_through(binding)
        .await
        .map_err(|error| error.to_string())?;
    if outbox.latest.as_ref().is_none_or(|item| {
        confirmed < item.operation_seq
            || (item.operation_id != operation_id
                && item.status != notryx_client_store::OutboxStatus::Abandoned)
    }) {
        return Err("pending_conflict_resolution".into());
    }
    let mut closing = CompletionSessionGuard {
        state: &mut state,
        edit_session_id,
        armed: true,
    };
    let completed = closing
        .state
        .store
        .complete_content_conflict_choose_remote_closing_session(
            binding,
            document_id,
            edit_session_id,
            operation_id,
        )
        .await;
    let result = match completed {
        Ok(result) => result,
        Err(error) => {
            if closing
                .state
                .store
                .dirty_registered_session_snapshot(binding, document_id, edit_session_id)
                .await
                .is_ok_and(|session| session.is_some())
            {
                closing.armed = false;
            }
            return Err(error.to_string());
        }
    };
    #[cfg(test)]
    if let Some(("after_store_completion", ready, resume)) = &hook {
        ready.notify_one();
        resume.notified().await;
    }
    closing.state.product_sessions.remove(edit_session_id);
    closing
        .state
        .gate
        .close_session(edit_session_id)
        .map_err(str::to_owned)?;
    closing.armed = false;
    Ok(
        notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Completed {
            generation: result.generation.to_string(),
        },
    )
}

#[cfg(test)]
mod complete_pending_remote_tests {
    include!("conflict_complete_pending_remote_tests.rs");
}

#[cfg(test)]
mod complete_pending_session_tests {
    include!("conflict_complete_pending_session_tests.rs");
}

#[allow(dead_code)]
async fn reopen_conflict_checkpoint_for_renderer(
    runtime: &Runtime,
    owner: &str,
    binding: &RegistrationBinding,
    document_id: &str,
    operation_id: &str,
    previous_edit_session_id: &str,
) -> Result<
    (
        notryx_client_store::ContentConflictResolutionState,
        notryx_protocol::WorkspaceOpenedDocument,
    ),
    String,
> {
    document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    operation_id
        .parse::<notryx_domain::OperationId>()
        .map_err(|_| "invalid_operation")?;
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    let session = state.session.clone().ok_or("not_connected")?;
    if registration_binding(session.binding()) != *binding {
        return Err("stale_registration".into());
    }
    let resolution =
        resolution_state_for_document(&mut state, binding, document_id, operation_id).await?;
    if state
        .store
        .read_registered_checkpoint(binding, document_id)
        .await
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Err("missing_draft".into());
    }
    if let Some(previous) = state
        .product_sessions
        .get(previous_edit_session_id)
        .cloned()
    {
        if previous.binding != *binding
            || previous.document_id != document_id
            || previous.kind != ProductSessionKind::Active
        {
            return Err("stale_session".into());
        }
        state
            .gate
            .check_session(previous_edit_session_id)
            .map_err(str::to_owned)?;
        state
            .close_local_document(&notryx_protocol::M0CloseRequest {
                document_id: document_id.to_owned(),
                edit_session_id: previous_edit_session_id.to_owned(),
            })
            .await?;
    } else {
        if state.gate.check_session(previous_edit_session_id).is_ok()
            || state
                .product_sessions
                .values()
                .any(|other| other.binding == *binding && other.document_id == document_id)
        {
            return Err("stale_session".into());
        }
    }
    let opened = state
        .try_open_local_draft(binding, document_id)
        .await?
        .ok_or("missing_draft")?;
    Ok((resolution, opened))
}

#[tauri::command]
pub(crate) async fn workspace_continue_remote_content_conflict(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictProgressRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictProgressResponse, String> {
    crate::check_caller(&webview)?;
    let version = request
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&request.binding);
    let result = continue_remote_choice_for_renderer(
        &runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.edit_session_id,
        &request.operation_id,
        version,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_complete_pending_remote_content_conflict(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictProgressRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictChooseRemoteResponse, String> {
    crate::check_caller(&webview)?;
    let version = request
        .expected_intent_version
        .parse::<u64>()
        .map_err(|_| "invalid_intent_version")?;
    let binding = crate::from_contract(&request.binding);
    let result = complete_pending_remote_choice_for_renderer(
        &runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.edit_session_id,
        &request.operation_id,
        version,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_reopen_content_conflict_checkpoint(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceContentConflictReopenRequest,
) -> Result<notryx_protocol::WorkspaceContentConflictReopenResponse, String> {
    crate::check_caller(&webview)?;
    let binding = crate::from_contract(&request.binding);
    let (state, document) = reopen_conflict_checkpoint_for_renderer(
        &runtime,
        &request.renderer_session_id,
        &binding,
        &request.document_id,
        &request.operation_id,
        &request.previous_edit_session_id,
    )
    .await?;
    crate::check_caller(&webview)?;
    Ok(notryx_protocol::WorkspaceContentConflictReopenResponse {
        state: resolution_to_contract(state),
        document,
    })
}

#[cfg(test)]
mod reopen_checkpoint_tests {
    include!("conflict_reopen_checkpoint_tests.rs");
}

#[cfg(test)]
mod displayed_hash_tests {
    include!("conflict_displayed_hash_tests.rs");
}

#[cfg(test)]
mod confirmation_success_tests {
    include!("choose_remote_confirmation_success_tests.rs");
}

#[cfg(test)]
mod confirmation_db_failure_tests {
    include!("choose_remote_confirmation_db_failure_tests.rs");
}

#[cfg(test)]
mod unconfirmed_followup_tests {
    include!("choose_remote_unconfirmed_followup_tests.rs");
}

#[cfg(test)]
mod confirmation_gate_tests {
    include!("choose_remote_confirmation_gate_tests.rs");
}

#[cfg(test)]
mod confirmation_cancel_tests {
    include!("choose_remote_confirmation_cancel_tests.rs");
}

#[cfg(test)]
mod tests {
    include!("conflict_snapshot_server_tests.rs");
    include!("conflict_snapshot_command_tests.rs");
    include!("conflict_snapshot_return_race_tests.rs");
}
