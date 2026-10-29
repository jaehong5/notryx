use crate::{Runtime, registration_binding};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use notryx_client_store::{
    FrozenContentDismissApproval, FrozenDocumentOperationKind, OutboxAbandonState, OutboxStatus,
    RegistrationBinding,
};
use notryx_protocol::{
    WorkspaceAbandonState, WorkspaceFrozenContentDismissed,
    WorkspaceFrozenContentLocalDismissRequest, WorkspaceFrozenContentLocalInspectRequest,
    WorkspaceFrozenContentLocalInspection, WorkspaceFrozenContentRemoteEvidence,
    WorkspaceOutboxStatus, content_hash,
};
use std::sync::Arc;
use tauri::Webview;

const LOCAL_REVIEW_LABEL: &str = "notryx-frozen-content-local-dismiss-view-v1";
const MAX_REMOTE_BYTES: u64 = 25 * 1024 * 1024;

fn canonical_positive(value: &str) -> Result<u64, String> {
    let parsed = value.parse::<u64>().map_err(|_| "invalid_sequence")?;
    if parsed == 0 || parsed.to_string() != value {
        return Err("invalid_sequence".into());
    }
    Ok(parsed)
}

fn decoded_verified(value: &str, hash: &str) -> Result<Vec<u8>, String> {
    let max = notryx_client_store::MAX_DOCUMENT_BYTES;
    if value.len() > max.div_ceil(3) * 4 {
        return Err("invalid_snapshot".into());
    }
    let bytes = STANDARD.decode(value).map_err(|_| "invalid_snapshot")?;
    if bytes.len() > max || STANDARD.encode(&bytes) != value || content_hash(&bytes) != hash {
        return Err("invalid_snapshot".into());
    }
    Ok(bytes)
}

fn status_parts(
    value: &WorkspaceFrozenContentLocalInspection,
) -> Result<(OutboxStatus, OutboxAbandonState), String> {
    match (
        value.operation_status,
        value.abandon_state,
        value.outcome_unknown,
    ) {
        (WorkspaceOutboxStatus::Queued, WorkspaceAbandonState::None, false) => {
            Ok((OutboxStatus::Queued, OutboxAbandonState::None))
        }
        (WorkspaceOutboxStatus::Sent, WorkspaceAbandonState::None, true) => {
            Ok((OutboxStatus::Sent, OutboxAbandonState::None))
        }
        (WorkspaceOutboxStatus::Abandoning, WorkspaceAbandonState::Requested, false) => {
            Ok((OutboxStatus::Abandoning, OutboxAbandonState::Requested))
        }
        (WorkspaceOutboxStatus::Abandoning, WorkspaceAbandonState::Sent, false) => {
            Ok((OutboxStatus::Abandoning, OutboxAbandonState::Sent))
        }
        _ => Err("invalid_operation_state".into()),
    }
}

