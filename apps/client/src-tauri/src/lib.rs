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
    WorkspaceDocumentOutboxState, WorkspaceDraft, WorkspaceDraftRequest, WorkspaceInventoryRequest,
    WorkspaceLocalRequest, WorkspaceOpenRequest, WorkspaceOpenedDocument, WorkspaceOutboxEntry,
    WorkspaceOutboxFailure, WorkspaceOutboxPage, WorkspaceOutboxRequest, WorkspaceOutboxStatus,
    WorkspacePairRequest, WorkspacePairStatus, WorkspaceQueueRequest, WorkspaceQueuedOperation,
    WorkspaceRegistration, WorkspaceSendResult,
};
use std::{collections::HashMap, sync::Arc};
use tauri::{Emitter, Manager, Webview};
#[cfg(not(target_os = "android"))]
use tauri::{WebviewUrl, WebviewWindowBuilder};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

mod external_native;
mod gate;
mod migration_native;
mod preview_native;
mod recovery_native;
mod send;

struct Runtime(Arc<Mutex<RuntimeState>>);

struct RuntimeState {
    store: ClientStore,
    gate: gate::RequestGate,
    session: Option<Arc<Session>>,
    pending_pair: Option<Arc<Session>>,
    product_sessions: HashMap<String, ProductSession>,
    preview_limit: Arc<Semaphore>,
    preview_slots: Arc<Semaphore>,
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
            session: None,
            pending_pair: None,
            product_sessions: HashMap::new(),
            preview_limit: Arc::new(Semaphore::new(1)),
            preview_slots: Arc::new(Semaphore::new(8)),
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
        Ok(WorkspaceDocumentOutboxState {
            binding: to_contract(binding.clone()),
            document_id: document_id.into(),
            latest: status.latest.map(outbox_entry),
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
    let snapshot = {
        let mut state = runtime.0.lock().await;
        state
            .gate
            .begin_pair(&request.renderer_session_id)
            .map_err(str::to_owned)?
    };
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
    complete_pair(&runtime, snapshot, session, info, caller_valid).await
}

async fn complete_pair(
    runtime: &Runtime,
    snapshot: gate::PairSnapshot,
    session: Arc<Session>,
    info: Result<ServerInfo, notryx_client_runtime::Error>,
    caller_valid: bool,
) -> Result<ServerInfo, String> {
    let mut state = runtime.0.lock().await;
    state.pending_pair = Some(session.clone());
    if !caller_valid || state.gate.validate_pair(&snapshot).is_err() {
        return Err("pairing_pending_adoption".into());
    }
    let info = info.map_err(|_| "pairing_pending_adoption")?;
    let binding = registration_binding(session.binding());
    if state.store.activate_registration(&binding).await.is_err() {
        return Err("pairing_pending_adoption".into());
    }
    state.gate.commit_pair(&snapshot).map_err(str::to_owned)?;
    state.session = Some(session);
    state.pending_pair = None;
    Ok(info)
}

#[tauri::command]
async fn workspace_pair_status(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<WorkspacePairStatus, String> {
    check_caller(&webview)?;
    let state = runtime.0.lock().await;
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
    complete_pair(&runtime, snapshot, session, info, caller_valid).await
}

#[tauri::command]
async fn workspace_discard_pending_pair(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceLocalRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let mut state = runtime.0.lock().await;
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
    let (snapshot, session) = {
        let state = runtime.0.lock().await;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        (snapshot, state.session.clone().ok_or("not_connected")?)
    };
    let page = session
        .inventory(after)
        .await
        .map_err(|error| format!("{error:?}"))?;
    check_caller(&webview)?;
    let state = runtime.0.lock().await;
    state.gate.validate(&snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &session))
    {
        return Err("stale_registration".into());
    }
    Ok(page)
}

#[tauri::command]
async fn workspace_open(
    webview: Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceOpenRequest,
) -> Result<WorkspaceOpenedDocument, String> {
    check_caller(&webview)?;
    let (snapshot, session) = {
        let mut state = runtime.0.lock().await;
        let snapshot = state
            .gate
            .snapshot(&request.renderer_session_id)
            .map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        let binding = registration_binding(session.binding());
        let document_id = request.node.node_id.to_string();
        if let Some(opened) = state.try_open_local_draft(&binding, &document_id).await? {
            return Ok(opened);
        }
        (snapshot, session)
    };
    let bytes = session
        .content(&request.node)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let document_id = request.node.node_id.to_string();
    let revision = request
        .node
        .content_rev
        .as_ref()
        .ok_or("invalid_node")?
        .as_str()
        .to_owned();
    let binding = registration_binding(session.binding());
    check_caller(&webview)?;
    let mut state = runtime.0.lock().await;
    state.gate.validate(&snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &session))
    {
        return Err("stale_registration".into());
    }
    if let Some(opened) = state.try_open_local_draft(&binding, &document_id).await? {
        return Ok(opened);
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
    Ok(WorkspaceOpenedDocument {
        binding: to_contract(binding),
        document_id,
        edit_session_id,
        generation: generation.to_string(),
        bytes_base64: STANDARD.encode(bytes),
        readonly,
        local_draft: false,
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
    send::send_one(runtime.0.clone(), &request.renderer_session_id).await
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
                    app.manage(Runtime(Arc::new(Mutex::new(RuntimeState::new(
                        store,
                        Uuid::new_v4().to_string(),
                    )))));
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
                runtime.gate.replace_owner(Uuid::new_v4().to_string());
            }
        })
        .on_window_event(|window, event| {
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
            workspace_inventory,
            workspace_open,
            workspace_save,
            workspace_close,
            workspace_local_drafts,
            workspace_open_draft,
            workspace_pair_status,
            workspace_adopt_pending_pair,
            workspace_discard_pending_pair,
            workspace_queue,
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
