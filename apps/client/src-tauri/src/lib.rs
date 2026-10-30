use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_runtime::{Binding, Session};
use notryx_client_store::{
    ClientStore, MAX_DOCUMENT_BYTES, OutboxAbandonState, OutboxFailureKind, OutboxStatus,
    OutboxSummary, RegistrationBinding, StoreError,
};
use notryx_domain::{NodeId, WorkspaceId};
use notryx_protocol::{
    InventoryPage, M0CloseRequest, M0OpenRequest, M0OpenedDocument, M0SaveRequest,
    M0SavedCheckpoint, ServerInfo, WorkspaceAbandonRequest, WorkspaceAbandonState,
    WorkspaceCacheWarning, WorkspaceDocumentOutboxState, WorkspaceDraft, WorkspaceDraftRequest,
    WorkspaceInventoryRequest, WorkspaceLocalRequest, WorkspaceOpenRequest,
    WorkspaceOpenedDocument, WorkspaceOutboxEntry, WorkspaceOutboxFailure, WorkspaceOutboxPage,
    WorkspaceOutboxRequest, WorkspaceOutboxStatus, WorkspacePairRequest, WorkspacePairStatus,
    WorkspaceQueueRequest, WorkspaceQueuedOperation, WorkspaceRegistration, WorkspaceSendResult,
    WorkspaceSyncState, WorkspaceSyncStatus,
};
use std::{collections::HashMap, sync::Arc};
use tauri::{Emitter, Manager, Webview};
#[cfg(not(target_os = "android"))]
use tauri::{WebviewUrl, WebviewWindowBuilder};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

#[cfg(test)]
mod auth_cache_tests;
mod conflict_snapshot_native;
mod create_native;
#[cfg(test)]
mod create_native_tests;
mod credential_native;
mod delete_native;
mod external_native;
#[cfg(test)]
mod frozen_content_abandoning_tests;
#[cfg(test)]
mod frozen_content_apply_contract_tests;
#[cfg(test)]
mod frozen_content_apply_role_tests;
#[cfg(test)]
mod frozen_content_base_revision_tests;
#[cfg(test)]
mod frozen_content_command_tests;
#[cfg(test)]
mod frozen_content_cross_epoch_apply_tests;
#[cfg(test)]
mod frozen_content_decision_status_tests;
#[cfg(test)]
mod frozen_content_dismiss_contract_tests;
#[cfg(test)]
mod frozen_content_epoch_tests;
#[cfg(test)]
mod frozen_content_finish_race_tests;
#[cfg(test)]
mod frozen_content_finish_root_tests;
#[cfg(test)]
mod frozen_content_inspection_race_tests;
#[cfg(test)]
mod frozen_content_inspection_tests;
#[cfg(test)]
mod frozen_content_local_dismiss_contract_tests;
#[cfg(test)]
mod frozen_content_local_index_failure_tests;
#[cfg(test)]
mod frozen_content_local_index_race_tests;
mod frozen_content_local_native;
#[cfg(test)]
mod frozen_content_local_review_gate_tests;
mod frozen_content_native;
mod frozen_create_native;
mod frozen_delete_native;
mod frozen_move_native;
mod gate;
mod migration_native;
mod move_native;
#[cfg(test)]
mod offline_restart_tests;
mod preview_native;
mod recovery_native;
#[cfg(test)]
mod remote_change_auth_extra_tests;
mod remote_change_dirty;
#[cfg(test)]
mod remote_change_notify_extra_tests;
#[cfg(test)]
mod remote_change_partial_page_tests;
#[cfg(test)]
mod remote_change_race_extra_tests;
#[cfg(test)]
mod remote_change_snapshot_error_tests;
#[cfg(test)]
mod remote_change_tests;
#[cfg(test)]
mod remote_delete_observation_tests;
mod remote_deleted_copy_native;
mod remote_refresh_native;
mod restore_native;
mod search_native;
mod send;
mod sync_bootstrap;
mod sync_catchup;
mod sync_content;
mod sync_lifecycle;
mod trash_native;

struct Runtime(Arc<Mutex<RuntimeState>>);
struct ForegroundHandle(Arc<sync_lifecycle::ForegroundSignal>);

struct RuntimeState {
    store: ClientStore,
    gate: gate::RequestGate,
    transmission_gate: send::TransmissionGate,
    initial_sync: Option<send::InitialSyncToken>,
    sync_task: Option<tokio::task::JoinHandle<()>>,
    sync_state: Option<WorkspaceSyncState>,
    foreground: Arc<sync_lifecycle::ForegroundSignal>,
    session: Option<Arc<Session>>,
    pending_pair: Option<Arc<Session>>,
    credential_io: Arc<Mutex<()>>,
    credential_identity_unsupported: Option<(RegistrationBinding, Uuid)>,
    product_sessions: HashMap<String, ProductSession>,
    remote_refresh_candidates: HashMap<String, remote_refresh_native::Candidate>,
    preview_limit: Arc<Semaphore>,
    preview_slots: Arc<Semaphore>,
    search_generation: u64,
    search_sequence: u64,
    search_session: Option<search_native::SearchSession>,
}