fn local_fingerprint(value: &WorkspaceFrozenContentLocalInspection) -> Result<String, String> {
    if value.remote_index_error
        && !matches!(
            value.remote_evidence,
            WorkspaceFrozenContentRemoteEvidence::Unknown
        )
    {
        return Err("invalid_snapshot".into());
    }
    let current = crate::from_contract(&value.current_binding);
    let old = crate::from_contract(&value.old_binding);
    if current == old || current.workspace_id != old.workspace_id {
        return Err("different_workspace".into());
    }
    let doc: notryx_domain::NodeId = value.document_id.parse().map_err(|_| "invalid_document")?;
    let op: notryx_domain::OperationId = value
        .operation_id
        .parse()
        .map_err(|_| "invalid_operation")?;
    if doc.to_string() != value.document_id || op.to_string() != value.operation_id {
        return Err("invalid_snapshot".into());
    }
    if let Some(dependency) = &value.dependency {
        let parsed: notryx_domain::OperationId =
            dependency.parse().map_err(|_| "invalid_operation")?;
        if parsed.to_string() != *dependency {
            return Err("invalid_snapshot".into());
        }
    }
    canonical_positive(&value.operation_seq)?;
    canonical_positive(&value.operation_generation)?;
    canonical_positive(&value.local_generation)?;
    status_parts(value)?;
    decoded_verified(&value.operation_bytes_base64, &value.operation_hash)?;
    decoded_verified(&value.local_bytes_base64, &value.local_hash)?;
    decoded_verified(&value.base_bytes_base64, &value.base_hash)?;
    match &value.remote_evidence {
        WorkspaceFrozenContentRemoteEvidence::Deleted { change_seq } => {
            canonical_positive(change_seq)?;
        }
        WorkspaceFrozenContentRemoteEvidence::TooLarge { change_seq, size } => {
            canonical_positive(change_seq)?;
            if canonical_positive(size)? <= MAX_REMOTE_BYTES {
                return Err("invalid_snapshot".into());
            }
        }
        WorkspaceFrozenContentRemoteEvidence::Unknown => {}
    }
    let mut canonical = LOCAL_REVIEW_LABEL.as_bytes().to_vec();
    canonical.extend_from_slice(&serde_json::to_vec(value).map_err(|_| "invalid_snapshot")?);
    Ok(content_hash(&canonical))
}

fn operation_status(
    status: OutboxStatus,
    abandon: OutboxAbandonState,
) -> Result<(WorkspaceOutboxStatus, WorkspaceAbandonState, bool), String> {
    match (status, abandon) {
        (OutboxStatus::Queued, OutboxAbandonState::None) => Ok((
            WorkspaceOutboxStatus::Queued,
            WorkspaceAbandonState::None,
            false,
        )),
        (OutboxStatus::Sent, OutboxAbandonState::None) => Ok((
            WorkspaceOutboxStatus::Sent,
            WorkspaceAbandonState::None,
            true,
        )),
        (OutboxStatus::Abandoning, OutboxAbandonState::Requested) => Ok((
            WorkspaceOutboxStatus::Abandoning,
            WorkspaceAbandonState::Requested,
            false,
        )),
        (OutboxStatus::Abandoning, OutboxAbandonState::Sent) => Ok((
            WorkspaceOutboxStatus::Abandoning,
            WorkspaceAbandonState::Sent,
            false,
        )),
        _ => Err("unsupported_operation".into()),
    }
}

