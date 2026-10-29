use crate::{Runtime, gate::RequestSnapshot, registration_binding};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use notryx_client_runtime::Session;
use notryx_client_store::{
    FrozenContentApplyApproval, OutboxAbandonState, OutboxOperation, OutboxStatus,
    RegistrationBinding,
};
use notryx_domain::{ChangeSeq, NodeId};
use notryx_protocol::{
    NodeState, WorkspaceAbandonState, WorkspaceFrozenContentInspectRequest,
    WorkspaceFrozenContentInspection, WorkspaceOutboxStatus, content_hash,
};
use std::sync::Arc;
use tauri::Webview;

pub(crate) struct FrozenContentInspection {
    pub(crate) operation: OutboxOperation,
    pub(crate) outcome_unknown: bool,
    pub(crate) local_generation: u64,
    pub(crate) local_bytes: Vec<u8>,
    pub(crate) base_bytes: Vec<u8>,
    pub(crate) base_revision: String,
    pub(crate) remote_revision: String,
    pub(crate) remote_bytes: Vec<u8>,
    gate: RequestSnapshot,
    generation: u64,
    session: Arc<Session>,
    current: RegistrationBinding,
    old: RegistrationBinding,
    cursor: ChangeSeq,
    root: NodeId,
    node: NodeState,
}