#[derive(Clone)]
struct ProductSession {
    document_id: String,
    binding: RegistrationBinding,
    kind: ProductSessionKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProductSessionKind {
    Active,
    Recovery,
    Readonly,
}

fn from_contract(binding: &WorkspaceRegistration) -> RegistrationBinding {
    RegistrationBinding {
        server_origin: binding.server_origin.clone(),
        workspace_id: binding.workspace_id.clone(),
        workspace_epoch: binding.workspace_epoch.clone(),
        device_id: binding.device_id.clone(),
        device_stream_id: binding.device_stream_id.clone(),
    }
}

fn to_contract(binding: RegistrationBinding) -> WorkspaceRegistration {
    WorkspaceRegistration {
        server_origin: binding.server_origin,
        workspace_id: binding.workspace_id,
        workspace_epoch: binding.workspace_epoch,
        device_id: binding.device_id,
        device_stream_id: binding.device_stream_id,
    }
}

fn outbox_entry(item: OutboxSummary) -> WorkspaceOutboxEntry {
    WorkspaceOutboxEntry {
        operation_id: item.operation_id,
        operation_seq: item.operation_seq.to_string(),
        document_id: item.document_id,
        generation: item.generation.to_string(),
        frozen: item.frozen,
        payload_retired: item.payload_retired,
        status: match item.status {
            OutboxStatus::Queued => WorkspaceOutboxStatus::Queued,
            OutboxStatus::Sent => WorkspaceOutboxStatus::Sent,
            OutboxStatus::Committed => WorkspaceOutboxStatus::Committed,
            OutboxStatus::Conflict => WorkspaceOutboxStatus::Conflict,
            OutboxStatus::Abandoning => WorkspaceOutboxStatus::Abandoning,
            OutboxStatus::Abandoned => WorkspaceOutboxStatus::Abandoned,
            OutboxStatus::Failed => WorkspaceOutboxStatus::Failed,
        },
        abandon_state: match item.abandon_state {
            OutboxAbandonState::None => WorkspaceAbandonState::None,
            OutboxAbandonState::Requested => WorkspaceAbandonState::Requested,
            OutboxAbandonState::Sent => WorkspaceAbandonState::Sent,
        },
        failure: item.failure_kind.map(|kind| match kind {
            OutboxFailureKind::NodeDeleted => WorkspaceOutboxFailure::NodeDeleted,
            OutboxFailureKind::RecoveryConflict => WorkspaceOutboxFailure::RecoveryConflict,
            OutboxFailureKind::NotFound => WorkspaceOutboxFailure::NotFound,
            OutboxFailureKind::NotApplied => WorkspaceOutboxFailure::NotApplied,
        }),
    }
}

impl RuntimeState {
    fn new(store: ClientStore, owner: String) -> Self {
        Self {
            store,
            gate: gate::RequestGate::new(owner),
            transmission_gate: send::TransmissionGate::default(),
            initial_sync: None,
            sync_task: None,
            sync_state: None,
            foreground: Arc::new(sync_lifecycle::ForegroundSignal::new()),
            session: None,
            pending_pair: None,
            credential_io: Arc::new(Mutex::new(())),
            credential_identity_unsupported: None,
            product_sessions: HashMap::new(),
            remote_refresh_candidates: HashMap::new(),
            preview_limit: Arc::new(Semaphore::new(1)),
            preview_slots: Arc::new(Semaphore::new(8)),
            search_generation: 0,
            search_sequence: 0,
            search_session: None,
        }
    }