async fn local_sources(
    state: &mut crate::RuntimeState,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
) -> Result<WorkspaceFrozenContentLocalInspection, String> {
    let operation = state
        .store
        .read_outbox_operation(old, operation_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("missing_operation")?;
    if !operation.frozen || operation.document_id != document || operation.operation_seq == 0 {
        return Err("unsupported_operation".into());
    }
    let review = state
        .store
        .frozen_document_review(current, old, document, operation.operation_seq - 1, 1)
        .await
        .map_err(|error| error.to_string())?;
    let entry = review.operations.first().ok_or("missing_operation")?;
    if entry.operation_id != operation_id
        || entry.kind != FrozenDocumentOperationKind::Content
        || entry.operation_seq != operation.operation_seq
        || entry.generation != Some(operation.generation)
        || entry.status != operation.status
        || entry.abandon_state != operation.abandon_state
        || entry.expected_revision != operation.expected_revision
    {
        return Err("stale_inspection".into());
    }
    let payload = state
        .store
        .frozen_document_operation_bytes(current, old, document, operation_id)
        .await
        .map_err(|error| error.to_string())?;
    if payload != operation.bytes {
        return Err("stale_inspection".into());
    }
    let draft = review.draft.ok_or("missing_draft")?;
    let (status, abandon, outcome_unknown) =
        operation_status(operation.status, operation.abandon_state)?;
    Ok(WorkspaceFrozenContentLocalInspection {
        current_binding: crate::to_contract(current.clone()),
        old_binding: crate::to_contract(old.clone()),
        document_id: document.into(),
        operation_id: operation_id.into(),
        operation_seq: operation.operation_seq.to_string(),
        operation_status: status,
        abandon_state: abandon,
        dependency: entry.dependency.clone(),
        expected_revision: operation.expected_revision,
        operation_generation: operation.generation.to_string(),
        local_generation: draft.generation.to_string(),
        outcome_unknown,
        base_revision: draft.base_revision,
        operation_hash: content_hash(&payload),
        operation_bytes_base64: STANDARD.encode(&payload),
        local_hash: content_hash(&draft.bytes),
        local_bytes_base64: STANDARD.encode(&draft.bytes),
        base_hash: content_hash(&draft.base_bytes),
        base_bytes_base64: STANDARD.encode(&draft.base_bytes),
        remote_evidence: WorkspaceFrozenContentRemoteEvidence::Unknown,
        remote_index_error: false,
    })
}

fn evidence_from_index(
    index: &Option<(notryx_domain::ChangeSeq, Vec<notryx_protocol::NodeState>)>,
    document: &str,
) -> WorkspaceFrozenContentRemoteEvidence {
    let Some((_, nodes)) = index else {
        return WorkspaceFrozenContentRemoteEvidence::Unknown;
    };
    let Some(node) = nodes
        .iter()
        .find(|node| node.node_id.to_string() == document)
    else {
        return WorkspaceFrozenContentRemoteEvidence::Unknown;
    };
    if node.last_change_seq.get() == 0 {
        return WorkspaceFrozenContentRemoteEvidence::Unknown;
    }
    if node.deleted {
        return WorkspaceFrozenContentRemoteEvidence::Deleted {
            change_seq: node.last_change_seq.get().to_string(),
        };
    }
    if let Some(size) = node
        .size
        .as_deref()
        .and_then(|size| size.parse::<u64>().ok())
        .filter(|size| *size > MAX_REMOTE_BYTES)
    {
        return WorkspaceFrozenContentRemoteEvidence::TooLarge {
            change_seq: node.last_change_seq.get().to_string(),
            size: size.to_string(),
        };
    }
    WorkspaceFrozenContentRemoteEvidence::Unknown
}

pub(crate) async fn review_frozen_content_local_for_renderer(
    runtime: &Runtime,
    request: WorkspaceFrozenContentLocalInspectRequest,
) -> Result<WorkspaceFrozenContentLocalInspection, String> {
    let current = crate::from_contract(&request.current_binding);
    let old = crate::from_contract(&request.old_binding);
    let (gate, generation, trusted, session, mut local, index) = {
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
        let session = state.session.clone();
        if session
            .as_ref()
            .is_some_and(|s| registration_binding(s.binding()) != current)
        {
            return Err("stale_registration".into());
        }
        let generation = state.transmission_gate.response_generation(&current);
        let trusted = state
            .transmission_gate
            .response_trusted(&current, generation);
        let mut local = local_sources(
            &mut state,
            &current,
            &old,
            &request.document_id,
            &request.operation_id,
        )
        .await?;
        let index = match state.store.sync_index(&current).await {
            Ok(index) => index,
            Err(_) => {
                local.remote_index_error = true;
                None
            }
        };
        (gate, generation, trusted, session, local, index)
    };
    if let (true, Some(session), Some((cursor, nodes))) = (trusted, &session, &index)
        && let Some(root) = nodes
            .iter()
            .find(|node| node.parent_id.is_none())
            .map(|node| node.node_id)
        && let Ok(info) = session.server_info().await
        && info.ready
        && info.head_seq == *cursor
        && info.root_id == root
        && info.journal_floor_seq.get() <= cursor.get()
    {
        local.remote_evidence = evidence_from_index(&index, &request.document_id);
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
        || state.transmission_gate.response_generation(&current) != generation
        || (trusted
            && !state
                .transmission_gate
                .response_trusted(&current, generation))
        || !match (&state.session, &session) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    {
        return Err("stale_registration".into());
    }
    let mut latest = local_sources(
        &mut state,
        &current,
        &old,
        &request.document_id,
        &request.operation_id,
    )
    .await?;
    let current_index = match state.store.sync_index(&current).await {
        Ok(index) => index,
        Err(_) => {
            latest.remote_index_error = true;
            None
        }
    };
    if !matches!(
        local.remote_evidence,
        WorkspaceFrozenContentRemoteEvidence::Unknown
    ) && current_index != index
    {
        return Err("stale_inspection".into());
    }
    latest.remote_evidence = local.remote_evidence.clone();
    if latest != local {
        return Err("stale_inspection".into());
    }
    local_fingerprint(&local)?;
    Ok(local)
}

#[tauri::command]
pub(crate) async fn workspace_review_frozen_content_local(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenContentLocalInspectRequest,
) -> Result<WorkspaceFrozenContentLocalInspection, String> {
    crate::check_caller(&webview)?;
    let result = review_frozen_content_local_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}

pub(crate) async fn dismiss_frozen_content_local_for_renderer(
    runtime: &Runtime,
    request: WorkspaceFrozenContentLocalDismissRequest,
) -> Result<WorkspaceFrozenContentDismissed, String> {
    let value = &request.inspected;
    let fingerprint = local_fingerprint(value)?;
    let current = crate::from_contract(&value.current_binding);
    let old = crate::from_contract(&value.old_binding);
    let result = WorkspaceFrozenContentDismissed {
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
        if value.remote_index_error {
            return Err("resync_required".into());
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
    let fresh = review_frozen_content_local_for_renderer(
        runtime,
        WorkspaceFrozenContentLocalInspectRequest {
            renderer_session_id: request.renderer_session_id.clone(),
            current_binding: value.current_binding.clone(),
            old_binding: value.old_binding.clone(),
            document_id: value.document_id.clone(),
            operation_id: value.operation_id.clone(),
        },
    )
    .await?;
    if fresh != *value {
        return Err("stale_inspection".into());
    }
    let (operation_status, abandon_state) = status_parts(value)?;
    let approval = FrozenContentDismissApproval {
        current: current.clone(),
        old: old.clone(),
        document: value.document_id.clone(),
        operation_id: value.operation_id.clone(),
        operation_seq: canonical_positive(&value.operation_seq)?,
        operation_generation: canonical_positive(&value.operation_generation)?,
        operation_status,
        abandon_state,
        dependency: value.dependency.clone(),
        expected_revision: value.expected_revision.clone(),
        payload_hash: value.operation_hash.clone(),
        payload: decoded_verified(&value.operation_bytes_base64, &value.operation_hash)?,
        draft_generation: canonical_positive(&value.local_generation)?,
        draft: decoded_verified(&value.local_bytes_base64, &value.local_hash)?,
        base: decoded_verified(&value.base_bytes_base64, &value.base_hash)?,
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
        .is_some()
    {
        return Ok(result);
    }
    let latest = local_sources(
        &mut state,
        &current,
        &old,
        &value.document_id,
        &value.operation_id,
    )
    .await?;
    let mut expected = value.clone();
    expected.remote_evidence = WorkspaceFrozenContentRemoteEvidence::Unknown;
    if latest != expected {
        return Err("stale_inspection".into());
    }
    let index = state
        .store
        .sync_index(&current)
        .await
        .map_err(|_| "resync_required")?;
    if !matches!(
        value.remote_evidence,
        WorkspaceFrozenContentRemoteEvidence::Unknown
    ) && evidence_from_index(&index, &value.document_id) != value.remote_evidence
    {
        return Err("stale_inspection".into());
    }
    state
        .store
        .dismiss_frozen_content(&approval)
        .await
        .map_err(|error| error.to_string())?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn workspace_dismiss_frozen_content_local(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenContentLocalDismissRequest,
) -> Result<WorkspaceFrozenContentDismissed, String> {
    crate::check_caller(&webview)?;
    let result = dismiss_frozen_content_local_for_renderer(&runtime, request).await?;
    crate::check_caller(&webview)?;
    Ok(result)
}