pub(crate) async fn inspect_frozen_content_for_renderer(
    runtime: &Runtime,
    renderer_owner: &str,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    operation_id: &str,
) -> Result<FrozenContentInspection, String> {
    if current == old || current.workspace_id != old.workspace_id {
        return Err("different_workspace".into());
    }
    let (gate, generation, session, operation, draft, cursor, root, node) = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .check_queue(renderer_owner)
            .map_err(str::to_owned)?;
        let gate = state.gate.snapshot(renderer_owner).map_err(str::to_owned)?;
        if state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(current)
        {
            return Err("stale_registration".into());
        }
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *current {
            return Err("stale_registration".into());
        }
        let generation = state.transmission_gate.response_generation(current);
        if !state
            .transmission_gate
            .response_trusted(current, generation)
        {
            return Err("stale_sync".into());
        }
        let operation = state
            .store
            .read_outbox_operation(old, operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("missing_operation")?;
        if !operation.frozen
            || !matches!(
                operation.status,
                OutboxStatus::Queued | OutboxStatus::Sent | OutboxStatus::Abandoning
            )
        {
            return Err("unsupported_operation".into());
        }
        let draft = state
            .store
            .read_registered_checkpoint(old, &operation.document_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("missing_draft")?;
        if draft.generation < operation.generation
            || (draft.generation == operation.generation && draft.bytes != operation.bytes)
            || (operation.status == OutboxStatus::Sent
                && operation.expected_revision.as_deref() != Some(draft.base_revision.as_str()))
        {
            return Err("invalid_draft".into());
        }
        let (cursor, nodes) = state
            .store
            .sync_index(current)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("resync_required")?;
        let root = nodes
            .iter()
            .find(|node| node.parent_id.is_none())
            .map(|node| node.node_id)
            .ok_or("resync_required")?;
        let node = nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == operation.document_id)
            .ok_or("resync_required")?;
        if node.deleted || node.kind != notryx_protocol::NodeKind::File {
            return Err("remote_not_live".into());
        }
        (
            gate, generation, session, operation, draft, cursor, root, node,
        )
    };

    let info = session
        .server_info()
        .await
        .map_err(|error| format!("{error:?}"))?;
    if !info.ready
        || info.head_seq != cursor
        || info.root_id != root
        || info.journal_floor_seq.get() > cursor.get()
    {
        return Err("resync_required".into());
    }
    let remote_bytes = session
        .content(&node)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let after = session
        .server_info()
        .await
        .map_err(|error| format!("{error:?}"))?;
    if !after.ready
        || after.head_seq != cursor
        || after.root_id != root
        || after.journal_floor_seq.get() > cursor.get()
    {
        return Err("resync_required".into());
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
    state
        .gate
        .check_queue(renderer_owner)
        .map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(current)
        || !state
            .session
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &session))
        || !state
            .transmission_gate
            .response_trusted(current, generation)
    {
        return Err("stale_registration".into());
    }
    let current_operation = state
        .store
        .read_outbox_operation(old, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_operation")?;
    let current_draft = state
        .store
        .read_registered_checkpoint(old, &operation.document_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_draft")?;
    let (current_cursor, nodes) = state
        .store
        .sync_index(current)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("resync_required")?;
    if current_operation != operation
        || current_draft != draft
        || current_cursor != cursor
        || !nodes.iter().any(|current_node| current_node == &node)
    {
        return Err("stale_inspection".into());
    }
    let remote_revision = node
        .content_rev
        .as_ref()
        .ok_or("invalid_remote_content")?
        .as_str()
        .to_owned();
    Ok(FrozenContentInspection {
        outcome_unknown: operation.status == OutboxStatus::Sent,
        local_generation: draft.generation,
        operation,
        local_bytes: draft.bytes,
        base_bytes: draft.base_bytes,
        base_revision: draft.base_revision,
        remote_revision,
        remote_bytes,
        gate,
        generation,
        session,
        current: current.clone(),
        old: old.clone(),
        cursor,
        root,
        node,
    })
}

pub(crate) async fn inspect_frozen_content_command_for_renderer(
    runtime: &Runtime,
    request: WorkspaceFrozenContentInspectRequest,
) -> Result<WorkspaceFrozenContentInspection, String> {
    request
        .operation_id
        .parse::<notryx_domain::OperationId>()
        .map_err(|_| "invalid_operation")?;
    let current = crate::from_contract(&request.current_binding);
    let old = crate::from_contract(&request.old_binding);
    let result = inspect_frozen_content_for_renderer(
        runtime,
        &request.renderer_session_id,
        &current,
        &old,
        &request.operation_id,
    )
    .await?;
    finish_frozen_content_inspection(runtime, request, result).await
}

pub(crate) async fn finish_frozen_content_inspection(
    runtime: &Runtime,
    request: WorkspaceFrozenContentInspectRequest,
    result: FrozenContentInspection,
) -> Result<WorkspaceFrozenContentInspection, String> {
    if crate::from_contract(&request.current_binding) != result.current
        || crate::from_contract(&request.old_binding) != result.old
        || request.operation_id != result.operation.operation_id
    {
        return Err("stale_inspection".into());
    }
    let operation_status = match result.operation.status {
        OutboxStatus::Queued => WorkspaceOutboxStatus::Queued,
        OutboxStatus::Sent => WorkspaceOutboxStatus::Sent,
        OutboxStatus::Abandoning => WorkspaceOutboxStatus::Abandoning,
        _ => return Err("unsupported_operation".into()),
    };
    if content_hash(&result.operation.bytes) != result.operation.payload_hash {
        return Err("invalid_operation_payload".into());
    }
    let response = WorkspaceFrozenContentInspection {
        current_binding: request.current_binding,
        old_binding: request.old_binding,
        document_id: result.operation.document_id.clone(),
        operation_id: result.operation.operation_id.clone(),
        operation_seq: result.operation.operation_seq.to_string(),
        operation_status,
        abandon_state: match result.operation.abandon_state {
            OutboxAbandonState::None => WorkspaceAbandonState::None,
            OutboxAbandonState::Requested => WorkspaceAbandonState::Requested,
            OutboxAbandonState::Sent => WorkspaceAbandonState::Sent,
        },
        operation_generation: result.operation.generation.to_string(),
        local_generation: result.local_generation.to_string(),
        expected_revision: result.operation.expected_revision.clone(),
        outcome_unknown: result.outcome_unknown,
        base_revision: result.base_revision.clone(),
        base_bytes_base64: STANDARD.encode(&result.base_bytes),
        base_hash: content_hash(&result.base_bytes),
        operation_bytes_base64: STANDARD.encode(&result.operation.bytes),
        operation_hash: result.operation.payload_hash.clone(),
        local_bytes_base64: STANDARD.encode(&result.local_bytes),
        local_hash: content_hash(&result.local_bytes),
        remote_revision: result.remote_revision.clone(),
        remote_bytes_base64: STANDARD.encode(&result.remote_bytes),
        remote_hash: content_hash(&result.remote_bytes),
    };
    let mut state = runtime.0.lock().await;
    state.gate.validate(&result.gate).map_err(str::to_owned)?;
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
        != Some(&result.current)
        || !state
            .session
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &result.session))
        || !state
            .transmission_gate
            .response_trusted(&result.current, result.generation)
    {
        return Err("stale_registration".into());
    }
    let operation = state
        .store
        .read_outbox_operation(&result.old, &result.operation.operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_operation")?;
    let draft = state
        .store
        .read_registered_checkpoint(&result.old, &result.operation.document_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_draft")?;
    let (cursor, nodes) = state
        .store
        .sync_index(&result.current)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("resync_required")?;
    if operation != result.operation
        || draft.generation != result.local_generation
        || draft.bytes != result.local_bytes
        || draft.base_bytes != result.base_bytes
        || draft.base_revision != result.base_revision
        || cursor != result.cursor
        || !nodes
            .iter()
            .any(|node| node.parent_id.is_none() && node.node_id == result.root)
        || !nodes.iter().any(|node| node == &result.node)
    {
        return Err("stale_inspection".into());
    }
    Ok(response)
}

#[tauri::command]
pub(crate) async fn workspace_inspect_frozen_content(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenContentInspectRequest,
) -> Result<WorkspaceFrozenContentInspection, String> {
    crate::check_caller(&webview)?;
    let result = inspect_frozen_content_command_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

pub(crate) use notryx_protocol::{
    WorkspaceFrozenContentApplyChoice as FrozenContentApplyChoice,
    WorkspaceFrozenContentApplyRequest as FrozenContentApplyRequest,
    WorkspaceFrozenContentApplyResponse as FrozenContentApplyResult,
};

fn frozen_view_fingerprint(request: &FrozenContentApplyRequest) -> Result<String, String> {
    match request.choice {
        FrozenContentApplyChoice::LatestDraft => {}
    }
    frozen_inspection_fingerprint(
        &request.inspected,
        "notryx-frozen-content-latest-draft-view-v1",
        true,
    )
}

fn frozen_inspection_fingerprint(
    value: &WorkspaceFrozenContentInspection,
    purpose: &str,
    require_same_epoch: bool,
) -> Result<String, String> {
    let current = crate::from_contract(&value.current_binding);
    let old = crate::from_contract(&value.old_binding);
    if current == old
        || current.workspace_id != old.workspace_id
        || (require_same_epoch && current.workspace_epoch != old.workspace_epoch)
    {
        return Err("unsupported_registration".into());
    }
    value
        .document_id
        .parse::<NodeId>()
        .map_err(|_| "invalid_document")?;
    value
        .operation_id
        .parse::<notryx_domain::OperationId>()
        .map_err(|_| "invalid_operation")?;
    for sequence in [
        &value.operation_seq,
        &value.operation_generation,
        &value.local_generation,
    ] {
        let parsed = sequence.parse::<u64>().map_err(|_| "invalid_sequence")?;
        if parsed == 0 || parsed.to_string() != *sequence {
            return Err("invalid_sequence".into());
        }
    }
    let (status, abandon) = match (
        value.operation_status,
        value.abandon_state,
        value.outcome_unknown,
    ) {
        (WorkspaceOutboxStatus::Queued, WorkspaceAbandonState::None, false) => ("queued", "none"),
        (WorkspaceOutboxStatus::Sent, WorkspaceAbandonState::None, true) => ("sent", "none"),
        (WorkspaceOutboxStatus::Abandoning, WorkspaceAbandonState::Requested, false) => {
            ("abandoning", "requested")
        }
        (WorkspaceOutboxStatus::Abandoning, WorkspaceAbandonState::Sent, false) => {
            ("abandoning", "sent")
        }
        _ => return Err("invalid_operation_state".into()),
    };
    for (encoded, hash, maximum) in [
        (
            &value.operation_bytes_base64,
            &value.operation_hash,
            notryx_client_store::MAX_DOCUMENT_BYTES,
        ),
        (
            &value.base_bytes_base64,
            &value.base_hash,
            notryx_client_store::MAX_DOCUMENT_BYTES,
        ),
        (
            &value.local_bytes_base64,
            &value.local_hash,
            notryx_client_store::MAX_DOCUMENT_BYTES,
        ),
        (
            &value.remote_bytes_base64,
            &value.remote_hash,
            25 * 1024 * 1024,
        ),
    ] {
        if encoded.len() > maximum.div_ceil(3) * 4 {
            return Err("invalid_snapshot".into());
        }
        let bytes = STANDARD.decode(encoded).map_err(|_| "invalid_snapshot")?;
        if bytes.len() > maximum
            || STANDARD.encode(&bytes) != *encoded
            || content_hash(&bytes) != *hash
        {
            return Err("invalid_snapshot".into());
        }
    }
    let mut canonical = Vec::new();
    fn field(canonical: &mut Vec<u8>, value: &str) {
        canonical.extend_from_slice(&(value.len() as u64).to_be_bytes());
        canonical.extend_from_slice(value.as_bytes());
    }
    field(&mut canonical, purpose);
    for binding in [&current, &old] {
        for value in [
            &binding.server_origin,
            &binding.workspace_id,
            &binding.workspace_epoch,
            &binding.device_id,
            &binding.device_stream_id,
        ] {
            field(&mut canonical, value);
        }
    }
    for item in [
        value.document_id.as_str(),
        value.operation_id.as_str(),
        value.operation_seq.as_str(),
        status,
        abandon,
        value.operation_generation.as_str(),
        value.local_generation.as_str(),
        if value.outcome_unknown {
            "true"
        } else {
            "false"
        },
        if value.expected_revision.is_some() {
            "some"
        } else {
            "none"
        },
        value.expected_revision.as_deref().unwrap_or(""),
        value.base_revision.as_str(),
        value.base_hash.as_str(),
        value.operation_hash.as_str(),
        value.local_hash.as_str(),
        value.remote_revision.as_str(),
        value.remote_hash.as_str(),
    ] {
        field(&mut canonical, item);
    }
    Ok(content_hash(&canonical))
}

pub(crate) async fn apply_frozen_content_for_renderer(
    runtime: &Runtime,
    request: FrozenContentApplyRequest,
) -> Result<FrozenContentApplyResult, String> {
    let fingerprint = frozen_view_fingerprint(&request)?;
    let current = crate::from_contract(&request.inspected.current_binding);
    let old = crate::from_contract(&request.inspected.old_binding);
    let document = request.inspected.document_id.clone();
    let operation_id = request.inspected.operation_id.clone();
    let (gate, generation, session) = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .check_queue(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        if state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&current)
        {
            return Err("stale_registration".into());
        }
        if let Some(decision) = state
            .store
            .lookup_frozen_content_decision(&current, &old, &document, &operation_id, &fingerprint)
            .await
            .map_err(|error| error.to_string())?
        {
            return Ok(FrozenContentApplyResult {
                operation_id: decision.operation_id,
                operation_seq: decision.operation_seq.to_string(),
            });
        }
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != current {
            return Err("stale_registration".into());
        }
        let generation = state.transmission_gate.response_generation(&current);
        if !state
            .transmission_gate
            .response_trusted(&current, generation)
        {
            return Err("stale_sync".into());
        }
        (gate, generation, session)
    };
    match session
        .current_role()
        .await
        .map_err(|error| format!("{error:?}"))?
    {
        notryx_protocol::WorkspaceRole::Owner | notryx_protocol::WorkspaceRole::Editor => {}
        notryx_protocol::WorkspaceRole::Reader => return Err("write_forbidden".into()),
    }
    let inspection = inspect_frozen_content_for_renderer(
        runtime,
        &request.renderer_session_id,
        &current,
        &old,
        &operation_id,
    )
    .await?;
    if !Arc::ptr_eq(&inspection.session, &session) || inspection.generation != generation {
        return Err("stale_registration".into());
    }
    let approval = FrozenContentApplyApproval {
        old: old.clone(),
        current: current.clone(),
        document: document.clone(),
        old_operation_id: operation_id.clone(),
        old_operation_seq: inspection.operation.operation_seq,
        old_operation_hash: inspection.operation.payload_hash.clone(),
        old_operation_status: inspection.operation.status,
        generation: inspection.local_generation,
        draft: inspection.local_bytes.clone(),
        base: inspection.base_bytes.clone(),
        base_revision: inspection.base_revision.clone(),
        remote_revision: inspection.remote_revision.clone(),
        remote_hash: content_hash(&inspection.remote_bytes),
        remote_size: inspection.remote_bytes.len() as u64,
        remote_bytes: inspection.remote_bytes.clone(),
        remote_cursor: String::from(inspection.cursor),
        view_fingerprint: fingerprint.clone(),
    };
    let shown = finish_frozen_content_inspection(
        runtime,
        WorkspaceFrozenContentInspectRequest {
            renderer_session_id: request.renderer_session_id.clone(),
            current_binding: request.inspected.current_binding.clone(),
            old_binding: request.inspected.old_binding.clone(),
            operation_id: operation_id.clone(),
        },
        inspection,
    )
    .await?;
    if shown != request.inspected {
        return Err("stale_inspection".into());
    }
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
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
        != Some(&current)
        || !state
            .session
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &session))
        || !state
            .transmission_gate
            .response_trusted(&current, generation)
    {
        return Err("stale_registration".into());
    }
    if let Some(decision) = state
        .store
        .lookup_frozen_content_decision(&current, &old, &document, &operation_id, &fingerprint)
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(FrozenContentApplyResult {
            operation_id: decision.operation_id,
            operation_seq: decision.operation_seq.to_string(),
        });
    }
    let id = state
        .store
        .apply_frozen_content_approval(&approval)
        .await
        .map_err(|error| error.to_string())?;
    let decision = state
        .store
        .lookup_frozen_content_decision(&current, &old, &document, &operation_id, &fingerprint)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_decision")?;
    if decision.operation_id != id {
        return Err("invalid_decision".into());
    }
    Ok(FrozenContentApplyResult {
        operation_id: decision.operation_id,
        operation_seq: decision.operation_seq.to_string(),
    })
}