    async fn document_state(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<WorkspaceDocumentOutboxState, String> {
        document_id
            .parse::<NodeId>()
            .map_err(|_| "invalid_document")?;
        let status = self
            .store
            .document_outbox_state(binding, document_id)
            .await
            .map_err(|error| error.to_string())?;
        let first_content_conflict = if status.has_conflict
            && self
                .store
                .current_registration()
                .await
                .map_err(|error| error.to_string())?
                .as_ref()
                == Some(binding)
        {
            self.store
                .first_content_conflict(binding, document_id)
                .await
                .map_err(|error| error.to_string())?
                .map(|item| notryx_protocol::WorkspaceContentConflictRef {
                    operation_id: item.operation_id,
                    operation_seq: item.operation_seq.to_string(),
                })
        } else {
            None
        };
        Ok(WorkspaceDocumentOutboxState {
            binding: to_contract(binding.clone()),
            document_id: document_id.into(),
            latest: status.latest.map(outbox_entry),
            first_content_conflict,
            has_conflict: status.has_conflict,
            has_failure: status.has_failure,
            confirmed_through: status.confirmed_through.to_string(),
        })
    }

    async fn queue_document(
        &mut self,
        request: &WorkspaceQueueRequest,
    ) -> Result<WorkspaceQueuedOperation, String> {
        request
            .document_id
            .parse::<NodeId>()
            .map_err(|_| "invalid_document")?;
        self.gate
            .check_queue(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let binding = registration_binding(self.session.as_ref().ok_or("not_connected")?.binding());
        let edit = self
            .product_sessions
            .get(&request.edit_session_id)
            .ok_or("stale_session")?;
        if edit.document_id != request.document_id
            || edit.binding != binding
            || edit.kind != ProductSessionKind::Active
        {
            return Err("stale_session".into());
        }
        let expected_generation = request
            .expected_generation
            .parse::<u64>()
            .map_err(|_| "invalid_generation")?;
        let item = self
            .store
            .queue_checkpoint(&binding, &request.document_id, expected_generation)
            .await
            .map_err(|error| match error {
                StoreError::GenerationMismatch => "stale_checkpoint".into(),
                other => other.to_string(),
            })?;
        Ok(WorkspaceQueuedOperation {
            operation_id: item.operation_id,
            operation_seq: item.operation_seq.to_string(),
            document_id: item.document_id,
            generation: item.generation.to_string(),
            committed: item.status == OutboxStatus::Committed,
        })
    }

    async fn local_drafts(&mut self) -> Result<Vec<WorkspaceDraft>, String> {
        let current = self
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?;
        let session_binding = self
            .session
            .as_ref()
            .map(|session| registration_binding(session.binding()));
        let drafts = self
            .store
            .list_registered_drafts()
            .await
            .map_err(|error| error.to_string())?;
        Ok(drafts
            .into_iter()
            .map(|draft| WorkspaceDraft {
                current_registration: current.as_ref() == Some(&draft.binding),
                has_session: session_binding.as_ref() == Some(&draft.binding),
                binding: to_contract(draft.binding),
                document_id: draft.document_id,
                generation: draft.generation.to_string(),
            })
            .collect())
    }

    async fn open_local_draft(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<WorkspaceOpenedDocument, String> {
        self.try_open_local_draft(binding, document_id)
            .await?
            .ok_or_else(|| "missing_draft".into())
    }

    async fn try_open_local_draft(
        &mut self,
        binding: &RegistrationBinding,
        document_id: &str,
    ) -> Result<Option<WorkspaceOpenedDocument>, String> {
        let Some((opened, active)) = self
            .store
            .try_open_registered_draft(binding, document_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let kind = if active {
            ProductSessionKind::Active
        } else {
            ProductSessionKind::Recovery
        };
        if let Err(error) = self.gate.open_session(opened.edit_session_id.clone()) {
            if active {
                let _ = self
                    .store
                    .close_registered_document(binding, document_id, &opened.edit_session_id)
                    .await;
            } else {
                let _ = self
                    .store
                    .close_recovery_draft(binding, document_id, &opened.edit_session_id)
                    .await;
            }
            return Err(error.into());
        }
        self.product_sessions.insert(
            opened.edit_session_id.clone(),
            ProductSession {
                document_id: document_id.into(),
                binding: binding.clone(),
                kind,
            },
        );
        Ok(Some(WorkspaceOpenedDocument {
            binding: to_contract(binding.clone()),
            document_id: document_id.into(),
            edit_session_id: opened.edit_session_id,
            generation: opened.generation.to_string(),
            bytes_base64: STANDARD.encode(opened.bytes),
            readonly: false,
            local_draft: true,
            has_session: self
                .session
                .as_ref()
                .is_some_and(|session| registration_binding(session.binding()) == *binding)
                && !self
                    .transmission_gate
                    .has_block(binding, send::TransmissionBlock::AuthRequired),
            cache_warning: None,
            open_notice: None,
        }))
    }

    async fn save_local_document(
        &mut self,
        request: &M0SaveRequest,
    ) -> Result<M0SavedCheckpoint, String> {
        if request.bytes_base64.len() > MAX_DOCUMENT_BYTES.div_ceil(3) * 4 {
            return Err("document_too_large".into());
        }
        let bytes = STANDARD
            .decode(&request.bytes_base64)
            .map_err(|_| "invalid_bytes")?;
        validate_edited_source(&bytes)?;
        let generation = request
            .generation
            .parse::<u64>()
            .map_err(|_| "invalid_generation")?;
        let session = self
            .product_sessions
            .get(&request.edit_session_id)
            .cloned()
            .ok_or("stale_session")?;
        if session.document_id != request.document_id {
            return Err("stale_session".into());
        }
        let saved = match session.kind {
            ProductSessionKind::Readonly => return Err("read_only_source".into()),
            ProductSessionKind::Active => {
                self.store
                    .save_registered_checkpoint(
                        &session.binding,
                        &request.document_id,
                        &request.edit_session_id,
                        generation,
                        &bytes,
                    )
                    .await
            }
            ProductSessionKind::Recovery => {
                self.store
                    .save_recovery_checkpoint(
                        &session.binding,
                        &request.document_id,
                        &request.edit_session_id,
                        generation,
                        &bytes,
                    )
                    .await
            }
        }
        .map_err(|error| error.to_string())?;
        Ok(M0SavedCheckpoint {
            edit_session_id: saved.edit_session_id,
            generation: saved.generation.to_string(),
        })
    }

    async fn close_local_document(&mut self, request: &M0CloseRequest) -> Result<(), String> {
        let session = self
            .product_sessions
            .get(&request.edit_session_id)
            .cloned()
            .ok_or("stale_session")?;
        if session.document_id != request.document_id {
            return Err("stale_session".into());
        }
        match session.kind {
            ProductSessionKind::Readonly => (),
            ProductSessionKind::Active => self
                .store
                .close_registered_document(
                    &session.binding,
                    &request.document_id,
                    &request.edit_session_id,
                )
                .await
                .map_err(|error| error.to_string())?,
            ProductSessionKind::Recovery => self
                .store
                .close_recovery_draft(
                    &session.binding,
                    &request.document_id,
                    &request.edit_session_id,
                )
                .await
                .map_err(|error| error.to_string())?,
        }
        self.gate
            .close_session(&request.edit_session_id)
            .map_err(str::to_owned)?;
        self.product_sessions.remove(&request.edit_session_id);
        self.remote_refresh_candidates
            .remove(&request.edit_session_id);
        Ok(())
    }
}

fn registration_binding(binding: &Binding) -> RegistrationBinding {
    RegistrationBinding {
        server_origin: binding.origin.clone(),
        workspace_id: binding.workspace_id.to_string(),
        workspace_epoch: binding.workspace_epoch.to_string(),
        device_id: binding.device_id.to_string(),
        device_stream_id: binding.device_stream_id.to_string(),
    }
}

fn check_caller(webview: &Webview) -> Result<(), String> {
    let url = webview.url().map_err(|_| "invalid_caller")?;
    let local = (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
        || (cfg!(debug_assertions)
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(1420));
    if webview.label() != "main" || !local || !matches!(url.path(), "" | "/" | "/index.html") {
        return Err("invalid_caller".into());
    }
    Ok(())
}

fn fixture(id: &str) -> Result<Vec<u8>, String> {
    let text = match id {
        "welcome" => "# Notryx\n\n이 문서는 M0 시험용 초안입니다.\n\n여기서 편집한 내용은 이 기기에만 저장됩니다.\n".to_owned(),
        "unicode" => "\u{feff}---\r\ncustom: preserve\r\n---\r\n# 한글 😀 e\u{301}\r\n\r\n```unknown\r\n[[합성 문서]]\r\n```\r\n<!-- keep -->".to_owned(),
        "mixed" => "# 혼합 줄바꿈\r\n원본\n보존\r\n".to_owned(),
        "non-utf8" => return Ok(vec![0xff, 0xfe, 0x00, 0xd8]),
        "100k" | "100k-b" | "5m" | "5m-b" => {
            let size = if id.starts_with("100k") { 100 * 1024 } else { MAX_DOCUMENT_BYTES };
            let mut text = "# 저장 성능 시험\n한글 😀 e\u{301}\n".to_owned();
            while text.len() + 65 <= size {
                text.push_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n");
            }
            text.push_str(&"x".repeat(size - text.len()));
            text
        }
        _ => return Err("unknown_fixture".into()),
    };
    Ok(text.into_bytes())
}

#[tauri::command]
async fn m0_renderer_session(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
) -> Result<String, String> {
    check_caller(&webview)?;
    Ok(runtime.0.lock().await.gate.owner().to_owned())
}

#[tauri::command]
async fn m0_open(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0OpenRequest,
) -> Result<M0OpenedDocument, String> {
    check_caller(&webview)?;
    let base = fixture(&request.document_id)?;
    let mut runtime = runtime.0.lock().await;
    runtime
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let opened = runtime
        .store
        .open_document(&request.document_id, &base, "m0-fixture-v1")
        .await
        .map_err(|e| e.to_string())?;
    if let Err(error) = runtime.gate.open_session(opened.edit_session_id.clone()) {
        let _ = runtime
            .store
            .close_document(&request.document_id, &opened.edit_session_id);
        return Err(error.into());
    }
    Ok(M0OpenedDocument {
        document_id: opened.document_id,
        edit_session_id: opened.edit_session_id,
        generation: opened.generation.to_string(),
        bytes_base64: STANDARD.encode(opened.bytes),
    })
}

#[tauri::command]
async fn m0_save(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0SaveRequest,
) -> Result<M0SavedCheckpoint, String> {
    check_caller(&webview)?;
    if request.document_id == "non-utf8" {
        return Err("read_only_source".into());
    }
    if request.bytes_base64.len() > MAX_DOCUMENT_BYTES.div_ceil(3) * 4 {
        return Err("document_too_large".into());
    }
    let bytes = STANDARD
        .decode(&request.bytes_base64)
        .map_err(|_| "invalid_bytes")?;
    validate_edited_source(&bytes)?;
    let generation = request
        .generation
        .parse::<u64>()
        .map_err(|_| "invalid_generation")?;
    let saved = runtime
        .0
        .lock()
        .await
        .store
        .save_checkpoint(
            &request.document_id,
            &request.edit_session_id,
            generation,
            &bytes,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(M0SavedCheckpoint {
        edit_session_id: saved.edit_session_id,
        generation: saved.generation.to_string(),
    })
}

#[tauri::command]
async fn m0_close(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0CloseRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let mut runtime = runtime.0.lock().await;
    runtime
        .store
        .close_document(&request.document_id, &request.edit_session_id)
        .map_err(|e| e.to_string())?;
    runtime
        .gate
        .close_session(&request.edit_session_id)
        .map_err(str::to_owned)
}

#[tauri::command]
async fn workspace_pair(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspacePairRequest,
) -> Result<ServerInfo, String> {
    check_caller(&webview)?;
    let workspace_id = request
        .workspace_id
        .parse::<WorkspaceId>()
        .map_err(|_| "invalid_workspace")?;
    let snapshot = begin_workspace_pair(&runtime, &request.renderer_session_id).await?;
    let session = match Session::pair(&request.origin, workspace_id, &request.pairing_secret).await
    {
        Ok(session) => Arc::new(session),
        Err(error) => {
            runtime
                .0
                .lock()
                .await
                .gate
                .fail_pair(&snapshot)
                .map_err(str::to_owned)?;
            return Err(match error {
                notryx_client_runtime::Error::InvalidOrigin => "invalid_origin",
                notryx_client_runtime::Error::InvalidRequest => "invalid_request",
                _ => "pairing_result_unknown",
            }
            .into());
        }
    };
    let info = session.server_info().await;
    let caller_valid = check_caller(&webview).is_ok();
    complete_pair_and_start(
        &runtime,
        snapshot,
        session,
        info,
        caller_valid,
        Some(webview.app_handle().clone()),
    )
    .await
}

async fn begin_workspace_pair(
    runtime: &Runtime,
    owner: &str,
) -> Result<gate::PairSnapshot, String> {
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    if state
        .store
        .pending_credential()
        .await
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("pairing_pending_adoption".into());
    }
    state.gate.begin_pair(owner).map_err(str::to_owned)
}

async fn complete_pair_and_start(
    runtime: &Runtime,
    snapshot: gate::PairSnapshot,
    session: Arc<Session>,
    info: Result<ServerInfo, notryx_client_runtime::Error>,
    caller_valid: bool,
    app: Option<tauri::AppHandle>,
) -> Result<ServerInfo, String> {
    let info = complete_pair(runtime, snapshot, session.clone(), info, caller_valid).await;
    if let Some(handle) = &app {
        let _ = handle.emit_to("main", "workspace-credential-status-changed", ());
    }
    let info = info?;
    sync_lifecycle::start_background_sync(runtime, session, app).await;
    Ok(info)
}

async fn complete_pair(
    runtime: &Runtime,
    snapshot: gate::PairSnapshot,
    session: Arc<Session>,
    info: Result<ServerInfo, notryx_client_runtime::Error>,
    caller_valid: bool,
) -> Result<ServerInfo, String> {
    {
        let mut state = runtime.0.lock().await;
        state.pending_pair = Some(session.clone());
        if !caller_valid || state.gate.validate_pair(&snapshot).is_err() {
            return Err("pairing_pending_adoption".into());
        }
    }
    let info = info.map_err(|_| "pairing_pending_adoption")?;
    credential_native::install_paired(runtime, &snapshot, session).await?;
    Ok(info)
}

#[tauri::command]
async fn workspace_sync_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<WorkspaceSyncState, String> {
    check_caller(&webview)?;
    sync_lifecycle::sync_state_for_renderer(&runtime, &request.renderer_session_id).await
}

#[tauri::command]
async fn workspace_credential_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<notryx_protocol::WorkspaceCredentialState, String> {
    check_caller(&webview)?;
    credential_status_for_renderer(&runtime, &request.renderer_session_id).await
}

async fn credential_status_for_renderer(
    runtime: &Runtime,
    owner: &str,
) -> Result<notryx_protocol::WorkspaceCredentialState, String> {
    use notryx_protocol::WorkspaceCredentialStatus;
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    let status = if state
        .store
        .pending_credential()
        .await
        .map_err(|error| error.to_string())?
        .is_some()
    {
        WorkspaceCredentialStatus::Pending
    } else {
        match state
            .store
            .active_credential()
            .await
            .map_err(|error| error.to_string())?
        {
            None => WorkspaceCredentialStatus::None,
            Some((binding, None))
                if state
                    .session
                    .as_ref()
                    .is_some_and(|session| registration_binding(session.binding()) == binding) =>
            {
                WorkspaceCredentialStatus::SessionOnly
            }
            Some((_, None)) => WorkspaceCredentialStatus::RegistrationRequired,
            Some((binding, Some(id)))
                if state.credential_identity_unsupported.as_ref().is_some_and(
                    |(stored_binding, stored_id)| stored_binding == &binding && *stored_id == id,
                ) =>
            {
                WorkspaceCredentialStatus::IdentityUnsupported
            }
            Some((binding, Some(_)))
                if state
                    .session
                    .as_ref()
                    .is_some_and(|session| registration_binding(session.binding()) == binding) =>
            {
                WorkspaceCredentialStatus::Secure
            }
            Some((_, Some(_))) => WorkspaceCredentialStatus::Unavailable,
        }
    };
    Ok(notryx_protocol::WorkspaceCredentialState { status })
}

#[tauri::command]
async fn workspace_pair_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<WorkspacePairStatus, String> {
    check_caller(&webview)?;
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if state
        .store
        .pending_credential()
        .await
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Ok(WorkspacePairStatus::PendingAdoption);
    }
    match state
        .gate
        .pair_status(&request.renderer_session_id, state.pending_pair.is_some())
        .map_err(str::to_owned)?
    {
        "none" => Ok(WorkspacePairStatus::None),
        "in_progress" => Ok(WorkspacePairStatus::InProgress),
        "pending_adoption" => Ok(WorkspacePairStatus::PendingAdoption),
        _ => Err("invalid_pair_status".into()),
    }
}

#[tauri::command]
async fn workspace_adopt_pending_pair(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<ServerInfo, String> {
    check_caller(&webview)?;
    if runtime.0.lock().await.pending_pair.is_none() {
        credential_native::recover_pending(&runtime, &request.renderer_session_id).await?;
        check_caller(&webview)?;
    }
    let (snapshot, session) = {
        let mut state = runtime.0.lock().await;
        let snapshot = state
            .gate
            .adopt_pair(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.pending_pair.take().ok_or("pairing_in_progress")?;
        (snapshot, session)
    };
    let info = session.server_info().await;
    let caller_valid = check_caller(&webview).is_ok();
    complete_pair_and_start(
        &runtime,
        snapshot,
        session,
        info,
        caller_valid,
        Some(webview.app_handle().clone()),
    )
    .await
}

#[tauri::command]
async fn workspace_discard_pending_pair(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if let Some(pending) = state
        .store
        .pending_credential()
        .await
        .map_err(|error| error.to_string())?
    {
        let _ = pending;
        drop(state);
        credential_native::discard_for_renderer(&runtime, &request.renderer_session_id).await?;
        check_caller(&webview)?;
        let _ = webview.emit("workspace-credential-status-changed", ());
        let resume_runtime = Runtime(runtime.0.clone());
        let handle = webview.app_handle().clone();
        tauri::async_runtime::spawn(async move {
            credential_native::resume(&resume_runtime, handle).await;
        });
        return Ok(());
    }
    let snapshot = state
        .gate
        .adopt_pair(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    if state.pending_pair.is_none() {
        return Err("pairing_in_progress".into());
    }
    state.gate.fail_pair(&snapshot).map_err(str::to_owned)?;
    state.pending_pair = None;
    Ok(())
}

#[tauri::command]
async fn workspace_inventory(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceInventoryRequest,
) -> Result<InventoryPage, String> {
    check_caller(&webview)?;
    let after = request
        .after_id
        .as_deref()
        .map(str::parse::<NodeId>)
        .transpose()
        .map_err(|_| "invalid_node")?;
    let page = inventory_for_renderer(&runtime, &request.renderer_session_id, after).await;
    check_caller(&webview)?;
    let notify_auth = {
        let state = runtime.0.lock().await;
        state.session.as_ref().is_some_and(|session| {
            state.transmission_gate.has_block(
                &registration_binding(session.binding()),
                send::TransmissionBlock::AuthRequired,
            )
        })
    };
    if notify_auth {
        let _ = webview.emit("workspace-sync-status-changed", ());
    }
    page
}

async fn local_inventory(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
    after: Option<NodeId>,
) -> Result<InventoryPage, String> {
    let (_, nodes) = state
        .store
        .sync_index(binding)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("not_connected")?;
    let after = after.map(|value| value.to_string());
    let mut nodes = nodes
        .into_iter()
        .filter(|node| {
            !node.deleted
                && after
                    .as_ref()
                    .is_none_or(|after| node.node_id.to_string() > *after)
        })
        .take(101)
        .collect::<Vec<_>>();
    let next_after_id = if nodes.len() > 100 {
        nodes.pop();
        nodes.last().map(|node| node.node_id)
    } else {
        None
    };
    Ok(InventoryPage {
        workspace_epoch: binding
            .workspace_epoch
            .parse()
            .map_err(|_| "invalid_registration")?,
        nodes,
        next_after_id,
    })
}

async fn inventory_for_renderer(
    runtime: &Runtime,
    owner: &str,
    after: Option<NodeId>,
) -> Result<InventoryPage, String> {
    let (snapshot, session, generation) = {
        let mut state = runtime.0.lock().await;
        let snapshot = state.gate.snapshot(owner).map_err(str::to_owned)?;
        let session = state.session.clone();
        let local = session.as_ref().is_none_or(|session| {
            state
                .transmission_gate
                .blocked_status(&registration_binding(session.binding()))
                == notryx_protocol::WorkspaceSendStatus::AuthRequired
        });
        if local {
            let binding = state
                .store
                .current_registration()
                .await
                .map_err(|error| error.to_string())?
                .ok_or("not_connected")?;
            if session
                .as_ref()
                .is_some_and(|session| registration_binding(session.binding()) != binding)
            {
                return Err("stale_registration".into());
            }
            return local_inventory(&mut state, &binding, after).await;
        }
        let session = session.expect("a remote inventory requires a session");
        let generation = state
            .transmission_gate
            .response_generation(&registration_binding(session.binding()));
        (snapshot, session, generation)
    };
    let page = session.inventory(after).await;
    let mut state = runtime.0.lock().await;
    state.gate.validate(&snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &session))
    {
        return Err("stale_registration".into());
    }
    match page {
        Ok(page) => {
            let binding = registration_binding(session.binding());
            if state.transmission_gate.response_generation(&binding) != generation {
                return Err("stale_sync".into());
            }
            Ok(page)
        }
        Err(notryx_client_runtime::Error::Unauthorized) => {
            let binding = registration_binding(session.binding());
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
            if !state
                .transmission_gate
                .has_block(&binding, send::TransmissionBlock::AuthRequired)
            {
                state
                    .transmission_gate
                    .block(&binding, send::TransmissionBlock::AuthRequired);
            }
            state.sync_state = Some(WorkspaceSyncState {
                binding: to_contract(binding.clone()),
                status: WorkspaceSyncStatus::AuthRequired,
            });
            if state.transmission_gate.blocked_status(&binding)
                != notryx_protocol::WorkspaceSendStatus::AuthRequired
            {
                return Err("stale_sync".into());
            }
            local_inventory(&mut state, &binding, after).await
        }
        Err(notryx_client_runtime::Error::Network | notryx_client_runtime::Error::Unavailable) => {
            let binding = registration_binding(session.binding());
            if state.transmission_gate.response_generation(&binding) != generation {
                return Err("stale_sync".into());
            }
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
            local_inventory(&mut state, &binding, after).await
        }
        Err(error) => Err(format!("{error:?}")),
    }
}

#[tauri::command]
async fn workspace_search(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceSearchRequest,
) -> Result<notryx_protocol::WorkspaceSearchPage, String> {
    check_caller(&webview)?;
    let result = search_native::workspace_search_for_renderer(&runtime.0, &request).await;
    check_caller(&webview)?;
    result
}

#[tauri::command]
async fn workspace_cancel_search(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceSearchCancelRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    search_native::cancel_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &from_contract(&request.binding),
        &request.request_id,
    )
    .await
}

#[tauri::command]
async fn workspace_trash(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceInventoryRequest,
) -> Result<notryx_protocol::TrashPage, String> {
    check_caller(&webview)?;
    let after = request
        .after_id
        .as_deref()
        .map(str::parse::<NodeId>)
        .transpose()
        .map_err(|_| "invalid_node")?;
    let result = trash_native::trash_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        after,
        100,
        #[cfg(test)]
        None,
    )
    .await;
    check_caller(&webview)?;
    if matches!(&result, Err(error) if matches!(error.as_str(), "Binding" | "EpochMismatch" | "Contract" | "TooLarge"))
    {
        let _ = webview.emit("workspace-sync-status-changed", ());
    }
    result
}

#[tauri::command]
async fn workspace_open(
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

enum WorkspaceOpenStart {
    Draft(WorkspaceOpenedDocument),
    Remote(WorkspaceRemoteOpen),
    Stale(WorkspaceRemoteOpen),
}

#[derive(Clone)]
struct WorkspaceRemoteOpen {
    snapshot: gate::RequestSnapshot,
    session: Arc<Session>,
    generation: u64,
    node: notryx_protocol::NodeState,
}

async fn refresh_open_node(
    runtime: &Runtime,
    remote: &WorkspaceRemoteOpen,
) -> Result<Option<notryx_protocol::NodeState>, String> {
    let binding = registration_binding(remote.session.binding());
    {
        let state =
            sync_bootstrap::checked_state(runtime, &remote.snapshot, &remote.session).await?;
        if !state
            .transmission_gate
            .response_trusted(&binding, remote.generation)
        {
            return Err("stale_sync".into());
        }
    }
    sync_catchup::catch_up_for_generation(
        runtime,
        Some(&remote.snapshot),
        &remote.session,
        Some(remote.generation),
    )
    .await
    .map_err(|error| error.message())?;
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

enum WorkspaceRefresh {
    Draft(WorkspaceOpenedDocument),
    Node(notryx_protocol::NodeState),
    Deleted,
}

async fn finish_open_refresh(
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

async fn begin_workspace_open(
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

async fn finish_workspace_open(
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
        Err(notryx_client_runtime::Error::Network) => {
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

#[tauri::command]
async fn workspace_save(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0SaveRequest,
) -> Result<M0SavedCheckpoint, String> {
    check_caller(&webview)?;
    runtime.0.lock().await.save_local_document(&request).await
}

#[tauri::command]
async fn workspace_close(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: M0CloseRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    runtime.0.lock().await.close_local_document(&request).await
}

#[tauri::command]
async fn workspace_local_drafts(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<Vec<WorkspaceDraft>, String> {
    check_caller(&webview)?;
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    state.local_drafts().await
}

#[tauri::command]
async fn workspace_open_draft(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDraftRequest,
) -> Result<WorkspaceOpenedDocument, String> {
    check_caller(&webview)?;
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    state
        .open_local_draft(&from_contract(&request.binding), &request.document_id)
        .await
}

#[tauri::command]
async fn workspace_queue(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceQueueRequest,
) -> Result<WorkspaceQueuedOperation, String> {
    check_caller(&webview)?;
    runtime.0.lock().await.queue_document(&request).await
}

#[tauri::command]
async fn workspace_request_abandon(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceAbandonRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    request
        .operation_id
        .parse::<notryx_domain::OperationId>()
        .map_err(|_| "invalid_operation")?;
    let mut state = runtime.0.lock().await;
    state
        .gate
        .check_queue(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let binding = registration_binding(state.session.as_ref().ok_or("not_connected")?.binding());
    state
        .store
        .request_outbox_abandon(&binding, &request.operation_id)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn workspace_send_next(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<WorkspaceSendResult, String> {
    check_caller(&webview)?;
    let result = send::send_one_with_app(
        runtime.0.clone(),
        &request.renderer_session_id,
        Some(webview.app_handle()),
    )
    .await;
    if result.as_ref().map_or(true, |value| {
        matches!(
            value.status,
            notryx_protocol::WorkspaceSendStatus::NeedsReview
                | notryx_protocol::WorkspaceSendStatus::SyncPending
                | notryx_protocol::WorkspaceSendStatus::AuthRequired
        )
    }) {
        let _ = webview
            .app_handle()
            .emit_to("main", "workspace-sync-status-changed", ());
    }
    result
}

#[tauri::command]
async fn workspace_outbox(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceOutboxRequest,
) -> Result<WorkspaceOutboxPage, String> {
    check_caller(&webview)?;
    let after = request
        .after_seq
        .parse::<u64>()
        .map_err(|_| "invalid_sequence")?;
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    let page = state
        .store
        .list_outbox(&from_contract(&request.binding), after, request.limit)
        .await
        .map_err(|error| error.to_string())?;
    Ok(WorkspaceOutboxPage {
        entries: page.entries.into_iter().map(outbox_entry).collect(),
        next_after_seq: page.next_after_seq.map(|seq| seq.to_string()),
    })
}

#[tauri::command]
async fn workspace_document_outbox(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDraftRequest,
) -> Result<WorkspaceDocumentOutboxState, String> {
    check_caller(&webview)?;
    let mut state = runtime.0.lock().await;
    state
        .gate
        .snapshot(&request.renderer_session_id)
        .map_err(str::to_owned)?;
    state
        .document_state(&from_contract(&request.binding), &request.document_id)
        .await
}

#[tauri::command]
fn m0_exit(webview: Webview, app: tauri::AppHandle) -> Result<(), String> {
    check_caller(&webview)?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn m0_open_isolation(webview: Webview, app: tauri::AppHandle) -> Result<(), String> {
    check_caller(&webview)?;
    #[cfg(not(target_os = "android"))]
    if app.get_webview_window("renderer-probe").is_none() {
        WebviewWindowBuilder::new(
            &app,
            "renderer-probe",
            WebviewUrl::App("isolation.html".into()),
        )
        .title("Notryx renderer probe")
        .on_navigation(|url| {
            url.path() == "/isolation.html"
                && ((url.scheme() == "tauri" && url.host_str() == Some("localhost"))
                    || (cfg!(debug_assertions)
                        && url.scheme() == "http"
                        && url.host_str() == Some("127.0.0.1")
                        && url.port() == Some(1420)))
        })
        .build()
        .map_err(|_| "isolation_window_failed")?;
    }
    #[cfg(target_os = "android")]
    let _ = app;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let directory = std::env::var_os("NOTRYX_M0_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or(app.path().app_data_dir()?);
            std::fs::create_dir_all(&directory)?;
            let path = directory.join("drafts.sqlite3");
            let startup = match tauri::async_runtime::block_on(ClientStore::open(&path)) {
                Ok(store) => {
                    let state = RuntimeState::new(store, Uuid::new_v4().to_string());
                    app.manage(ForegroundHandle(state.foreground.clone()));
                    let runtime = Runtime(Arc::new(Mutex::new(state)));
                    let resume_runtime = Runtime(runtime.0.clone());
                    let handle = app.handle().clone();
                    app.manage(runtime);
                    tauri::async_runtime::spawn(async move {
                        credential_native::resume(&resume_runtime, handle).await;
                    });
                    migration_native::Startup::Ready
                }
                Err(StoreError::StoreLocked) => migration_native::Startup::StoreLocked,
                Err(StoreError::StorePathChanged) => migration_native::Startup::Unavailable,
                Err(_) => match tauri::async_runtime::block_on(
                    notryx_client_store::RecoveryStore::open(&path),
                ) {
                    Ok(store) => migration_native::Startup::Recovery(Arc::new(Mutex::new(
                        migration_native::MigrationState::new(store, Uuid::new_v4().to_string()),
                    ))),
                    Err(StoreError::StoreLocked) => migration_native::Startup::StoreLocked,
                    Err(_) => migration_native::Startup::Unavailable,
                },
            };
            app.manage(startup);
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if webview.label() == "main"
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
                && let Some(startup) = webview.try_state::<migration_native::Startup>()
                && let migration_native::Startup::Recovery(state) = startup.inner()
            {
                let mut state = tauri::async_runtime::block_on(state.lock());
                state.replace_owner(Uuid::new_v4().to_string());
            }
            if webview.label() == "main"
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
                && let Some(runtime) = webview.try_state::<Runtime>()
            {
                // Invalidate before the new page can invoke IPC, including queued opens from the old page.
                let mut runtime = tauri::async_runtime::block_on(runtime.0.lock());
                runtime.store.reset_edit_sessions();
                runtime.product_sessions.clear();
                runtime.remote_refresh_candidates.clear();
                search_native::replace_renderer(&mut runtime, Uuid::new_v4().to_string());
            }
        })
        .on_window_event(|window, event| {
            if window.label() == "main"
                && let tauri::WindowEvent::Focused(active) = event
                && let Some(foreground) = window.try_state::<ForegroundHandle>()
            {
                foreground.0.set(*active);
            }
            if window.label() == "main"
                && window.try_state::<Runtime>().is_some()
                && let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.emit("close-requested", ());
            }
        })
        .invoke_handler(tauri::generate_handler![
            m0_renderer_session,
            m0_open,
            m0_save,
            m0_close,
            m0_exit,
            m0_open_isolation,
            workspace_pair,
            workspace_sync_status,
            workspace_credential_status,
            workspace_inventory,
            workspace_search,
            workspace_cancel_search,
            workspace_trash,
            workspace_open,
            remote_refresh_native::workspace_prepare_remote_refresh,
            remote_refresh_native::workspace_commit_remote_refresh,
            remote_change_dirty::workspace_inspect_dirty_remote,
            remote_deleted_copy_native::workspace_prepare_deleted_draft_copy,
            remote_deleted_copy_native::workspace_confirm_deleted_draft_copy,
            remote_deleted_copy_native::workspace_deleted_draft_copy_status,
            remote_deleted_copy_native::workspace_progress_deleted_draft_copy,
            remote_deleted_copy_native::workspace_reopen_deleted_draft_copy,
            remote_deleted_copy_native::workspace_request_deleted_draft_copy_followup_abandon,
            remote_deleted_copy_native::workspace_reconfirm_deleted_draft_copy,
            conflict_snapshot_native::workspace_prepare_content_conflict,
            conflict_snapshot_native::workspace_content_conflict_resolution_status,
            conflict_snapshot_native::workspace_choose_remote_content_conflict,
            conflict_snapshot_native::workspace_confirm_content_conflict_copy,
            conflict_snapshot_native::workspace_confirm_content_conflict_merge,
            conflict_snapshot_native::workspace_queue_content_conflict_merge,
            conflict_snapshot_native::workspace_content_conflict_merge_status,
            conflict_snapshot_native::workspace_content_conflict_merge_followup_status,
            conflict_snapshot_native::workspace_request_content_conflict_merge_followup_abandon,
            conflict_snapshot_native::workspace_progress_content_conflict_copy,
            conflict_snapshot_native::workspace_content_conflict_copy_status,
            conflict_snapshot_native::workspace_retry_content_conflict_copy,
            conflict_snapshot_native::workspace_continue_remote_content_conflict,
            conflict_snapshot_native::workspace_complete_pending_remote_content_conflict,
            conflict_snapshot_native::workspace_reopen_content_conflict_checkpoint,
            workspace_save,
            workspace_close,
            workspace_local_drafts,
            workspace_open_draft,
            workspace_pair_status,
            workspace_adopt_pending_pair,
            workspace_discard_pending_pair,
            workspace_queue,
            restore_native::workspace_queue_restore,
            restore_native::workspace_restore_status,
            restore_native::workspace_request_restore_abandon,
            delete_native::workspace_queue_delete,
            delete_native::workspace_delete_status,
            move_native::workspace_queue_move,
            move_native::workspace_move_status,
            move_native::workspace_request_move_abandon,
            delete_native::workspace_request_delete_abandon,
            create_native::workspace_queue_create,
            create_native::workspace_create_parents,
            create_native::workspace_create_status,
            create_native::workspace_request_create_abandon,
            frozen_delete_native::workspace_frozen_deletes,
            frozen_delete_native::workspace_resolve_frozen_delete,
            frozen_move_native::workspace_frozen_moves,
            frozen_content_native::workspace_inspect_frozen_content,
            frozen_content_native::workspace_apply_frozen_content,
            frozen_content_native::workspace_inspect_frozen_content_cross_epoch,
            frozen_content_native::workspace_apply_frozen_content_cross_epoch,
            frozen_content_native::workspace_dismiss_frozen_content,
            frozen_content_local_native::workspace_review_frozen_content_local,
            frozen_content_local_native::workspace_dismiss_frozen_content_local,
            frozen_content_native::workspace_frozen_content_decision_status,
            frozen_move_native::workspace_resolve_frozen_move,
            frozen_create_native::workspace_frozen_creates,
            frozen_create_native::workspace_resolve_frozen_create,
            workspace_request_abandon,
            workspace_send_next,
            workspace_outbox,
            workspace_document_outbox,
            migration_native::client_startup_status,
            migration_native::migration_inventory,
            migration_native::migration_source,
            migration_native::migration_export,
            preview_native::workspace_preview_image,
            external_native::workspace_open_external,
            recovery_native::workspace_export_source,
            recovery_native::workspace_copy_source
        ])
        .run(tauri::generate_context!())
        .expect("client runtime failed");
}

fn validate_edited_source(bytes: &[u8]) -> Result<(), String> {
    std::str::from_utf8(bytes).map_err(|_| "invalid_utf8")?;
    if bytes.contains(&b'\r') {
        return Err("non_lf_source".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edited_source_requires_utf8_and_lf_at_the_native_boundary() {
        assert!(validate_edited_source("\u{feff}# 한글 😀\n<!-- keep -->".as_bytes()).is_ok());
        assert_eq!(
            validate_edited_source(b"a\r\nb"),
            Err("non_lf_source".into())
        );
        assert_eq!(validate_edited_source(b"a\rb"), Err("non_lf_source".into()));
        assert_eq!(validate_edited_source(&[0xff]), Err("invalid_utf8".into()));
    }

    #[tokio::test]
    async fn local_drafts_survive_restart_and_can_be_edited_offline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("draft.sqlite3");
        let binding = RegistrationBinding {
            server_origin: "https://example.invalid".into(),
            workspace_id: Uuid::new_v4().to_string(),
            workspace_epoch: Uuid::new_v4().to_string(),
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
        };
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        let open = store
            .open_registered_document(&binding, "document", b"base", "rev")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, "document", &open.edit_session_id, 1, b"draft")
            .await
            .unwrap();
        store
            .close_registered_document(&binding, "document", &open.edit_session_id)
            .await
            .unwrap();
        store.close().await.unwrap();

        let store = ClientStore::open(&path).await.unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        let listing = state.local_drafts().await.unwrap();
        assert_eq!(listing.len(), 1);
        assert!(listing[0].current_registration);
        assert!(!listing[0].has_session);
        let pair = state.gate.begin_pair("owner").unwrap();
        assert_eq!(
            state
                .open_local_draft(&binding, "document")
                .await
                .err()
                .as_deref(),
            Some("pairing_in_progress")
        );
        state.gate.fail_pair(&pair).unwrap();
        let opened = state.open_local_draft(&binding, "document").await.unwrap();
        assert!(opened.local_draft);
        assert_eq!(opened.binding, to_contract(binding.clone()));
        assert_eq!(STANDARD.decode(&opened.bytes_base64).unwrap(), b"draft");
        assert_eq!(opened.generation, "1");
        state
            .save_local_document(&M0SaveRequest {
                document_id: "document".into(),
                edit_session_id: opened.edit_session_id.clone(),
                generation: "2".into(),
                bytes_base64: STANDARD.encode(b"new draft"),
            })
            .await
            .unwrap();
        state
            .close_local_document(&M0CloseRequest {
                document_id: "document".into(),
                edit_session_id: opened.edit_session_id,
            })
            .await
            .unwrap();
        let draft = state
            .store
            .read_registered_checkpoint(&binding, "document")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"new draft");
        assert_eq!(draft.base_bytes, b"base");
        let mut next = binding.clone();
        next.device_id = Uuid::new_v4().to_string();
        next.device_stream_id = Uuid::new_v4().to_string();
        state.store.activate_registration(&next).await.unwrap();
        let listing = state.local_drafts().await.unwrap();
        assert!(!listing[0].current_registration);
        assert!(!listing[0].has_session);
        let recovered = state.open_local_draft(&binding, "document").await.unwrap();
        assert_eq!(
            STANDARD.decode(&recovered.bytes_base64).unwrap(),
            b"new draft"
        );
        state
            .save_local_document(&M0SaveRequest {
                document_id: "document".into(),
                edit_session_id: recovered.edit_session_id.clone(),
                generation: "3".into(),
                bytes_base64: STANDARD.encode(b"recovery draft"),
            })
            .await
            .unwrap();
        state
            .close_local_document(&M0CloseRequest {
                document_id: "document".into(),
                edit_session_id: recovered.edit_session_id,
            })
            .await
            .unwrap();
        let draft = state
            .store
            .read_registered_checkpoint(&binding, "document")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"recovery draft");
        assert_eq!(draft.base_bytes, b"base");
    }
}