#[tauri::command]
pub(crate) async fn workspace_apply_frozen_content(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: FrozenContentApplyRequest,
) -> Result<FrozenContentApplyResult, String> {
    crate::check_caller(&webview)?;
    let result = apply_frozen_content_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

pub(crate) async fn dismiss_frozen_content_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceFrozenContentDismissRequest,
) -> Result<notryx_protocol::WorkspaceFrozenContentDismissed, String> {
    let value = &request.inspected;
    let fingerprint =
        frozen_inspection_fingerprint(value, "notryx-frozen-content-dismiss-view-v1", false)?;
    let current = crate::from_contract(&value.current_binding);
    let old = crate::from_contract(&value.old_binding);
    let result = notryx_protocol::WorkspaceFrozenContentDismissed {
        operation_id: value.operation_id.clone(),
        operation_seq: value.operation_seq.clone(),
        operation_status: value.operation_status,
        abandon_state: value.abandon_state,
    };
    let (gate, generation, session) = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .check_queue(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        if state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&current)
        {
            return Err("stale_registration".into());
        }
        if state
            .store
            .lookup_frozen_content_dismissal(
                &current,
                &old,
                &value.document_id,
                &value.operation_id,
                &fingerprint,
            )
            .await
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(result);
        }
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != current {
            return Err("stale_registration".into());
        }
        let generation = state.transmission_gate.response_generation(&current);
        if !state
            .transmission_gate
            .response_trusted(&current, generation)
        {
            return Err("stale_sync".into());
        }
        (gate, generation, session)
    };
    session
        .current_role()
        .await
        .map_err(|error| format!("{error:?}"))?;
    let approval = notryx_client_store::FrozenContentDismissApproval {
        current: current.clone(),
        old: old.clone(),
        document: value.document_id.clone(),
        operation_id: value.operation_id.clone(),
        operation_seq: value
            .operation_seq
            .parse()
            .map_err(|_| "invalid_sequence")?,
        operation_generation: value
            .operation_generation
            .parse()
            .map_err(|_| "invalid_sequence")?,
        operation_status: match value.operation_status {
            WorkspaceOutboxStatus::Queued => OutboxStatus::Queued,
            WorkspaceOutboxStatus::Sent => OutboxStatus::Sent,
            WorkspaceOutboxStatus::Abandoning => OutboxStatus::Abandoning,
            _ => return Err("invalid_operation_state".into()),
        },
        dependency: None,
        abandon_state: match value.abandon_state {
            WorkspaceAbandonState::None => OutboxAbandonState::None,
            WorkspaceAbandonState::Requested => OutboxAbandonState::Requested,
            WorkspaceAbandonState::Sent => OutboxAbandonState::Sent,
        },
        expected_revision: value.expected_revision.clone(),
        payload_hash: value.operation_hash.clone(),
        payload: STANDARD
            .decode(&value.operation_bytes_base64)
            .map_err(|_| "invalid_snapshot")?,
        draft_generation: value
            .local_generation
            .parse()
            .map_err(|_| "invalid_sequence")?,
        draft: STANDARD
            .decode(&value.local_bytes_base64)
            .map_err(|_| "invalid_snapshot")?,
        base: STANDARD
            .decode(&value.base_bytes_base64)
            .map_err(|_| "invalid_snapshot")?,
        base_revision: value.base_revision.clone(),
        view_fingerprint: fingerprint.clone(),
    };
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
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
        != Some(&current)
        || !state
            .session
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &session))
        || !state
            .transmission_gate
            .response_trusted(&current, generation)
    {
        return Err("stale_registration".into());
    }
    if state
        .store
        .lookup_frozen_content_dismissal(
            &current,
            &old,
            &value.document_id,
            &value.operation_id,
            &fingerprint,
        )
        .await
        .map_err(|error| error.to_string())?
        .is_none()
    {
        state
            .store
            .dismiss_frozen_content(&approval)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_dismiss_frozen_content(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceFrozenContentDismissRequest,
) -> Result<notryx_protocol::WorkspaceFrozenContentDismissed, String> {
    crate::check_caller(&webview)?;
    let result = dismiss_frozen_content_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

pub(crate) async fn frozen_content_decision_status_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceFrozenContentDecisionStatusRequest,
) -> Result<notryx_protocol::WorkspaceFrozenContentDecisionStatusResponse, String> {
    let current = crate::from_contract(&request.binding);
    let old = crate::from_contract(&request.old_binding);
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
        != Some(&current)
    {
        return Err("stale_registration".into());
    }
    let result = state
        .store
        .frozen_content_decision_status(
            &current,
            &old,
            &request.document_id,
            &request.old_operation_id,
        )
        .await
        .map_err(|error| error.to_string())?;
    let is_dismissed = state
        .store
        .frozen_content_dismissal_status(
            &current,
            &old,
            &request.document_id,
            &request.old_operation_id,
        )
        .await
        .map_err(|error| error.to_string())?;
    if result.is_some() && is_dismissed {
        return Err("invalid_decision".into());
    }
    let dismissed = if is_dismissed {
        let operation = state
            .store
            .read_outbox_operation(&old, &request.old_operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("invalid_decision")?;
        if operation.document_id != request.document_id || !operation.frozen {
            return Err("invalid_decision".into());
        }
        Some(notryx_protocol::WorkspaceFrozenContentDismissed {
            operation_id: operation.operation_id,
            operation_seq: operation.operation_seq.to_string(),
            operation_status: match operation.status {
                OutboxStatus::Queued => WorkspaceOutboxStatus::Queued,
                OutboxStatus::Sent => WorkspaceOutboxStatus::Sent,
                OutboxStatus::Abandoning => WorkspaceOutboxStatus::Abandoning,
                _ => return Err("invalid_decision".into()),
            },
            abandon_state: match operation.abandon_state {
                OutboxAbandonState::None => WorkspaceAbandonState::None,
                OutboxAbandonState::Requested => WorkspaceAbandonState::Requested,
                OutboxAbandonState::Sent => WorkspaceAbandonState::Sent,
            },
        })
    } else {
        None
    };
    let decision = result.map(|item| notryx_protocol::WorkspaceFrozenContentDecision {
        operation_id: item.operation_id,
        operation_seq: item.operation_seq.to_string(),
        status: match item.status {
            OutboxStatus::Queued => WorkspaceOutboxStatus::Queued,
            OutboxStatus::Sent => WorkspaceOutboxStatus::Sent,
            OutboxStatus::Committed => WorkspaceOutboxStatus::Committed,
            OutboxStatus::Conflict => WorkspaceOutboxStatus::Conflict,
            OutboxStatus::Abandoning => WorkspaceOutboxStatus::Abandoning,
            OutboxStatus::Abandoned => WorkspaceOutboxStatus::Abandoned,
            OutboxStatus::Failed => WorkspaceOutboxStatus::Failed,
        },
        approved_generation: item.approved_generation.to_string(),
        approved_hash: item.approved_hash,
    });
    Ok(
        notryx_protocol::WorkspaceFrozenContentDecisionStatusResponse {
            binding: request.binding,
            old_binding: request.old_binding,
            document_id: request.document_id,
            old_operation_id: request.old_operation_id,
            decision,
            dismissed,
        },
    )
}

#[tauri::command]
pub(crate) async fn workspace_frozen_content_decision_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceFrozenContentDecisionStatusRequest,
) -> Result<notryx_protocol::WorkspaceFrozenContentDecisionStatusResponse, String> {
    crate::check_caller(&webview)?;
    let result = frozen_content_decision_status_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

fn cross_epoch_target(
    nodes: &[NodeState],
    root: NodeId,
    document: &str,
) -> Result<notryx_protocol::WorkspaceFrozenContentCrossEpochTarget, String> {
    use std::collections::{HashMap, HashSet};
    let mut index = HashMap::new();
    for node in nodes {
        if index.insert(node.node_id, node).is_some() {
            return Err("resync_required".into());
        }
    }
    let document_id: NodeId = document.parse().map_err(|_| "invalid_document")?;
    let node = *index.get(&document_id).ok_or("resync_required")?;
    if node.deleted || node.kind != notryx_protocol::NodeKind::File {
        return Err("remote_not_live".into());
    }
    let parent_id = node.parent_id.ok_or("resync_required")?;
    let mut parts = vec![node.name.clone()];
    let mut seen = HashSet::from([node.node_id]);
    let mut parent = parent_id;
    loop {
        if !seen.insert(parent) {
            return Err("resync_required".into());
        }
        let folder = *index.get(&parent).ok_or("resync_required")?;
        if folder.deleted || folder.kind != notryx_protocol::NodeKind::Folder {
            return Err("resync_required".into());
        }
        if folder.node_id == root {
            if folder.parent_id.is_some() {
                return Err("resync_required".into());
            }
            break;
        }
        if folder.name.is_empty() {
            return Err("resync_required".into());
        }
        parts.push(folder.name.clone());
        parent = folder.parent_id.ok_or("resync_required")?;
    }
    parts.reverse();
    Ok(notryx_protocol::WorkspaceFrozenContentCrossEpochTarget {
        node_id: node.node_id.to_string(),
        parent_id: parent_id.to_string(),
        name: node.name.clone(),
        path: format!("/{}", parts.join("/")),
    })
}

pub(crate) async fn inspect_frozen_content_cross_epoch_for_renderer(
    runtime: &Runtime,
    request: WorkspaceFrozenContentInspectRequest,
) -> Result<notryx_protocol::WorkspaceFrozenContentCrossEpochInspection, String> {
    let current = crate::from_contract(&request.current_binding);
    let old = crate::from_contract(&request.old_binding);
    if current.workspace_id != old.workspace_id || current.workspace_epoch == old.workspace_epoch {
        return Err("unsupported_registration".into());
    }
    let result = inspect_frozen_content_for_renderer(
        runtime,
        &request.renderer_session_id,
        &current,
        &old,
        &request.operation_id,
    )
    .await?;
    let target = {
        let mut state = runtime.0.lock().await;
        state.gate.validate(&result.gate).map_err(str::to_owned)?;
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
            != Some(&current)
            || !state
                .session
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &result.session))
            || !state
                .transmission_gate
                .response_trusted(&current, result.generation)
        {
            return Err("stale_registration".into());
        }
        let (cursor, nodes) = state
            .store
            .sync_index(&current)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("resync_required")?;
        if cursor != result.cursor || !nodes.iter().any(|node| node == &result.node) {
            return Err("stale_inspection".into());
        }
        cross_epoch_target(&nodes, result.root, &result.operation.document_id)?
    };
    let inspected = finish_frozen_content_inspection(runtime, request, result).await?;
    Ok(
        notryx_protocol::WorkspaceFrozenContentCrossEpochInspection {
            inspected,
            current_target: target,
        },
    )
}

#[tauri::command]
pub(crate) async fn workspace_inspect_frozen_content_cross_epoch(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenContentInspectRequest,
) -> Result<notryx_protocol::WorkspaceFrozenContentCrossEpochInspection, String> {
    crate::check_caller(&webview)?;
    let result = inspect_frozen_content_cross_epoch_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

fn cross_epoch_fingerprint(
    request: &notryx_protocol::WorkspaceFrozenContentCrossEpochApplyRequest,
) -> Result<String, String> {
    if !request.same_node_is_not_identity {
        return Err("confirmation_required".into());
    }
    let value = &request.inspected.inspected;
    let current = crate::from_contract(&value.current_binding);
    let old = crate::from_contract(&value.old_binding);
    if current.workspace_id != old.workspace_id || current.workspace_epoch == old.workspace_epoch {
        return Err("unsupported_registration".into());
    }
    let target = &request.inspected.current_target;
    let node: NodeId = target.node_id.parse().map_err(|_| "invalid_document")?;
    let parent: NodeId = target.parent_id.parse().map_err(|_| "invalid_document")?;
    if node.to_string() != target.node_id || parent.to_string() != target.parent_id {
        return Err("invalid_document".into());
    }
    if target.node_id != value.document_id || target.name.is_empty() || target.path.is_empty() {
        return Err("InvalidRequest".into());
    }
    let base =
        frozen_inspection_fingerprint(value, "notryx-frozen-content-cross-epoch-view-v1", false)?;
    let mut data = base.into_bytes();
    data.extend_from_slice(&serde_json::to_vec(target).map_err(|_| "invalid_snapshot")?);
    Ok(content_hash(&data))
}

pub(crate) async fn apply_frozen_content_cross_epoch_for_renderer(
    runtime: &Runtime,
    request: notryx_protocol::WorkspaceFrozenContentCrossEpochApplyRequest,
) -> Result<FrozenContentApplyResult, String> {
    let fingerprint = cross_epoch_fingerprint(&request)?;
    let value = &request.inspected.inspected;
    let target = &request.inspected.current_target;
    let current = crate::from_contract(&value.current_binding);
    let old = crate::from_contract(&value.old_binding);
    let (gate, generation, session) = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .check_queue(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let gate = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        if state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&current)
        {
            return Err("stale_registration".into());
        }
        if let Some(decision) = state
            .store
            .lookup_frozen_content_decision(
                &current,
                &old,
                &value.document_id,
                &value.operation_id,
                &fingerprint,
            )
            .await
            .map_err(|error| error.to_string())?
        {
            return Ok(FrozenContentApplyResult {
                operation_id: decision.operation_id,
                operation_seq: decision.operation_seq.to_string(),
            });
        }
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != current {
            return Err("stale_registration".into());
        }
        let generation = state.transmission_gate.response_generation(&current);
        if !state
            .transmission_gate
            .response_trusted(&current, generation)
        {
            return Err("stale_sync".into());
        }
        (gate, generation, session)
    };
    match session
        .current_role()
        .await
        .map_err(|error| format!("{error:?}"))?
    {
        notryx_protocol::WorkspaceRole::Owner | notryx_protocol::WorkspaceRole::Editor => {}
        notryx_protocol::WorkspaceRole::Reader => return Err("write_forbidden".into()),
    }
    let inspection = inspect_frozen_content_for_renderer(
        runtime,
        &request.renderer_session_id,
        &current,
        &old,
        &value.operation_id,
    )
    .await?;
    if !Arc::ptr_eq(&inspection.session, &session) || inspection.generation != generation {
        return Err("stale_registration".into());
    }
    let approval = FrozenContentApplyApproval {
        old: old.clone(),
        current: current.clone(),
        document: value.document_id.clone(),
        old_operation_id: value.operation_id.clone(),
        old_operation_seq: inspection.operation.operation_seq,
        old_operation_hash: inspection.operation.payload_hash.clone(),
        old_operation_status: inspection.operation.status,
        generation: inspection.local_generation,
        draft: inspection.local_bytes.clone(),
        base: inspection.base_bytes.clone(),
        base_revision: inspection.base_revision.clone(),
        remote_revision: inspection.remote_revision.clone(),
        remote_hash: content_hash(&inspection.remote_bytes),
        remote_size: inspection.remote_bytes.len() as u64,
        remote_bytes: inspection.remote_bytes.clone(),
        remote_cursor: String::from(inspection.cursor),
        view_fingerprint: fingerprint.clone(),
    };
    let current_target = {
        let mut state = runtime.0.lock().await;
        state.gate.validate(&gate).map_err(str::to_owned)?;
        if state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&current)
            || !state
                .session
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &session))
            || !state
                .transmission_gate
                .response_trusted(&current, generation)
        {
            return Err("stale_registration".into());
        }
        let (cursor, nodes) = state
            .store
            .sync_index(&current)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("resync_required")?;
        if cursor != inspection.cursor || !nodes.iter().any(|node| node == &inspection.node) {
            return Err("stale_inspection".into());
        }
        cross_epoch_target(&nodes, inspection.root, &value.document_id)?
    };
    let shown = finish_frozen_content_inspection(
        runtime,
        WorkspaceFrozenContentInspectRequest {
            renderer_session_id: request.renderer_session_id.clone(),
            current_binding: value.current_binding.clone(),
            old_binding: value.old_binding.clone(),
            operation_id: value.operation_id.clone(),
        },
        inspection,
    )
    .await?;
    if shown != *value || current_target != *target {
        return Err("stale_inspection".into());
    }
    let confirmation = notryx_client_store::FrozenContentCrossEpochConfirmation {
        selected_current_document: target.node_id.clone(),
        current_parent: target.parent_id.clone(),
        current_name: target.name.clone(),
        displayed_path: target.path.clone(),
        old_epoch: old.workspace_epoch.clone(),
        current_epoch: current.workspace_epoch.clone(),
        remote_revision: value.remote_revision.clone(),
        remote_hash: value.remote_hash.clone(),
        explicitly_confirmed: true,
    };
    let mut state = runtime.0.lock().await;
    state.gate.validate(&gate).map_err(str::to_owned)?;
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
        != Some(&current)
        || !state
            .session
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &session))
        || !state
            .transmission_gate
            .response_trusted(&current, generation)
    {
        return Err("stale_registration".into());
    }
    if let Some(decision) = state
        .store
        .lookup_frozen_content_decision(
            &current,
            &old,
            &value.document_id,
            &value.operation_id,
            &fingerprint,
        )
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(FrozenContentApplyResult {
            operation_id: decision.operation_id,
            operation_seq: decision.operation_seq.to_string(),
        });
    }
    let id = state
        .store
        .apply_frozen_content_across_epochs(&approval, &confirmation)
        .await
        .map_err(|error| error.to_string())?;
    let decision = state
        .store
        .lookup_frozen_content_decision(
            &current,
            &old,
            &value.document_id,
            &value.operation_id,
            &fingerprint,
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_decision")?;
    if decision.operation_id != id {
        return Err("invalid_decision".into());
    }
    Ok(FrozenContentApplyResult {
        operation_id: decision.operation_id,
        operation_seq: decision.operation_seq.to_string(),
    })
}

#[tauri::command]
pub(crate) async fn workspace_apply_frozen_content_cross_epoch(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceFrozenContentCrossEpochApplyRequest,
) -> Result<FrozenContentApplyResult, String> {
    crate::check_caller(&webview)?;
    let result = apply_frozen_content_cross_epoch_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}
