use crate::{RuntimeState, gate::SendSnapshot, registration_binding};
use notryx_client_runtime::{
    AbandonMutation, ContentMutation, CreateMutation, DeleteMutation, MoveMutation,
    OperationLookup, RestoreMutation, Session,
};
use notryx_client_store::{
    CreateOutboxOperation, DeleteOutboxOperation, MoveOutboxOperation, OutboxAbandonState,
    OutboxAbandoned, OutboxCommit, OutboxConflict, OutboxFailure, OutboxFailureKind,
    OutboxOperation, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    RestoreOutboxOperation, StoreError,
};
use notryx_domain::ContentRevision;
use notryx_protocol::{OperationReceipt, WorkspaceSendResult, WorkspaceSendStatus};
use std::future::Future;
use std::sync::Arc;
use std::task::Poll;
use tauri::Emitter;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum TransmissionBlock {
    AuthRequired,
    EpochMismatch,
    ProtocolMismatch,
    ResyncRequired,
    HistoryRegression,
    BindingMismatch,
    UnverifiedResponse,
    InitialSync,
}

impl TransmissionBlock {
    fn invalidates_response(self) -> bool {
        matches!(
            self,
            Self::AuthRequired
                | Self::ProtocolMismatch
                | Self::EpochMismatch
                | Self::BindingMismatch
                | Self::UnverifiedResponse
                | Self::ResyncRequired
                | Self::HistoryRegression
                | Self::InitialSync
        )
    }
}

#[derive(Default)]
pub(crate) struct TransmissionGate {
    blocks: Vec<(RegistrationBinding, TransmissionBlock)>,
    response_generations: Vec<(RegistrationBinding, u64)>,
    review_generations: Vec<(RegistrationBinding, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitialSyncToken {
    binding: RegistrationBinding,
    generation: u64,
}

#[allow(dead_code)]
impl TransmissionGate {
    pub(crate) fn begin_initial_sync(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Option<InitialSyncToken> {
        self.block(binding, TransmissionBlock::InitialSync);
        let generation = self.response_generation(binding);
        (generation != u64::MAX).then(|| InitialSyncToken {
            binding: binding.clone(),
            generation,
        })
    }

    pub(crate) fn complete_initial_sync(
        &mut self,
        binding: &RegistrationBinding,
        token: &InitialSyncToken,
    ) -> bool {
        if &token.binding != binding
            || token.generation == u64::MAX
            || self.response_generation(binding) != token.generation
        {
            return false;
        }
        self.clear_block(binding, TransmissionBlock::InitialSync)
    }

    pub(crate) fn initial_sync_current(
        &self,
        binding: &RegistrationBinding,
        token: &InitialSyncToken,
    ) -> bool {
        &token.binding == binding
            && token.generation != u64::MAX
            && self.response_generation(binding) == token.generation
            && self.has_block(binding, TransmissionBlock::InitialSync)
            && self.metadata_response_trusted(binding, token.generation)
    }

    pub(crate) fn metadata_response_trusted(
        &self,
        binding: &RegistrationBinding,
        generation: u64,
    ) -> bool {
        generation != u64::MAX
            && self.response_generation(binding) == generation
            && !self.blocks.iter().any(|(stored, reason)| {
                stored == binding
                    && !matches!(
                        reason,
                        TransmissionBlock::InitialSync | TransmissionBlock::ResyncRequired
                    )
            })
    }

    pub(crate) fn block(&mut self, binding: &RegistrationBinding, reason: TransmissionBlock) {
        if reason.invalidates_response() {
            match self
                .response_generations
                .iter_mut()
                .find(|(stored, _)| stored == binding)
            {
                Some((_, generation)) => *generation = generation.saturating_add(1),
                None => self.response_generations.push((binding.clone(), 1)),
            }
            if !matches!(
                reason,
                TransmissionBlock::InitialSync | TransmissionBlock::ResyncRequired
            ) {
                let generation = self.response_generation(binding);
                match self
                    .review_generations
                    .iter_mut()
                    .find(|(stored, _)| stored == binding)
                {
                    Some((_, recorded)) => *recorded = generation,
                    None => self.review_generations.push((binding.clone(), generation)),
                }
            }
        }
        if !self
            .blocks
            .iter()
            .any(|(stored, blocked)| stored == binding && *blocked == reason)
        {
            self.blocks.push((binding.clone(), reason));
        }
    }

    pub(crate) fn clear(
        &mut self,
        binding: &RegistrationBinding,
        reason: TransmissionBlock,
    ) -> bool {
        if reason == TransmissionBlock::InitialSync {
            return false;
        }
        self.clear_block(binding, reason)
    }

    fn clear_block(&mut self, binding: &RegistrationBinding, reason: TransmissionBlock) -> bool {
        if reason.invalidates_response() && self.response_generation(binding) == u64::MAX {
            return false;
        }
        let before = self.blocks.len();
        self.blocks
            .retain(|(stored, blocked)| stored != binding || *blocked != reason);
        self.blocks.len() != before
    }

    pub(crate) fn blocked(&self, binding: &RegistrationBinding) -> bool {
        self.blocks.iter().any(|(stored, _)| stored == binding)
    }

    pub(crate) fn has_block(
        &self,
        binding: &RegistrationBinding,
        reason: TransmissionBlock,
    ) -> bool {
        self.blocks
            .iter()
            .any(|(stored, blocked)| stored == binding && *blocked == reason)
    }

    pub(crate) fn blocked_status(&self, binding: &RegistrationBinding) -> WorkspaceSendStatus {
        if self.has_block(binding, TransmissionBlock::AuthRequired)
            && !self.blocks.iter().any(|(stored, reason)| {
                stored == binding
                    && !matches!(
                        reason,
                        TransmissionBlock::AuthRequired
                            | TransmissionBlock::InitialSync
                            | TransmissionBlock::ResyncRequired
                    )
            })
        {
            return WorkspaceSendStatus::AuthRequired;
        }
        if (self.has_block(binding, TransmissionBlock::InitialSync)
            || self.has_block(binding, TransmissionBlock::ResyncRequired))
            && !self.blocks.iter().any(|(stored, reason)| {
                stored == binding
                    && !matches!(
                        reason,
                        TransmissionBlock::InitialSync | TransmissionBlock::ResyncRequired
                    )
            })
        {
            WorkspaceSendStatus::SyncPending
        } else {
            WorkspaceSendStatus::NeedsReview
        }
    }

    fn stale_status(&self, binding: &RegistrationBinding, generation: u64) -> WorkspaceSendStatus {
        if self.blocked_status(binding) == WorkspaceSendStatus::AuthRequired {
            return WorkspaceSendStatus::AuthRequired;
        }
        let current = self.response_generation(binding);
        let review_since = self
            .review_generations
            .iter()
            .any(|(stored, recorded)| stored == binding && *recorded > generation);
        let review_active = self.blocks.iter().any(|(stored, reason)| {
            stored == binding
                && !matches!(
                    reason,
                    TransmissionBlock::InitialSync | TransmissionBlock::ResyncRequired
                )
        });
        if generation < current && current != u64::MAX && !review_since && !review_active {
            WorkspaceSendStatus::SyncPending
        } else {
            WorkspaceSendStatus::NeedsReview
        }
    }

    pub(crate) fn response_generation(&self, binding: &RegistrationBinding) -> u64 {
        self.response_generations
            .iter()
            .find(|(stored, _)| stored == binding)
            .map_or(0, |(_, generation)| *generation)
    }

    pub(crate) fn response_trusted(&self, binding: &RegistrationBinding, generation: u64) -> bool {
        !self
            .blocks
            .iter()
            .any(|(stored, reason)| stored == binding && reason.invalidates_response())
            && self.response_generation(binding) == generation
    }
}

struct SendPermit {
    state: Arc<Mutex<RuntimeState>>,
    snapshot: SendSnapshot,
    active: bool,
}

impl SendPermit {
    async fn finish(mut self) {
        let _ = self.state.lock().await.gate.finish_send(&self.snapshot);
        self.active = false;
    }
}

impl Drop for SendPermit {
    fn drop(&mut self) {
        if self.active {
            let state = self.state.clone();
            let snapshot = self.snapshot;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = state.lock().await.gate.finish_send(&snapshot);
                });
            }
        }
    }
}

fn outcome(
    binding: &RegistrationBinding,
    status: WorkspaceSendStatus,
    item: Option<&OutboxOperation>,
) -> WorkspaceSendResult {
    WorkspaceSendResult {
        binding: crate::to_contract(binding.clone()),
        status,
        operation_id: item.map(|item| item.operation_id.clone()),
        document_id: item.map(|item| item.document_id.clone()),
        generation: item.map(|item| item.generation.to_string()),
        content_block: None,
    }
}

fn restore_outcome(
    binding: &RegistrationBinding,
    status: WorkspaceSendStatus,
    item: &RestoreOutboxOperation,
) -> WorkspaceSendResult {
    WorkspaceSendResult {
        binding: crate::to_contract(binding.clone()),
        status,
        operation_id: Some(item.operation_id.clone()),
        document_id: Some(item.node_id.to_string()),
        generation: None,
        content_block: None,
    }
}

async fn restore_failure(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: &RestoreOutboxOperation,
    failure: (notryx_client_runtime::Error, bool),
) -> Result<WorkspaceSendResult, String> {
    let (error, restore_submission) = failure;
    let mut state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    if !state
        .transmission_gate
        .response_trusted(binding, response_generation)
    {
        let error_status = send_error_status(error);
        if error_status == WorkspaceSendStatus::NeedsReview {
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
        }
        let stale_status = state
            .transmission_gate
            .stale_status(binding, response_generation);
        let status = if stale_status == WorkspaceSendStatus::NeedsReview
            || error_status == WorkspaceSendStatus::Pending
        {
            stale_status
        } else {
            error_status
        };
        return Ok(restore_outcome(binding, status, item));
    }
    if let Some(reason) = block_reason(error) {
        state.transmission_gate.block(binding, reason);
    }
    let status = if restore_submission && error == notryx_client_runtime::Error::Forbidden {
        WorkspaceSendStatus::PermissionDenied
    } else {
        send_error_status(error)
    };
    Ok(restore_outcome(binding, status, item))
}

fn create_outcome(
    binding: &RegistrationBinding,
    status: WorkspaceSendStatus,
    item: &CreateOutboxOperation,
) -> WorkspaceSendResult {
    WorkspaceSendResult {
        binding: crate::to_contract(binding.clone()),
        status,
        operation_id: Some(item.operation_id.clone()),
        document_id: Some(item.request.node_id.to_string()),
        generation: None,
        content_block: None,
    }
}

fn delete_outcome(
    binding: &RegistrationBinding,
    status: WorkspaceSendStatus,
    item: &DeleteOutboxOperation,
) -> WorkspaceSendResult {
    WorkspaceSendResult {
        binding: crate::to_contract(binding.clone()),
        status,
        operation_id: Some(item.operation_id.clone()),
        document_id: Some(item.node_id.to_string()),
        generation: None,
        content_block: None,
    }
}

fn move_outcome(
    binding: &RegistrationBinding,
    status: WorkspaceSendStatus,
    item: &MoveOutboxOperation,
) -> WorkspaceSendResult {
    WorkspaceSendResult {
        binding: crate::to_contract(binding.clone()),
        status,
        operation_id: Some(item.operation_id.clone()),
        document_id: Some(item.node_id.to_string()),
        generation: None,
        content_block: None,
    }
}

async fn create_failure(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: &CreateOutboxOperation,
    failure: (notryx_client_runtime::Error, bool),
) -> Result<WorkspaceSendResult, String> {
    let (error, create_submission) = failure;
    let mut state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    if !state
        .transmission_gate
        .response_trusted(binding, response_generation)
    {
        let error_status = send_error_status(error);
        if error_status == WorkspaceSendStatus::NeedsReview {
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
        }
        let stale_status = state
            .transmission_gate
            .stale_status(binding, response_generation);
        let status = if stale_status == WorkspaceSendStatus::NeedsReview
            || error_status == WorkspaceSendStatus::Pending
        {
            stale_status
        } else {
            error_status
        };
        return Ok(create_outcome(binding, status, item));
    }
    if let Some(reason) = block_reason(error) {
        state.transmission_gate.block(binding, reason);
    }
    let status = if create_submission && error == notryx_client_runtime::Error::Forbidden {
        WorkspaceSendStatus::PermissionDenied
    } else {
        send_error_status(error)
    };
    Ok(create_outcome(binding, status, item))
}

async fn delete_failure(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: &DeleteOutboxOperation,
    failure: (notryx_client_runtime::Error, bool),
) -> Result<WorkspaceSendResult, String> {
    let (error, delete_submission) = failure;
    let mut state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    if !state
        .transmission_gate
        .response_trusted(binding, response_generation)
    {
        let error_status = send_error_status(error);
        if error_status == WorkspaceSendStatus::NeedsReview {
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
        }
        let stale_status = state
            .transmission_gate
            .stale_status(binding, response_generation);
        let status = if stale_status == WorkspaceSendStatus::NeedsReview
            || error_status == WorkspaceSendStatus::Pending
        {
            stale_status
        } else {
            error_status
        };
        return Ok(delete_outcome(binding, status, item));
    }
    if let Some(reason) = block_reason(error) {
        state.transmission_gate.block(binding, reason);
    }
    let status = if delete_submission && error == notryx_client_runtime::Error::Forbidden {
        WorkspaceSendStatus::PermissionDenied
    } else {
        send_error_status(error)
    };
    Ok(delete_outcome(binding, status, item))
}

async fn move_failure(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: &MoveOutboxOperation,
    failure: (notryx_client_runtime::Error, bool),
) -> Result<WorkspaceSendResult, String> {
    let (error, move_submission) = failure;
    let mut state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    if !state
        .transmission_gate
        .response_trusted(binding, response_generation)
    {
        let error_status = send_error_status(error);
        if error_status == WorkspaceSendStatus::NeedsReview {
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
        }
        let stale_status = state
            .transmission_gate
            .stale_status(binding, response_generation);
        let status = if stale_status == WorkspaceSendStatus::NeedsReview
            || error_status == WorkspaceSendStatus::Pending
        {
            stale_status
        } else {
            error_status
        };
        return Ok(move_outcome(binding, status, item));
    }
    if let Some(reason) = block_reason(error) {
        state.transmission_gate.block(binding, reason);
    }
    let status = if move_submission && error == notryx_client_runtime::Error::Forbidden {
        WorkspaceSendStatus::PermissionDenied
    } else {
        send_error_status(error)
    };
    Ok(move_outcome(binding, status, item))
}

fn ack_error_status(error: notryx_client_runtime::Error) -> WorkspaceSendStatus {
    match error {
        notryx_client_runtime::Error::Unauthorized => WorkspaceSendStatus::AuthRequired,
        notryx_client_runtime::Error::GatewayAuthRequired => {
            WorkspaceSendStatus::GatewayAuthRequired
        }
        notryx_client_runtime::Error::Network | notryx_client_runtime::Error::Unavailable => {
            WorkspaceSendStatus::AckPending
        }
        _ => WorkspaceSendStatus::NeedsReview,
    }
}

fn send_error_status(error: notryx_client_runtime::Error) -> WorkspaceSendStatus {
    match error {
        notryx_client_runtime::Error::Network | notryx_client_runtime::Error::Unavailable => {
            WorkspaceSendStatus::Pending
        }
        other => ack_error_status(other),
    }
}

fn block_reason(error: notryx_client_runtime::Error) -> Option<TransmissionBlock> {
    match error {
        notryx_client_runtime::Error::Unauthorized => Some(TransmissionBlock::AuthRequired),
        notryx_client_runtime::Error::Contract | notryx_client_runtime::Error::TooLarge => {
            Some(TransmissionBlock::ProtocolMismatch)
        }
        notryx_client_runtime::Error::EpochMismatch => Some(TransmissionBlock::EpochMismatch),
        notryx_client_runtime::Error::Binding => Some(TransmissionBlock::BindingMismatch),
        _ => None,
    }
}

async fn request_failure(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: &OutboxOperation,
    error: notryx_client_runtime::Error,
) -> Result<WorkspaceSendResult, String> {
    let mut state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    if !state
        .transmission_gate
        .response_trusted(binding, response_generation)
    {
        let error_status = send_error_status(error);
        if error_status == WorkspaceSendStatus::NeedsReview {
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
        }
        let stale_status = state
            .transmission_gate
            .stale_status(binding, response_generation);
        let status = if stale_status == WorkspaceSendStatus::NeedsReview
            || error_status == WorkspaceSendStatus::Pending
        {
            stale_status
        } else {
            error_status
        };
        return Ok(outcome(binding, status, Some(item)));
    }
    if let Some(reason) = block_reason(error) {
        state.transmission_gate.block(binding, reason);
    }
    Ok(outcome(binding, send_error_status(error), Some(item)))
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn acknowledge_and_record(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    through: u64,
    response_generation: u64,
    app: Option<&tauri::AppHandle>,
    #[cfg(test)] after_ack: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<Option<WorkspaceSendStatus>, String> {
    let mut request = std::pin::pin!(session.acknowledge(through));
    let started = {
        let state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if state.transmission_gate.blocked(binding)
            || !state
                .transmission_gate
                .response_trusted(binding, response_generation)
        {
            return Ok(Some(
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
            ));
        }
        std::future::poll_fn(|context| Poll::Ready(request.as_mut().poll(context))).await
    };
    let result = match started {
        Poll::Ready(result) => result,
        Poll::Pending => request.await,
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_ack {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let mut state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    if !state
        .transmission_gate
        .response_trusted(binding, response_generation)
    {
        if matches!(&result, Err(error) if ack_error_status(*error) == WorkspaceSendStatus::NeedsReview)
        {
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
        }
        let stale_status = state
            .transmission_gate
            .stale_status(binding, response_generation);
        let status = match result {
            Err(error)
                if stale_status != WorkspaceSendStatus::NeedsReview
                    && ack_error_status(error) != WorkspaceSendStatus::AckPending =>
            {
                ack_error_status(error)
            }
            _ => stale_status,
        };
        return Ok(Some(status));
    }
    let ack = match result {
        Ok(ack) => ack,
        Err(error) => {
            if let Some(reason) = block_reason(error) {
                state.transmission_gate.block(binding, reason);
            }
            return Ok(Some(ack_error_status(error)));
        }
    };
    let confirmed_before = state
        .store
        .confirmed_receipt_through(binding)
        .await
        .map_err(|error| error.to_string())?;
    match state.store.confirm_receipt_ack(binding, &ack).await {
        Ok(()) => {
            if ack.retired_through.get() > confirmed_before
                && let Some(app) = app
            {
                let _ = app.emit_to("main", "workspace-metadata-changed", ());
            }
            Ok(None)
        }
        Err(StoreError::Storage) => Ok(Some(WorkspaceSendStatus::AckPending)),
        Err(StoreError::StaleRegistration) => Err("stale_registration".into()),
        Err(_) => Ok(Some(WorkspaceSendStatus::NeedsReview)),
    }
}

fn valid_session(
    state: &RuntimeState,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
) -> Result<(), String> {
    state.gate.validate_send(snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, session))
    {
        return Err("stale_registration".into());
    }
    Ok(())
}

fn mutation(session: &Session, item: &OutboxOperation) -> Result<ContentMutation, String> {
    if item.binding != registration_binding(session.binding()) || item.frozen {
        return Err("stale_registration".into());
    }
    Ok(ContentMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        node_id: item.document_id.parse().map_err(|_| "invalid_document")?,
        expected_revision: ContentRevision::new(
            item.expected_revision.as_deref().ok_or("outbox_blocked")?,
        )
        .map_err(|_| "invalid_revision")?,
        bytes: item.bytes.clone(),
    })
}

fn abandon(session: &Session, item: &OutboxOperation) -> Result<AbandonMutation, String> {
    if item.binding != registration_binding(session.binding()) || item.frozen {
        return Err("stale_registration".into());
    }
    Ok(AbandonMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        node_id: item.document_id.parse().map_err(|_| "invalid_document")?,
        bytes: item.bytes.clone(),
    })
}

async fn apply_receipt(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
    item: &OutboxOperation,
    receipt: &OperationReceipt,
) -> Result<WorkspaceSendStatus, String> {
    let operation_id = item.operation_id.clone();
    let operation_seq = item.operation_seq;
    let workspace_epoch = binding.workspace_epoch.clone();
    let status = match receipt {
        OperationReceipt::Committed {
            content_rev,
            content_hash,
            node_id,
            ..
        } => {
            state
                .store
                .apply_outbox_commit(
                    binding,
                    &OutboxCommit {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                        document_id: node_id.to_string(),
                        content_revision: content_rev.as_str().to_owned(),
                        content_hash: content_hash.clone(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Committed
        }
        OperationReceipt::PreconditionFailed {
            current_content_rev,
            current_content_etag,
            ..
        } => {
            state
                .store
                .apply_outbox_conflict(
                    binding,
                    &OutboxConflict {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                        current_revision: current_content_rev.as_str().to_owned(),
                        current_etag: current_content_etag.clone(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Conflict
        }
        OperationReceipt::NodeDeleted { .. }
        | OperationReceipt::NotFound { .. }
        | OperationReceipt::RecoveryConflict { .. }
        | OperationReceipt::NotApplied { .. } => {
            let kind = match receipt {
                OperationReceipt::NodeDeleted { .. } => OutboxFailureKind::NodeDeleted,
                OperationReceipt::NotFound { .. } => OutboxFailureKind::NotFound,
                OperationReceipt::RecoveryConflict { .. } => OutboxFailureKind::RecoveryConflict,
                _ => OutboxFailureKind::NotApplied,
            };
            state
                .store
                .apply_outbox_failure(
                    binding,
                    &OutboxFailure {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                        kind,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Failed
        }
        OperationReceipt::Abandoned { .. } => {
            state
                .store
                .apply_outbox_abandoned(
                    binding,
                    &OutboxAbandoned {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Abandoned
        }
        OperationReceipt::Pending { .. } => WorkspaceSendStatus::Pending,
        _ => return Err("invalid_receipt".into()),
    };
    Ok(status)
}

#[cfg(test)]
pub async fn send_one(
    state: Arc<Mutex<RuntimeState>>,
    owner: &str,
) -> Result<WorkspaceSendResult, String> {
    send_one_with_app(state, owner, None).await
}

pub(crate) async fn send_one_with_app(
    state: Arc<Mutex<RuntimeState>>,
    owner: &str,
    app: Option<&tauri::AppHandle>,
) -> Result<WorkspaceSendResult, String> {
    let (snapshot, session) = {
        let mut state = state.lock().await;
        let session = state.session.clone().ok_or("not_connected")?;
        let snapshot = state.gate.begin_send(owner).map_err(str::to_owned)?;
        (snapshot, session)
    };
    let permit = SendPermit {
        state: state.clone(),
        snapshot,
        active: true,
    };
    let result = send_inner(
        &state,
        &session,
        &snapshot,
        app,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
    .await;
    permit.finish().await;
    result
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn send_restore(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: RestoreOutboxOperation,
    app: Option<&tauri::AppHandle>,
    #[cfg(test)] after_lookup: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    #[cfg(test)] after_receipt: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<WorkspaceSendResult, String> {
    if item.binding != *binding {
        return Err("stale_registration".into());
    }
    let request = RestoreMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        node_id: item.node_id,
        request: item.request.clone(),
    };
    let mut lookup = std::pin::pin!(session.restore_operation(&request));
    let started = {
        let state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if state.transmission_gate.blocked(binding)
            || !state
                .transmission_gate
                .response_trusted(binding, response_generation)
        {
            return Ok(restore_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        std::future::poll_fn(|context| Poll::Ready(lookup.as_mut().poll(context))).await
    };
    let result = match started {
        Poll::Ready(result) => result,
        Poll::Pending => lookup.await,
    };
    let lookup = match result {
        Ok(lookup) => lookup,
        Err(error) => {
            return restore_failure(
                state,
                session,
                snapshot,
                binding,
                response_generation,
                &item,
                (error, false),
            )
            .await;
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_lookup {
        let _ = ready.send(());
        let _ = resume.await;
    }
    {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            if matches!(lookup, OperationLookup::Retired) {
                state
                    .transmission_gate
                    .block(binding, TransmissionBlock::UnverifiedResponse);
            }
            return Ok(restore_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
    }
    let receipt = match lookup {
        OperationLookup::Receipt(receipt) => *receipt,
        OperationLookup::Retired => {
            let mut state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
            return Ok(restore_outcome(
                binding,
                WorkspaceSendStatus::NeedsReview,
                &item,
            ));
        }
        OperationLookup::Missing => {
            let restoration = match session.prepare_restore(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return restore_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let cancellation = match session.prepare_abandon_restore(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return restore_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let abandoning = item.status == OutboxStatus::Abandoning;
            let mut send = std::pin::pin!(async move {
                if abandoning {
                    cancellation.await
                } else {
                    restoration.await
                }
            });
            let started = {
                let mut state = state.lock().await;
                valid_session(&state, session, snapshot)?;
                if state.transmission_gate.blocked(binding)
                    || !state
                        .transmission_gate
                        .response_trusted(binding, response_generation)
                {
                    return Ok(restore_outcome(
                        binding,
                        state
                            .transmission_gate
                            .stale_status(binding, response_generation),
                        &item,
                    ));
                }
                let expected = PendingOutboxOperation::Restore(item.clone());
                let Some(prepared) = state
                    .store
                    .prepare_pending_outbox_matching(binding, &expected)
                    .await
                    .map_err(|error| error.to_string())?
                else {
                    return Ok(restore_outcome(
                        binding,
                        WorkspaceSendStatus::Pending,
                        &item,
                    ));
                };
                if !matches!(prepared, PendingOutboxOperation::Restore(ref prepared)
                    if prepared.operation_id == item.operation_id
                        && ((!abandoning && prepared.status == OutboxStatus::Sent)
                            || (abandoning && prepared.status == OutboxStatus::Abandoning && prepared.abandon_state == OutboxAbandonState::Sent)))
                {
                    return Err("invalid_outbox_state".into());
                }
                std::future::poll_fn(|context| Poll::Ready(send.as_mut().poll(context))).await
            };
            let result = match started {
                Poll::Ready(result) => result,
                Poll::Pending => send.await,
            };
            match result {
                Ok(receipt) => receipt,
                Err(error) => {
                    return restore_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, !abandoning),
                    )
                    .await;
                }
            }
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_receipt {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let (status, through) = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Ok(restore_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        let status = match &receipt {
            OperationReceipt::Pending { .. } => WorkspaceSendStatus::Pending,
            OperationReceipt::Restored { .. } => WorkspaceSendStatus::Committed,
            OperationReceipt::Abandoned { .. } => WorkspaceSendStatus::Abandoned,
            _ => WorkspaceSendStatus::Failed,
        };
        if status != WorkspaceSendStatus::Pending {
            state
                .store
                .apply_restore_receipt(binding, &receipt)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(app) = app {
                let _ = app.emit_to("main", "workspace-metadata-changed", ());
            }
        }
        let through = state
            .store
            .receipt_ack_through(binding)
            .await
            .map_err(|error| error.to_string())?;
        (status, through)
    };
    if status != WorkspaceSendStatus::Pending
        && let Some(ack_status) = acknowledge_and_record(
            state,
            session,
            snapshot,
            binding,
            through,
            response_generation,
            app,
            #[cfg(test)]
            None,
        )
        .await?
    {
        return Ok(restore_outcome(binding, ack_status, &item));
    }
    Ok(restore_outcome(binding, status, &item))
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn send_create(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: CreateOutboxOperation,
    app: Option<&tauri::AppHandle>,
    #[cfg(test)] after_lookup: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    #[cfg(test)] after_receipt: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<WorkspaceSendResult, String> {
    if item.binding != *binding {
        return Err("stale_registration".into());
    }
    let request = CreateMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        request: item.request.clone(),
    };
    let mut lookup = std::pin::pin!(session.create_operation(&request));
    let started = {
        let state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if state.transmission_gate.blocked(binding)
            || !state
                .transmission_gate
                .response_trusted(binding, response_generation)
        {
            return Ok(create_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        std::future::poll_fn(|context| Poll::Ready(lookup.as_mut().poll(context))).await
    };
    let result = match started {
        Poll::Ready(result) => result,
        Poll::Pending => lookup.await,
    };
    let lookup = match result {
        Ok(lookup) => lookup,
        Err(error) => {
            return create_failure(
                state,
                session,
                snapshot,
                binding,
                response_generation,
                &item,
                (error, false),
            )
            .await;
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_lookup {
        let _ = ready.send(());
        let _ = resume.await;
    }
    {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            if matches!(lookup, OperationLookup::Retired) {
                state
                    .transmission_gate
                    .block(binding, TransmissionBlock::UnverifiedResponse);
            }
            return Ok(create_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
    }
    let receipt = match lookup {
        OperationLookup::Receipt(receipt) => *receipt,
        OperationLookup::Retired => {
            let mut state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
            return Ok(create_outcome(
                binding,
                WorkspaceSendStatus::NeedsReview,
                &item,
            ));
        }
        OperationLookup::Missing => {
            let restoration = match session.prepare_create(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return create_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let cancellation = match session.prepare_abandon_create(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return create_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let abandoning = item.status == OutboxStatus::Abandoning;
            let mut send = std::pin::pin!(async move {
                if abandoning {
                    cancellation.await
                } else {
                    restoration.await
                }
            });
            let started = {
                let mut state = state.lock().await;
                valid_session(&state, session, snapshot)?;
                if state.transmission_gate.blocked(binding)
                    || !state
                        .transmission_gate
                        .response_trusted(binding, response_generation)
                {
                    return Ok(create_outcome(
                        binding,
                        state
                            .transmission_gate
                            .stale_status(binding, response_generation),
                        &item,
                    ));
                }
                let expected = PendingOutboxOperation::Create(item.clone());
                let Some(prepared) = state
                    .store
                    .prepare_pending_outbox_matching(binding, &expected)
                    .await
                    .map_err(|error| error.to_string())?
                else {
                    return Ok(create_outcome(binding, WorkspaceSendStatus::Pending, &item));
                };
                if !matches!(prepared, PendingOutboxOperation::Create(ref prepared)
                    if prepared.operation_id == item.operation_id
                        && ((!abandoning && prepared.status == OutboxStatus::Sent)
                            || (abandoning && prepared.status == OutboxStatus::Abandoning && prepared.abandon_state == OutboxAbandonState::Sent)))
                {
                    return Err("invalid_outbox_state".into());
                }
                std::future::poll_fn(|context| Poll::Ready(send.as_mut().poll(context))).await
            };
            let result = match started {
                Poll::Ready(result) => result,
                Poll::Pending => send.await,
            };
            match result {
                Ok(receipt) => receipt,
                Err(error) => {
                    return create_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, !abandoning),
                    )
                    .await;
                }
            }
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_receipt {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let (status, through) = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Ok(create_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        let status = match &receipt {
            OperationReceipt::Pending { .. } => WorkspaceSendStatus::Pending,
            OperationReceipt::Created { .. } => WorkspaceSendStatus::Committed,
            OperationReceipt::Abandoned { .. } => WorkspaceSendStatus::Abandoned,
            _ => WorkspaceSendStatus::Failed,
        };
        if status != WorkspaceSendStatus::Pending {
            state
                .store
                .apply_create_receipt(binding, &receipt)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(app) = app {
                let _ = app.emit_to("main", "workspace-metadata-changed", ());
            }
        }
        let through = state
            .store
            .receipt_ack_through(binding)
            .await
            .map_err(|error| error.to_string())?;
        (status, through)
    };
    if status != WorkspaceSendStatus::Pending
        && let Some(ack_status) = acknowledge_and_record(
            state,
            session,
            snapshot,
            binding,
            through,
            response_generation,
            app,
            #[cfg(test)]
            None,
        )
        .await?
    {
        return Ok(create_outcome(binding, ack_status, &item));
    }
    Ok(create_outcome(binding, status, &item))
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn send_delete(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: DeleteOutboxOperation,
    app: Option<&tauri::AppHandle>,
    #[cfg(test)] after_lookup: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    #[cfg(test)] after_receipt: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<WorkspaceSendResult, String> {
    if item.binding != *binding {
        return Err("stale_registration".into());
    }
    let request = DeleteMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        node_id: item.node_id,
        expected_node_etag: item.expected_node_etag.clone(),
    };
    let mut lookup = std::pin::pin!(session.delete_operation(&request));
    let started = {
        let state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if state.transmission_gate.blocked(binding)
            || !state
                .transmission_gate
                .response_trusted(binding, response_generation)
        {
            return Ok(delete_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        std::future::poll_fn(|context| Poll::Ready(lookup.as_mut().poll(context))).await
    };
    let result = match started {
        Poll::Ready(result) => result,
        Poll::Pending => lookup.await,
    };
    let lookup = match result {
        Ok(lookup) => lookup,
        Err(error) => {
            return delete_failure(
                state,
                session,
                snapshot,
                binding,
                response_generation,
                &item,
                (error, false),
            )
            .await;
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_lookup {
        let _ = ready.send(());
        let _ = resume.await;
    }
    {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            if matches!(lookup, OperationLookup::Retired) {
                state
                    .transmission_gate
                    .block(binding, TransmissionBlock::UnverifiedResponse);
            }
            return Ok(delete_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
    }
    let receipt = match lookup {
        OperationLookup::Receipt(receipt) => *receipt,
        OperationLookup::Retired => {
            let mut state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
            return Ok(delete_outcome(
                binding,
                WorkspaceSendStatus::NeedsReview,
                &item,
            ));
        }
        OperationLookup::Missing => {
            let restoration = match session.prepare_delete(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return delete_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let cancellation = match session.prepare_abandon_delete(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return delete_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let abandoning = item.status == OutboxStatus::Abandoning;
            let mut send = std::pin::pin!(async move {
                if abandoning {
                    cancellation.await
                } else {
                    restoration.await
                }
            });
            let started = {
                let mut state = state.lock().await;
                valid_session(&state, session, snapshot)?;
                if state.transmission_gate.blocked(binding)
                    || !state
                        .transmission_gate
                        .response_trusted(binding, response_generation)
                {
                    return Ok(delete_outcome(
                        binding,
                        state
                            .transmission_gate
                            .stale_status(binding, response_generation),
                        &item,
                    ));
                }
                let expected = PendingOutboxOperation::Delete(item.clone());
                let Some(prepared) = state
                    .store
                    .prepare_pending_outbox_matching(binding, &expected)
                    .await
                    .map_err(|error| error.to_string())?
                else {
                    return Ok(delete_outcome(binding, WorkspaceSendStatus::Pending, &item));
                };
                if !matches!(prepared, PendingOutboxOperation::Delete(ref prepared)
                    if prepared.operation_id == item.operation_id
                        && ((!abandoning && prepared.status == OutboxStatus::Sent)
                            || (abandoning && prepared.status == OutboxStatus::Abandoning && prepared.abandon_state == OutboxAbandonState::Sent)))
                {
                    return Err("invalid_outbox_state".into());
                }
                std::future::poll_fn(|context| Poll::Ready(send.as_mut().poll(context))).await
            };
            let result = match started {
                Poll::Ready(result) => result,
                Poll::Pending => send.await,
            };
            match result {
                Ok(receipt) => receipt,
                Err(error) => {
                    return delete_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, !abandoning),
                    )
                    .await;
                }
            }
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_receipt {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let (status, through) = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Ok(delete_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        let status = match &receipt {
            OperationReceipt::Pending { .. } => WorkspaceSendStatus::Pending,
            OperationReceipt::Deleted { .. } => WorkspaceSendStatus::Committed,
            OperationReceipt::Abandoned { .. } => WorkspaceSendStatus::Abandoned,
            _ => WorkspaceSendStatus::Failed,
        };
        if status != WorkspaceSendStatus::Pending {
            state
                .store
                .apply_delete_receipt(binding, &receipt)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(app) = app {
                let _ = app.emit_to("main", "workspace-metadata-changed", ());
            }
        }
        let through = state
            .store
            .receipt_ack_through(binding)
            .await
            .map_err(|error| error.to_string())?;
        (status, through)
    };
    if status != WorkspaceSendStatus::Pending
        && let Some(ack_status) = acknowledge_and_record(
            state,
            session,
            snapshot,
            binding,
            through,
            response_generation,
            app,
            #[cfg(test)]
            None,
        )
        .await?
    {
        return Ok(delete_outcome(binding, ack_status, &item));
    }
    Ok(delete_outcome(binding, status, &item))
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn send_move(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    response_generation: u64,
    item: MoveOutboxOperation,
    app: Option<&tauri::AppHandle>,
    #[cfg(test)] after_lookup: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    #[cfg(test)] after_receipt: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<WorkspaceSendResult, String> {
    if item.binding != *binding {
        return Err("stale_registration".into());
    }
    let request = MoveMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        node_id: item.node_id,
        expected_meta_etag: item.expected_meta_etag.clone(),
        request: item.request.clone(),
    };
    let mut lookup = std::pin::pin!(session.move_operation(&request));
    let started = {
        let state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if state.transmission_gate.blocked(binding)
            || !state
                .transmission_gate
                .response_trusted(binding, response_generation)
        {
            return Ok(move_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        std::future::poll_fn(|context| Poll::Ready(lookup.as_mut().poll(context))).await
    };
    let result = match started {
        Poll::Ready(result) => result,
        Poll::Pending => lookup.await,
    };
    let lookup = match result {
        Ok(lookup) => lookup,
        Err(error) => {
            return move_failure(
                state,
                session,
                snapshot,
                binding,
                response_generation,
                &item,
                (error, false),
            )
            .await;
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_lookup {
        let _ = ready.send(());
        let _ = resume.await;
    }
    {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            if matches!(lookup, OperationLookup::Retired) {
                state
                    .transmission_gate
                    .block(binding, TransmissionBlock::UnverifiedResponse);
            }
            return Ok(move_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
    }
    let receipt = match lookup {
        OperationLookup::Receipt(receipt) => *receipt,
        OperationLookup::Retired => {
            let mut state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            state
                .transmission_gate
                .block(binding, TransmissionBlock::UnverifiedResponse);
            return Ok(move_outcome(
                binding,
                WorkspaceSendStatus::NeedsReview,
                &item,
            ));
        }
        OperationLookup::Missing => {
            let restoration = match session.prepare_move(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return move_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let cancellation = match session.prepare_abandon_move(&request) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return move_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, false),
                    )
                    .await;
                }
            };
            let abandoning = item.status == OutboxStatus::Abandoning;
            let mut send = std::pin::pin!(async move {
                if abandoning {
                    cancellation.await
                } else {
                    restoration.await
                }
            });
            let started = {
                let mut state = state.lock().await;
                valid_session(&state, session, snapshot)?;
                if state.transmission_gate.blocked(binding)
                    || !state
                        .transmission_gate
                        .response_trusted(binding, response_generation)
                {
                    return Ok(move_outcome(
                        binding,
                        state
                            .transmission_gate
                            .stale_status(binding, response_generation),
                        &item,
                    ));
                }
                let expected = PendingOutboxOperation::Move(item.clone());
                let Some(prepared) = state
                    .store
                    .prepare_pending_outbox_matching(binding, &expected)
                    .await
                    .map_err(|error| error.to_string())?
                else {
                    return Ok(move_outcome(binding, WorkspaceSendStatus::Pending, &item));
                };
                if !matches!(prepared, PendingOutboxOperation::Move(ref prepared)
                    if prepared.operation_id == item.operation_id
                        && ((!abandoning && prepared.status == OutboxStatus::Sent)
                            || (abandoning && prepared.status == OutboxStatus::Abandoning && prepared.abandon_state == OutboxAbandonState::Sent)))
                {
                    return Err("invalid_outbox_state".into());
                }
                std::future::poll_fn(|context| Poll::Ready(send.as_mut().poll(context))).await
            };
            let result = match started {
                Poll::Ready(result) => result,
                Poll::Pending => send.await,
            };
            match result {
                Ok(receipt) => receipt,
                Err(error) => {
                    return move_failure(
                        state,
                        session,
                        snapshot,
                        binding,
                        response_generation,
                        &item,
                        (error, !abandoning),
                    )
                    .await;
                }
            }
        }
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_receipt {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let (status, through) = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(binding, response_generation)
        {
            return Ok(move_outcome(
                binding,
                state
                    .transmission_gate
                    .stale_status(binding, response_generation),
                &item,
            ));
        }
        let status = match &receipt {
            OperationReceipt::Pending { .. } => WorkspaceSendStatus::Pending,
            OperationReceipt::Moved { .. } => WorkspaceSendStatus::Committed,
            OperationReceipt::Abandoned { .. } => WorkspaceSendStatus::Abandoned,
            _ => WorkspaceSendStatus::Failed,
        };
        if status != WorkspaceSendStatus::Pending {
            state
                .store
                .apply_move_receipt(binding, &receipt)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(app) = app {
                let _ = app.emit_to("main", "workspace-metadata-changed", ());
            }
        }
        let through = state
            .store
            .receipt_ack_through(binding)
            .await
            .map_err(|error| error.to_string())?;
        (status, through)
    };
    if status != WorkspaceSendStatus::Pending
        && let Some(ack_status) = acknowledge_and_record(
            state,
            session,
            snapshot,
            binding,
            through,
            response_generation,
            app,
            #[cfg(test)]
            None,
        )
        .await?
    {
        return Ok(move_outcome(binding, ack_status, &item));
    }
    Ok(move_outcome(binding, status, &item))
}

async fn send_inner(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    app: Option<&tauri::AppHandle>,
    #[cfg(test)] after_lookup: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    #[cfg(test)] after_receipt: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<WorkspaceSendResult, String> {
    let binding = registration_binding(session.binding());
    let (ack_through, ack_already_confirmed, blocked, response_generation) = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        let response_generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Ok(outcome(
                &binding,
                state.transmission_gate.blocked_status(&binding),
                None,
            ));
        }
        let through = state
            .store
            .receipt_ack_through(&binding)
            .await
            .map_err(|error| error.to_string())?;
        let confirmed = state
            .store
            .confirmed_receipt_through(&binding)
            .await
            .map_err(|error| error.to_string())?;
        (
            through,
            confirmed >= through,
            state.transmission_gate.blocked(&binding),
            response_generation,
        )
    };
    if ack_through > 0
        && (!blocked || !ack_already_confirmed)
        && let Some(status) = acknowledge_and_record(
            state,
            session,
            snapshot,
            &binding,
            ack_through,
            response_generation,
            app,
            #[cfg(test)]
            None,
        )
        .await?
    {
        return Ok(outcome(&binding, status, None));
    }
    let first = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Ok(outcome(
                &binding,
                state
                    .transmission_gate
                    .stale_status(&binding, response_generation),
                None,
            ));
        }
        state
            .store
            .next_pending_outbox(&binding)
            .await
            .map_err(|error| error.to_string())?
    };
    match first {
        Some(PendingOutboxOperation::Move(item)) => {
            return send_move(
                state,
                session,
                snapshot,
                &binding,
                response_generation,
                item,
                app,
                #[cfg(test)]
                after_lookup,
                #[cfg(test)]
                after_receipt,
            )
            .await;
        }
        Some(PendingOutboxOperation::Delete(item)) => {
            return send_delete(
                state,
                session,
                snapshot,
                &binding,
                response_generation,
                item,
                app,
                #[cfg(test)]
                after_lookup,
                #[cfg(test)]
                after_receipt,
            )
            .await;
        }
        Some(PendingOutboxOperation::Restore(item)) => {
            return send_restore(
                state,
                session,
                snapshot,
                &binding,
                response_generation,
                item,
                app,
                #[cfg(test)]
                after_lookup,
                #[cfg(test)]
                after_receipt,
            )
            .await;
        }
        Some(PendingOutboxOperation::Create(item)) => {
            return send_create(
                state,
                session,
                snapshot,
                &binding,
                response_generation,
                item,
                app,
                #[cfg(test)]
                after_lookup,
                #[cfg(test)]
                after_receipt,
            )
            .await;
        }
        _ => {}
    }
    let item = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Ok(outcome(
                &binding,
                state
                    .transmission_gate
                    .stale_status(&binding, response_generation),
                None,
            ));
        }
        state
            .store
            .next_outbox(&binding)
            .await
            .map_err(|error| error.to_string())?
    };
    let Some(item) = item else {
        return Ok(outcome(&binding, WorkspaceSendStatus::Idle, None));
    };
    let receipt = if item.status == OutboxStatus::Abandoning {
        let abandon = abandon(session, &item)?;
        let mut request = std::pin::pin!(session.abandon(&abandon));
        let started = {
            let mut state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            if state.transmission_gate.blocked(&binding)
                || !state
                    .transmission_gate
                    .response_trusted(&binding, response_generation)
            {
                return Ok(outcome(
                    &binding,
                    state
                        .transmission_gate
                        .stale_status(&binding, response_generation),
                    Some(&item),
                ));
            }
            let Some(prepared) = state
                .store
                .prepare_outbox_matching(&binding, &item)
                .await
                .map_err(|error| error.to_string())?
            else {
                return Ok(outcome(&binding, WorkspaceSendStatus::Pending, Some(&item)));
            };
            if prepared.operation_id != item.operation_id
                || prepared.status != OutboxStatus::Abandoning
            {
                return Err("invalid_outbox_state".into());
            }
            std::future::poll_fn(|context| Poll::Ready(request.as_mut().poll(context))).await
        };
        let result = match started {
            Poll::Ready(result) => result,
            Poll::Pending => request.await,
        };
        match result {
            Ok(receipt) => receipt,
            Err(error) => {
                return request_failure(
                    state,
                    session,
                    snapshot,
                    &binding,
                    response_generation,
                    &item,
                    error,
                )
                .await;
            }
        }
    } else if matches!(item.status, OutboxStatus::Queued | OutboxStatus::Sent) {
        if item.status == OutboxStatus::Queued && item.expected_revision.is_none() {
            let mut state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            if state.transmission_gate.blocked(&binding)
                || !state
                    .transmission_gate
                    .response_trusted(&binding, response_generation)
            {
                return Ok(outcome(
                    &binding,
                    state
                        .transmission_gate
                        .stale_status(&binding, response_generation),
                    Some(&item),
                ));
            }
            let head = state
                .store
                .next_pending_outbox(&binding)
                .await
                .map_err(|error| error.to_string())?;
            if !matches!(head, Some(PendingOutboxOperation::Content(ref current)) if current == &item)
            {
                return Ok(outcome(&binding, WorkspaceSendStatus::Pending, Some(&item)));
            }
            if let Some(conflict) = state
                .store
                .content_conflict_blocking_fifo(&binding, &item)
                .await
                .map_err(|error| error.to_string())?
            {
                let mut result = outcome(&binding, WorkspaceSendStatus::NeedsReview, Some(&item));
                result.content_block = Some(notryx_protocol::WorkspaceContentConflictBlock {
                    conflict_operation_id: conflict.operation_id,
                    blocked_operation_id: item.operation_id.clone(),
                    blocked_seq: item.operation_seq.to_string(),
                });
                return Ok(result);
            }
        }
        let request = mutation(session, &item)?;
        let mut lookup_request = std::pin::pin!(session.operation(&request));
        let started = {
            let state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            if !state
                .transmission_gate
                .response_trusted(&binding, response_generation)
            {
                return Ok(outcome(
                    &binding,
                    state
                        .transmission_gate
                        .stale_status(&binding, response_generation),
                    Some(&item),
                ));
            }
            std::future::poll_fn(|context| Poll::Ready(lookup_request.as_mut().poll(context))).await
        };
        let lookup_result = match started {
            Poll::Ready(result) => result,
            Poll::Pending => lookup_request.await,
        };
        let lookup = match lookup_result {
            Ok(lookup) => lookup,
            Err(error) => {
                return request_failure(
                    state,
                    session,
                    snapshot,
                    &binding,
                    response_generation,
                    &item,
                    error,
                )
                .await;
            }
        };
        #[cfg(test)]
        if let Some((ready, resume)) = after_lookup {
            let _ = ready.send(());
            let _ = resume.await;
        }
        if matches!(&lookup, OperationLookup::Retired) {
            let mut state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            state
                .transmission_gate
                .block(&binding, TransmissionBlock::UnverifiedResponse);
            return Ok(outcome(
                &binding,
                WorkspaceSendStatus::NeedsReview,
                Some(&item),
            ));
        }
        {
            let state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            if !state
                .transmission_gate
                .response_trusted(&binding, response_generation)
            {
                return Ok(outcome(
                    &binding,
                    state
                        .transmission_gate
                        .stale_status(&binding, response_generation),
                    Some(&item),
                ));
            }
        }
        match lookup {
            OperationLookup::Receipt(receipt) => *receipt,
            OperationLookup::Missing => {
                let prepared = match session.prepare_put_content(&request) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        return request_failure(
                            state,
                            session,
                            snapshot,
                            &binding,
                            response_generation,
                            &item,
                            error,
                        )
                        .await;
                    }
                };
                let mut put = std::pin::pin!(prepared);
                let started = {
                    let mut state = state.lock().await;
                    valid_session(&state, session, snapshot)?;
                    if state.transmission_gate.blocked(&binding)
                        || !state
                            .transmission_gate
                            .response_trusted(&binding, response_generation)
                    {
                        return Ok(outcome(
                            &binding,
                            state
                                .transmission_gate
                                .stale_status(&binding, response_generation),
                            Some(&item),
                        ));
                    }
                    let Some(prepared) = state
                        .store
                        .prepare_outbox_matching(&binding, &item)
                        .await
                        .map_err(|error| error.to_string())?
                    else {
                        return Ok(outcome(&binding, WorkspaceSendStatus::Pending, Some(&item)));
                    };
                    if prepared.operation_id != item.operation_id
                        || prepared.status != OutboxStatus::Sent
                    {
                        return Err("invalid_outbox_state".into());
                    }
                    // Starting the request and setting a block share the same lock.
                    std::future::poll_fn(|context| Poll::Ready(put.as_mut().poll(context))).await
                };
                let result = match started {
                    Poll::Ready(result) => result,
                    Poll::Pending => put.await,
                };
                match result {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        return request_failure(
                            state,
                            session,
                            snapshot,
                            &binding,
                            response_generation,
                            &item,
                            error,
                        )
                        .await;
                    }
                }
            }
            OperationLookup::Retired => {
                let state = state.lock().await;
                valid_session(&state, session, snapshot)?;
                return Ok(outcome(
                    &binding,
                    WorkspaceSendStatus::NeedsReview,
                    Some(&item),
                ));
            }
        }
    } else {
        return Err("invalid_outbox_state".into());
    };
    #[cfg(test)]
    if let Some((ready, resume)) = after_receipt {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let (status, through) = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        if !state
            .transmission_gate
            .response_trusted(&binding, response_generation)
        {
            return Ok(outcome(
                &binding,
                state
                    .transmission_gate
                    .stale_status(&binding, response_generation),
                Some(&item),
            ));
        }
        let status = apply_receipt(&mut state, &binding, &item, &receipt).await?;
        if status != WorkspaceSendStatus::Pending
            && let Some(app) = app
        {
            let _ = app.emit_to("main", "workspace-metadata-changed", ());
        }
        let through = state
            .store
            .receipt_ack_through(&binding)
            .await
            .map_err(|error| error.to_string())?;
        (status, through)
    };
    if status != WorkspaceSendStatus::Pending
        && let Some(ack_status) = acknowledge_and_record(
            state,
            session,
            snapshot,
            &binding,
            through,
            response_generation,
            app,
            #[cfg(test)]
            None,
        )
        .await?
    {
        return Ok(outcome(&binding, ack_status, Some(&item)));
    }
    Ok(outcome(&binding, status, Some(&item)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProductSession, ProductSessionKind, RuntimeState, registration_binding};
    use notryx_client_runtime::Session;
    use notryx_client_store::ClientStore;
    use notryx_protocol::{
        AdminRequest, AdminResponse, AdminResult, WorkspaceQueueRequest, WorkspaceSendStatus,
    };
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    use tokio::sync::Mutex;

    include!("send_move_tests.rs");
    include!("send_move_fifo_tests.rs");
    include!("send_conflict_followup_tests.rs");
    mod content_fifo_block_tests {
        include!("send_content_fifo_block_tests.rs");
    }
    mod content_copy_tests {
        include!("send_content_conflict_copy_tests.rs");
    }
    mod content_copy_content_tests {
        include!("send_content_conflict_copy_content_tests.rs");
    }
    mod content_copy_durability_tests {
        include!("send_content_conflict_copy_durability_tests.rs");
    }
    mod content_copy_status_tests {
        include!("send_content_conflict_copy_status_tests.rs");
    }
    mod content_copy_history_tests {
        include!("send_content_conflict_copy_history_tests.rs");
    }
    mod content_copy_status_corrupt_tests {
        include!("send_content_conflict_copy_status_corrupt_tests.rs");
    }
    mod content_copy_existing_session_tests {
        include!("send_content_conflict_copy_existing_session_tests.rs");
    }
    mod content_copy_retired_retry_tests {
        include!("send_content_conflict_copy_retired_retry_tests.rs");
    }
    mod deleted_draft_copy_sent_fifo_tests {
        include!("deleted_draft_copy_sent_fifo_tests.rs");
    }
    mod deleted_draft_copy_recovery_tests {
        include!("deleted_draft_copy_recovery_tests.rs");
    }
    mod deleted_draft_copy_reconfirm_tests {
        include!("deleted_draft_copy_reconfirm_tests.rs");
    }
    mod deleted_draft_copy_status_tests {
        include!("deleted_draft_copy_status_tests.rs");
    }
    mod deleted_draft_copy_blocked_followup_tests {
        include!("deleted_draft_copy_blocked_followup_tests.rs");
    }
    include!("send_choose_remote_tests.rs");

    struct RestoreSenderRig {
        _directory: tempfile::TempDir,
        notes: std::path::PathBuf,
        server: RunningServer,
        config: Config,
        workspace_id: notryx_domain::WorkspaceId,
        owner_principal_id: String,
        principal_id: String,
        state: Arc<Mutex<RuntimeState>>,
        binding: RegistrationBinding,
    }

    async fn restore_sender_rig(
        role: notryx_protocol::WorkspaceRole,
        content: bool,
    ) -> RestoreSenderRig {
        use notryx_protocol::RestoreNodeRequest;
        use notryx_server::{DeleteNode, NodeSnapshot, Outcome};
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Deleted.md"), b"deleted base\n").unwrap();
        let document = workspace.adopt_root_document("Deleted.md").await.unwrap();
        std::fs::write(notes.join("Other.md"), b"other base\n").unwrap();
        let other = workspace.adopt_root_document("Other.md").await.unwrap();
        let before = NodeSnapshot::from(&document);
        let stream = workspace.register_stream().await.unwrap();
        let receipt = workspace
            .delete_node(DeleteNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: "019a1000-0000-7000-8000-000000000003".parse().unwrap(),
                node_id: before.node_id,
                expected_node_etag: before.etag().unwrap(),
            })
            .await
            .unwrap();
        let Outcome::Deleted(tombstone) = receipt.outcome else {
            panic!("delete")
        };
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let principal_id = if role == notryx_protocol::WorkspaceRole::Reader {
            let AdminResponse::Ok {
                result: AdminResult::Principal { principal_id },
            } = admin::request(
                &config.server.admin_socket,
                &AdminRequest::CreatePrincipal { role },
            )
            .await
            .unwrap()
            else {
                panic!("principal")
            };
            principal_id
        } else {
            owner_principal_id.clone()
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let request = RestoreNodeRequest {
            expected_meta_rev: tombstone.meta_rev,
            parent_id: tombstone.parent_id.unwrap(),
            name: "Restored.md".into(),
        };
        store
            .queue_restore(&binding, tombstone.node_id, &request)
            .await
            .unwrap();
        if content {
            let id = other.node_id.to_string();
            let opened = store
                .open_registered_document(
                    &binding,
                    &id,
                    b"other base\n",
                    other.content_rev.as_str(),
                )
                .await
                .unwrap();
            store
                .save_registered_checkpoint(
                    &binding,
                    &id,
                    &opened.edit_session_id,
                    1,
                    b"other changed\n",
                )
                .await
                .unwrap();
            store
                .close_registered_document(&binding, &id, &opened.edit_session_id)
                .await
                .unwrap();
            assert_eq!(
                store
                    .queue_checkpoint(&binding, &id, 1)
                    .await
                    .unwrap()
                    .operation_seq,
                2
            );
        }
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session);
        RestoreSenderRig {
            _directory: directory,
            notes,
            server,
            config,
            workspace_id,
            owner_principal_id,
            principal_id,
            state: Arc::new(Mutex::new(runtime)),
            binding,
        }
    }

    #[tokio::test]
    async fn restore_and_content_use_one_real_server_fifo_and_ack_stream() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, true).await;
        let first = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(first.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"deleted base\n"
        );
        assert_eq!(
            std::fs::read(rig.notes.join("Other.md")).unwrap(),
            b"other base\n"
        );
        {
            let mut state = rig.state.lock().await;
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                1
            );
        }
        let second = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(second.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            std::fs::read(rig.notes.join("Other.md")).unwrap(),
            b"other changed\n"
        );
        let mut state = rig.state.lock().await;
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            2
        );
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            2
        );
        drop(state);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_uses_one_real_server_fifo_with_restore_and_content() {
        use notryx_client_store::PendingOutboxOperation;
        use notryx_protocol::{CreateNodeRequest, NodeKind};

        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, true).await;
        let session = { rig.state.lock().await.session.clone().unwrap() };
        let runtime = crate::Runtime(rig.state.clone());
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        crate::sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        drop(runtime);
        let root_id = session.server_info().await.unwrap().root_id;
        let node_id = uuid::Uuid::now_v7().to_string().parse().unwrap();
        let request = CreateNodeRequest {
            node_id,
            parent_id: root_id,
            name: "Created Folder".into(),
            kind: NodeKind::Folder,
            upload_id: None,
        };
        let queued = {
            let mut state = rig.state.lock().await;
            state
                .store
                .queue_create(&rig.binding, &request)
                .await
                .unwrap()
        };
        assert_eq!(queued.operation_seq, 3);
        assert!(!rig.notes.join("Created Folder").exists());

        let first = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(first.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"deleted base\n"
        );
        assert_eq!(
            std::fs::read(rig.notes.join("Other.md")).unwrap(),
            b"other base\n"
        );
        assert!(!rig.notes.join("Created Folder").exists());

        let second = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(second.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            std::fs::read(rig.notes.join("Other.md")).unwrap(),
            b"other changed\n"
        );
        assert!(!rig.notes.join("Created Folder").exists());

        // Close the first store and release its exclusive lock before reopening.
        let RuntimeState { store, .. } = Arc::try_unwrap(rig.state)
            .unwrap_or_else(|_| panic!("runtime still has owners"))
            .into_inner();
        store.close().await.unwrap();
        let mut reopened = ClientStore::open(rig._directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        let pending = reopened
            .next_pending_outbox(&rig.binding)
            .await
            .unwrap()
            .unwrap();
        let PendingOutboxOperation::Create(pending) = pending else {
            panic!("create")
        };
        assert_eq!(pending.operation_id, queued.operation_id);
        assert_eq!(pending.operation_seq, 3);
        assert_eq!(pending.status, OutboxStatus::Queued);
        let mut runtime = RuntimeState::new(reopened, "owner".into());
        runtime.session = Some(session.clone());
        let reopened_state = Arc::new(Mutex::new(runtime));

        let third = send_one(reopened_state.clone(), "owner").await.unwrap();
        assert_eq!(third.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            third.operation_id.as_deref(),
            Some(queued.operation_id.as_str())
        );
        assert!(rig.notes.join("Created Folder").is_dir());
        assert_eq!(
            std::fs::read_dir(rig.notes.join("Created Folder"))
                .unwrap()
                .count(),
            0
        );
        let inventory = session.inventory(None).await.unwrap();
        let created = inventory
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .unwrap();
        assert_eq!(created.parent_id, Some(root_id));
        assert_eq!(created.name, "Created Folder");
        assert_eq!(created.kind, NodeKind::Folder);
        assert!(!created.deleted);
        {
            let mut state = reopened_state.lock().await;
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                3
            );
            assert_eq!(
                state
                    .store
                    .confirmed_receipt_through(&rig.binding)
                    .await
                    .unwrap(),
                3
            );
            assert!(
                state
                    .store
                    .next_pending_outbox(&rig.binding)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        rig.server.shutdown().await.unwrap();
    }

    async fn prepare_create_sender(
        rig: &RestoreSenderRig,
    ) -> (Arc<Session>, CreateOutboxOperation) {
        use notryx_protocol::{CreateNodeRequest, NodeKind};
        let session = rig.state.lock().await.session.clone().unwrap();
        let runtime = crate::Runtime(rig.state.clone());
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        crate::sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let parent_id = session.server_info().await.unwrap().root_id;
        let request = CreateNodeRequest {
            node_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            parent_id,
            name: "Boundary Folder".into(),
            kind: NodeKind::Folder,
            upload_id: None,
        };
        let item = rig
            .state
            .lock()
            .await
            .store
            .queue_create(&rig.binding, &request)
            .await
            .unwrap();
        (session, item)
    }

    async fn wait_until_send_gate_idle(state: &Arc<Mutex<RuntimeState>>) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut state = state.lock().await;
                match state.gate.begin_send("owner") {
                    Ok(snapshot) => {
                        state.gate.finish_send(&snapshot).unwrap();
                        break;
                    }
                    Err("transmission_in_progress") => {}
                    Err(error) => panic!("{error}"),
                }
                drop(state);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lost_create_response_recovers_original_receipt_without_new_server_change() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        assert_eq!(
            send_one(rig.state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Committed
        );
        let (session, item) = prepare_create_sender(&rig).await;
        assert_eq!(item.operation_seq, 2);
        let request = CreateMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            request: item.request.clone(),
        };
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let snapshot = rig.state.lock().await.gate.begin_send("owner").unwrap();
        let state = rig.state.clone();
        let sender_session = session.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &state,
                &sender_session,
                &snapshot,
                None,
                None,
                Some((ready, resumed)),
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        assert!(rig.notes.join("Boundary Folder").is_dir());
        let original = match session.create_operation(&request).await.unwrap() {
            OperationLookup::Receipt(receipt) => *receipt,
            other => panic!("expected receipt, got {other:?}"),
        };
        assert!(matches!(&original, OperationReceipt::Created { .. }));
        let head = session.server_info().await.unwrap().head_seq;
        {
            let mut state = rig.state.lock().await;
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                1
            );
            assert!(
                matches!(state.store.next_pending_outbox(&rig.binding).await.unwrap(),
                Some(PendingOutboxOperation::Create(ref pending)) if pending.operation_id == item.operation_id && pending.status == OutboxStatus::Sent)
            );
        }
        sending.abort();
        assert!(sending.await.unwrap_err().is_cancelled());
        drop(resume);
        wait_until_send_gate_idle(&rig.state).await;
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(item.operation_id.as_str())
        );
        assert_eq!(session.server_info().await.unwrap().head_seq, head);
        {
            let mut state = rig.state.lock().await;
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                2
            );
            assert_eq!(
                state
                    .store
                    .confirmed_receipt_through(&rig.binding)
                    .await
                    .unwrap(),
                2
            );
            assert!(
                state
                    .store
                    .next_pending_outbox(&rig.binding)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(rig._directory.path().join("client.sqlite3"))
                .read_only(true),
        )
        .await
        .unwrap();
        let saved: String =
            sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_id=?")
                .bind(&item.operation_id)
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<OperationReceipt>(&saved).unwrap(),
            original
        );
        db.close().await.unwrap();
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delayed_create_receipt_is_not_applied_after_binding_response_is_invalidated() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        assert_eq!(
            send_one(rig.state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Committed
        );
        let (session, item) = prepare_create_sender(&rig).await;
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let snapshot = rig.state.lock().await.gate.begin_send("owner").unwrap();
        let state = rig.state.clone();
        let sender_session = session.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &state,
                &sender_session,
                &snapshot,
                None,
                None,
                Some((ready, resumed)),
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        {
            let mut state = rig.state.lock().await;
            assert!(matches!(
                state.gate.begin_pair("owner"),
                Err("transmission_in_progress")
            ));
            state
                .transmission_gate
                .block(&rig.binding, TransmissionBlock::EpochMismatch);
        }
        resume.send(()).unwrap();
        assert_eq!(
            sending.await.unwrap().unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        {
            let mut state = rig.state.lock().await;
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                1
            );
            assert!(
                matches!(state.store.next_pending_outbox(&rig.binding).await.unwrap(),
                Some(PendingOutboxOperation::Create(ref pending)) if pending.operation_id == item.operation_id && pending.status == OutboxStatus::Sent)
            );
            assert!(
                state
                    .transmission_gate
                    .clear(&rig.binding, TransmissionBlock::EpochMismatch)
            );
        }
        assert!(rig.notes.join("Boundary Folder").is_dir());
        let head = session.server_info().await.unwrap().head_seq;
        assert_eq!(
            send_one(rig.state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Committed
        );
        assert_eq!(session.server_info().await.unwrap().head_seq, head);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reader_create_denial_keeps_sent_item_after_real_inventory_bootstrap() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Reader, false).await;
        let item = {
            let mut state = rig.state.lock().await;
            let PendingOutboxOperation::Restore(item) = state
                .store
                .next_pending_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("restore")
            };
            assert!(
                state
                    .store
                    .request_restore_abandon_matching(&rig.binding, &item)
                    .await
                    .unwrap()
            );
            item
        };
        assert_eq!(item.operation_seq, 1);
        assert_eq!(
            send_one(rig.state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Abandoned
        );
        let (session, create) = prepare_create_sender(&rig).await;
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::PermissionDenied);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(create.operation_id.as_str())
        );
        assert!(!rig.notes.join("Boundary Folder").exists());
        assert!(
            matches!(rig.state.lock().await.store.next_pending_outbox(&rig.binding).await.unwrap(),
            Some(PendingOutboxOperation::Create(ref pending)) if pending.operation_id == create.operation_id && pending.status == OutboxStatus::Sent)
        );
        assert_eq!(
            session
                .server_info()
                .await
                .unwrap()
                .workspace_epoch
                .to_string(),
            rig.binding.workspace_epoch
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_re_registration_freezes_unresolved_sent_operation() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        assert_eq!(
            send_one(rig.state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Committed
        );
        let (session, item) = prepare_create_sender(&rig).await;
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let snapshot = rig.state.lock().await.gate.begin_send("owner").unwrap();
        let state = rig.state.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &state,
                &session,
                &snapshot,
                None,
                None,
                Some((ready, resumed)),
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        assert!(matches!(
            rig.state.lock().await.gate.begin_pair("owner"),
            Err("transmission_in_progress")
        ));
        sending.abort();
        assert!(sending.await.unwrap_err().is_cancelled());
        drop(resume);
        wait_until_send_gate_idle(&rig.state).await;
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &rig.config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: rig.owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let replacement_session = Arc::new(
            Session::pair(
                &format!("http://{}", rig.server.local_addr()),
                rig.workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let replacement = registration_binding(replacement_session.binding());
        {
            let mut state = rig.state.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&replacement)
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(replacement_session);
            assert!(matches!(
                state.store.next_pending_outbox(&rig.binding).await,
                Err(StoreError::StaleRegistration)
            ));
            assert!(
                state
                    .store
                    .next_pending_outbox(&replacement)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                state.store.receipt_ack_through(&replacement).await.unwrap(),
                0
            );
        }
        assert!(rig.notes.join("Boundary Folder").is_dir());
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(rig._directory.path().join("client.sqlite3"))
                .read_only(true),
        )
        .await
        .unwrap();
        let (registration_status, status, receipt): (String, String, Option<String>) = sqlx::query_as(
            "SELECT r.status, o.status, o.receipt_json FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id=?"
        ).bind(&item.operation_id).fetch_one(&mut db).await.unwrap();
        assert_eq!(registration_status, "frozen");
        assert_eq!(status, "sent");
        assert!(receipt.is_none());
        db.close().await.unwrap();
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn nested_folder_save_after_offline_replacement_preserves_client_and_original() {
        use notryx_server::{CreateNode, MoveNode, Outcome};
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Document.md"), b"original document\n").unwrap();
        let document = workspace.adopt_root_document("Document.md").await.unwrap();
        let root = document.parent_id;
        let stream = workspace.register_stream().await.unwrap();
        let folder = "019a1000-0000-7000-8000-000000000006".parse().unwrap();
        assert!(matches!(
            workspace
                .create_node(CreateNode {
                    epoch: workspace.epoch(),
                    stream_id: stream,
                    operation_seq: 1,
                    operation_id: "019a1000-0000-7000-8000-000000000007".parse().unwrap(),
                    node_id: folder,
                    parent_id: root,
                    name: "Folder".into(),
                    kind: notryx_protocol::NodeKind::Folder,
                    upload_id: None,
                })
                .await
                .unwrap()
                .outcome,
            Outcome::Created(_)
        ));
        assert!(matches!(
            workspace
                .move_node(MoveNode {
                    epoch: workspace.epoch(),
                    stream_id: stream,
                    operation_seq: 2,
                    operation_id: "019a1000-0000-7000-8000-000000000008".parse().unwrap(),
                    node_id: document.node_id,
                    expected_meta_rev: document.meta_rev.clone(),
                    expected_parent_id: root,
                    parent_id: folder,
                    name: "Document.md".into(),
                })
                .await
                .unwrap()
                .outcome,
            Outcome::Moved(_)
        ));
        workspace.close().await.unwrap();
        let original_folder = directory.path().join("original-folder");
        std::fs::rename(notes.join("Folder"), &original_folder).unwrap();
        std::fs::create_dir(notes.join("Folder")).unwrap();
        std::fs::write(notes.join("Folder/Document.md"), b"original document\n").unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state.clone(),
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let before = session.server_info().await.unwrap();
        let client_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&client_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        let id = document.node_id.to_string();
        let opened = store
            .open_registered_document(
                &binding,
                &id,
                b"original document\n",
                document.content_rev.as_str(),
            )
            .await
            .unwrap();
        let edit = b"local draft to preserve\n";
        store
            .save_registered_checkpoint(&binding, &id, &opened.edit_session_id, 1, edit)
            .await
            .unwrap();
        let queued = store.queue_checkpoint(&binding, &id, 1).await.unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let response = send_one(state.clone(), "owner").await.unwrap();
        // The current path is authoritative after an offline folder replacement.
        assert_eq!(response.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            response.operation_id.as_deref(),
            Some(queued.operation_id.as_str())
        );
        {
            let mut runtime = state.lock().await;
            let draft = runtime
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.bytes, edit);
            assert_eq!(draft.base_bytes, edit);
            let item = runtime
                .store
                .read_outbox_operation(&binding, &queued.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(item.bytes, edit);
            assert_eq!(item.operation_seq, queued.operation_seq);
            assert_eq!(item.expected_revision, queued.expected_revision);
            assert_eq!(item.payload_hash, queued.payload_hash);
            assert_eq!(item.status, OutboxStatus::Committed);
            assert_eq!(
                runtime.store.receipt_ack_through(&binding).await.unwrap(),
                1
            );
        }
        assert_eq!(
            session.server_info().await.unwrap().head_seq.get(),
            before.head_seq.get() + 1
        );
        assert_eq!(
            std::fs::read(original_folder.join("Document.md")).unwrap(),
            b"original document\n"
        );
        assert_eq!(
            std::fs::read(notes.join("Folder/Document.md")).unwrap(),
            edit
        );
        assert_eq!(
            std::fs::read(
                server_state
                    .join("objects/sha256")
                    .join(&document.content_hash)
            )
            .unwrap(),
            b"original document\n"
        );
        drop(state);
        let mut reopened = ClientStore::open(client_path).await.unwrap();
        let draft = reopened
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, edit);
        assert_eq!(draft.base_bytes, edit);
        let pending = reopened
            .read_outbox_operation(&binding, &queued.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.bytes, edit);
        assert_eq!(pending.payload_hash, queued.payload_hash);
        assert_eq!(pending.status, OutboxStatus::Committed);
        drop(reopened);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reader_restore_sender_reports_denial_and_preserves_pending_request() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Reader, false).await;
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(
            serde_json::to_value(result.status).unwrap(),
            "permission_denied"
        );
        assert_eq!(result.binding, crate::to_contract(rig.binding.clone()));
        assert!(result.operation_id.is_some());
        assert!(result.document_id.is_some());
        assert_eq!(result.generation, None);
        assert!(!rig.notes.join("Restored.md").exists());
        let mut state = rig.state.lock().await;
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            0
        );
        let pending = state
            .store
            .next_pending_outbox(&rig.binding)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(pending, PendingOutboxOperation::Restore(item) if item.status == OutboxStatus::Sent)
        );
        drop(state);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restore_permission_change_reuses_original_operation_and_acknowledges_receipt() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Reader, false).await;
        let denied = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(denied.status, WorkspaceSendStatus::PermissionDenied);
        let original = {
            let mut state = rig.state.lock().await;
            let Some(PendingOutboxOperation::Restore(item)) =
                state.store.next_pending_outbox(&rig.binding).await.unwrap()
            else {
                panic!("restore")
            };
            assert_eq!(
                Some(item.operation_id.as_str()),
                denied.operation_id.as_deref()
            );
            assert_eq!(item.status, OutboxStatus::Sent);
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                0
            );
            item
        };
        assert!(matches!(
            admin::request(
                &rig.config.server.admin_socket,
                &AdminRequest::SetRole {
                    principal_id: rig.principal_id.clone(),
                    role: notryx_protocol::WorkspaceRole::Editor,
                }
            )
            .await
            .unwrap(),
            AdminResponse::Ok { .. }
        ));
        let committed = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(committed.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            committed.operation_id.as_deref(),
            Some(original.operation_id.as_str())
        );
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"deleted base\n"
        );
        let mut state = rig.state.lock().await;
        let page = state
            .store
            .list_restore_operations(&rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(
            page.entries[0].operation.operation_id,
            original.operation_id
        );
        assert_eq!(
            page.entries[0].operation.operation_seq,
            original.operation_seq
        );
        assert_eq!(page.entries[0].operation.request, original.request);
        assert!(matches!(
            page.entries[0].receipt.as_ref(),
            Some(OperationReceipt::Restored { .. })
        ));
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            original.operation_seq
        );
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            original.operation_seq
        );
        drop(state);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restore_permission_denial_is_limited_to_the_submission_response() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        let (session, snapshot, generation, item) = {
            let mut state = rig.state.lock().await;
            let session = state.session.clone().unwrap();
            let snapshot = state.gate.begin_send("owner").unwrap();
            let generation = state.transmission_gate.response_generation(&rig.binding);
            let Some(PendingOutboxOperation::Restore(item)) =
                state.store.next_pending_outbox(&rig.binding).await.unwrap()
            else {
                panic!("restore")
            };
            (session, snapshot, generation, item)
        };
        let lookup = restore_failure(
            &rig.state,
            &session,
            &snapshot,
            &rig.binding,
            generation,
            &item,
            (notryx_client_runtime::Error::Forbidden, false),
        )
        .await
        .unwrap();
        assert_eq!(lookup.status, WorkspaceSendStatus::NeedsReview);
        let submission = restore_failure(
            &rig.state,
            &session,
            &snapshot,
            &rig.binding,
            generation,
            &item,
            (notryx_client_runtime::Error::Forbidden, true),
        )
        .await
        .unwrap();
        assert_eq!(submission.status, WorkspaceSendStatus::PermissionDenied);
        assert_eq!(
            submission.operation_id.as_deref(),
            Some(item.operation_id.as_str())
        );
        rig.state.lock().await.gate.finish_send(&snapshot).unwrap();
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reader_can_abandon_confirmed_missing_restore_without_losing_following_draft() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Reader, true).await;
        let denied = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(
            serde_json::to_value(denied.status).unwrap(),
            "permission_denied"
        );
        let (session, item) = {
            let mut state = rig.state.lock().await;
            let session = state.session.clone().unwrap();
            let PendingOutboxOperation::Restore(item) = state
                .store
                .next_pending_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("restore")
            };
            (session, item)
        };
        let request = RestoreMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            request: item.request.clone(),
        };
        assert!(matches!(
            session.restore_operation(&request).await.unwrap(),
            OperationLookup::Missing
        ));
        {
            let mut state = rig.state.lock().await;
            assert!(
                state
                    .store
                    .request_restore_abandon_matching(&rig.binding, &item)
                    .await
                    .unwrap()
            );
        }
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Abandoned);
        assert!(!rig.notes.join("Restored.md").exists());
        let mut state = rig.state.lock().await;
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            1
        );
        assert_eq!(
            state
                .store
                .confirmed_receipt_through(&rig.binding)
                .await
                .unwrap(),
            1
        );
        let Some(PendingOutboxOperation::Content(content)) =
            state.store.next_pending_outbox(&rig.binding).await.unwrap()
        else {
            panic!("content must follow")
        };
        assert_eq!(content.bytes, b"other changed\n");
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &content.document_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"other changed\n");
        assert_eq!(draft.base_bytes, b"other base\n");
        drop(state);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn admitted_restore_wins_over_later_abandon_intent() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        let (session, item) = {
            let mut state = rig.state.lock().await;
            let session = state.session.clone().unwrap();
            let PendingOutboxOperation::Restore(item) = state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("restore")
            };
            (session, item)
        };
        let request = RestoreMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            request: item.request.clone(),
        };
        assert!(matches!(
            session.restore_operation(&request).await.unwrap(),
            OperationLookup::Missing
        ));
        {
            let mut state = rig.state.lock().await;
            assert!(
                state
                    .store
                    .request_restore_abandon_matching(&rig.binding, &item)
                    .await
                    .unwrap()
            );
        }
        let original = session.restore(&request).await.unwrap();
        assert!(matches!(original, OperationReceipt::Restored { .. }));
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"deleted base\n"
        );
        let mut state = rig.state.lock().await;
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            1
        );
        let page = state
            .store
            .list_restore_operations(&rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].receipt.as_ref(), Some(&original));
        drop(state);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retired_restore_receipt_requires_review_without_resubmitting() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        let (session, item) = {
            let mut state = rig.state.lock().await;
            let session = state.session.clone().unwrap();
            let PendingOutboxOperation::Restore(item) = state
                .store
                .next_pending_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("restore")
            };
            (session, item)
        };
        let request = RestoreMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            request: item.request.clone(),
        };
        session.restore(&request).await.unwrap();
        session.acknowledge(item.operation_seq).await.unwrap();
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::NeedsReview);
        let mut state = rig.state.lock().await;
        assert_eq!(
            state.store.receipt_ack_through(&rig.binding).await.unwrap(),
            0
        );
        assert!(
            matches!(state.store.next_pending_outbox(&rig.binding).await.unwrap(), Some(PendingOutboxOperation::Restore(ref pending)) if pending.status == OutboxStatus::Queued)
        );
        drop(state);
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"deleted base\n"
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retired_restore_receipt_blocks_current_registration_and_reports_review() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, true).await;
        let (session, item) = {
            let mut state = rig.state.lock().await;
            state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
                binding: crate::to_contract(rig.binding.clone()),
                status: notryx_protocol::WorkspaceSyncStatus::Ready,
            });
            let session = state.session.clone().unwrap();
            let Some(PendingOutboxOperation::Restore(item)) =
                state.store.next_pending_outbox(&rig.binding).await.unwrap()
            else {
                panic!("restore")
            };
            (session, item)
        };
        let request = RestoreMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            request: item.request.clone(),
        };
        session.restore(&request).await.unwrap();
        session.acknowledge(item.operation_seq).await.unwrap();
        assert!(matches!(
            session.restore_operation(&request).await.unwrap(),
            OperationLookup::Retired
        ));
        let before = session.server_info().await.unwrap();
        let result = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::NeedsReview);
        let runtime = crate::Runtime(rig.state.clone());
        let reported = crate::sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
            .await
            .unwrap();
        assert_eq!(
            reported.status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        let repeated = send_one(rig.state.clone(), "owner").await.unwrap();
        assert_eq!(repeated.status, WorkspaceSendStatus::NeedsReview);
        {
            let mut state = rig.state.lock().await;
            assert!(
                state
                    .transmission_gate
                    .has_block(&rig.binding, TransmissionBlock::UnverifiedResponse)
            );
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                0
            );
            assert_eq!(
                state.store.next_pending_outbox(&rig.binding).await.unwrap(),
                Some(PendingOutboxOperation::Restore(item.clone()))
            );
            let drafts = state.store.list_registered_drafts().await.unwrap();
            let other = drafts
                .iter()
                .find(|draft| draft.document_id != item.node_id.to_string())
                .unwrap();
            let draft = state
                .store
                .read_registered_checkpoint(&rig.binding, &other.document_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.bytes, b"other changed\n");
            assert_eq!(draft.base_bytes, b"other base\n");
            let page = state.store.list_outbox(&rig.binding, 0, 100).await.unwrap();
            assert_eq!(page.entries.len(), 1);
            assert_eq!(page.entries[0].operation_seq, 2);
            assert_eq!(page.entries[0].status, OutboxStatus::Queued);
        }
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(rig._directory.path().join("client.sqlite3"))
                .read_only(true),
        )
        .await
        .unwrap();
        let (bytes,): (Vec<u8>,) = sqlx::query_as(
            "SELECT bytes FROM client_outbox WHERE operation_kind='content' AND operation_seq=2",
        )
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(bytes, b"other changed\n");
        db.close().await.unwrap();
        assert_eq!(
            session.server_info().await.unwrap().head_seq,
            before.head_seq
        );
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"deleted base\n"
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restore_re_registration_waits_for_send_and_freezes_uncertain_receipt() {
        let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let (snapshot, session) = {
            let mut state = rig.state.lock().await;
            (
                state.gate.begin_send("owner").unwrap(),
                state.session.clone().unwrap(),
            )
        };
        let state = rig.state.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &state,
                &session,
                &snapshot,
                None,
                None,
                Some((ready, resumed)),
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        {
            let mut state = rig.state.lock().await;
            assert!(matches!(
                state.gate.begin_pair("owner"),
                Err("transmission_in_progress")
            ));
            assert_eq!(
                state.store.receipt_ack_through(&rig.binding).await.unwrap(),
                0
            );
        }
        sending.abort();
        assert!(sending.await.unwrap_err().is_cancelled());
        drop(resume);
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &rig.config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: rig.owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let new_session = Arc::new(
            Session::pair(
                &format!("http://{}", rig.server.local_addr()),
                rig.workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let replacement = registration_binding(new_session.binding());
        let pair = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut state = rig.state.lock().await;
                match state.gate.begin_pair("owner") {
                    Ok(pair) => break pair,
                    Err("transmission_in_progress") => {}
                    Err(error) => panic!("{error}"),
                }
                drop(state);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        {
            let mut state = rig.state.lock().await;
            state
                .store
                .activate_registration(&replacement)
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(new_session);
            assert!(matches!(
                state.store.next_pending_outbox(&rig.binding).await,
                Err(StoreError::StaleRegistration)
            ));
            assert!(
                state
                    .store
                    .next_pending_outbox(&replacement)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                state.store.receipt_ack_through(&replacement).await.unwrap(),
                0
            );
        }
        assert_eq!(
            std::fs::read(rig.notes.join("Restored.md")).unwrap(),
            b"deleted base\n"
        );
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(rig._directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        let (registration_status, status, receipt): (String, String, Option<String>) = sqlx::query_as("SELECT r.status, o.status, o.receipt_json FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_kind='restore'").fetch_one(&mut db).await.unwrap();
        assert_eq!(registration_status, "frozen");
        assert_eq!(status, "sent");
        assert!(receipt.is_none());
        db.close().await.unwrap();
        rig.server.shutdown().await.unwrap();
    }

    #[test]
    fn oversized_send_response_requires_protocol_revalidation() {
        assert_eq!(
            block_reason(notryx_client_runtime::Error::TooLarge),
            Some(TransmissionBlock::ProtocolMismatch)
        );
    }

    #[test]
    fn transmission_blocks_are_isolated_by_binding_and_reason() {
        let binding = RegistrationBinding {
            server_origin: "http://127.0.0.1:1234".into(),
            workspace_id: "workspace".into(),
            workspace_epoch: "epoch".into(),
            device_id: "device".into(),
            device_stream_id: "stream".into(),
        };
        let mut replacement = binding.clone();
        replacement.device_stream_id = "replacement".into();
        let mut gate = TransmissionGate::default();
        let original_generation = gate.response_generation(&binding);
        gate.block(&binding, TransmissionBlock::EpochMismatch);
        assert!(!gate.response_trusted(&binding, original_generation));
        gate.block(&binding, TransmissionBlock::ProtocolMismatch);
        assert!(!gate.response_trusted(&binding, original_generation));
        assert!(gate.response_trusted(&replacement, gate.response_generation(&replacement)));
        assert!(gate.blocked(&binding));
        let mut renderer = crate::gate::RequestGate::new("owner".into());
        renderer.replace_owner("reloaded-owner".into());
        assert!(gate.blocked(&binding));
        assert!(!gate.blocked(&replacement));
        assert!(!gate.clear(&replacement, TransmissionBlock::EpochMismatch));
        assert!(gate.clear(&binding, TransmissionBlock::EpochMismatch));
        assert!(gate.blocked(&binding));
        assert!(gate.clear(&binding, TransmissionBlock::ProtocolMismatch));
        assert!(!gate.blocked(&binding));
        assert!(!gate.response_trusted(&binding, original_generation));
        assert!(gate.response_trusted(&binding, gate.response_generation(&binding)));
    }

    fn history_block_invalidates_inflight_response(reason: TransmissionBlock) {
        let binding = RegistrationBinding {
            server_origin: "http://127.0.0.1:1234".into(),
            workspace_id: "workspace".into(),
            workspace_epoch: "epoch".into(),
            device_id: "device".into(),
            device_stream_id: "stream".into(),
        };
        let mut gate = TransmissionGate::default();
        let generation = gate.response_generation(&binding);
        gate.block(&binding, reason);
        assert!(!gate.response_trusted(&binding, generation));
        assert!(gate.clear(&binding, reason));
        assert!(!gate.response_trusted(&binding, generation));
        assert!(gate.response_trusted(&binding, gate.response_generation(&binding)));
    }

    #[test]
    fn resync_required_invalidates_inflight_response() {
        history_block_invalidates_inflight_response(TransmissionBlock::ResyncRequired);
    }

    #[test]
    fn history_regression_invalidates_inflight_response() {
        history_block_invalidates_inflight_response(TransmissionBlock::HistoryRegression);
    }

    #[test]
    fn resync_wait_does_not_hide_a_separate_review_block() {
        let binding = RegistrationBinding {
            server_origin: "http://127.0.0.1:1234".into(),
            workspace_id: "workspace".into(),
            workspace_epoch: "epoch".into(),
            device_id: "device".into(),
            device_stream_id: "stream".into(),
        };
        let mut gate = TransmissionGate::default();
        gate.block(&binding, TransmissionBlock::ResyncRequired);
        assert_eq!(
            gate.blocked_status(&binding),
            WorkspaceSendStatus::SyncPending
        );
        let _initial = gate.begin_initial_sync(&binding).unwrap();
        assert_eq!(
            gate.blocked_status(&binding),
            WorkspaceSendStatus::SyncPending
        );
        for reason in [
            TransmissionBlock::EpochMismatch,
            TransmissionBlock::ProtocolMismatch,
            TransmissionBlock::HistoryRegression,
            TransmissionBlock::BindingMismatch,
        ] {
            gate.block(&binding, reason);
            assert_eq!(
                gate.blocked_status(&binding),
                WorkspaceSendStatus::NeedsReview
            );
            assert!(gate.clear(&binding, reason));
            assert_eq!(
                gate.blocked_status(&binding),
                WorkspaceSendStatus::SyncPending
            );
        }
    }

    #[test]
    fn stale_resync_response_keeps_the_cause_even_after_the_block_clears() {
        let binding = RegistrationBinding {
            server_origin: "http://127.0.0.1:1234".into(),
            workspace_id: "workspace".into(),
            workspace_epoch: "epoch".into(),
            device_id: "device".into(),
            device_stream_id: "stream".into(),
        };
        for reason in [
            TransmissionBlock::ProtocolMismatch,
            TransmissionBlock::EpochMismatch,
            TransmissionBlock::HistoryRegression,
            TransmissionBlock::BindingMismatch,
        ] {
            let mut gate = TransmissionGate::default();
            let start = gate.response_generation(&binding);
            gate.block(&binding, TransmissionBlock::ResyncRequired);
            assert_eq!(
                gate.stale_status(&binding, start),
                WorkspaceSendStatus::SyncPending
            );
            assert!(gate.clear(&binding, TransmissionBlock::ResyncRequired));
            assert_eq!(
                gate.stale_status(&binding, start),
                WorkspaceSendStatus::SyncPending
            );
            gate.block(&binding, reason);
            assert!(gate.clear(&binding, reason));
            assert_eq!(
                gate.stale_status(&binding, start),
                WorkspaceSendStatus::NeedsReview
            );
        }
    }

    #[test]
    fn stale_initial_sync_success_cannot_clear_current_or_other_blocks() {
        let binding = RegistrationBinding {
            server_origin: "http://127.0.0.1:1234".into(),
            workspace_id: "workspace".into(),
            workspace_epoch: "epoch".into(),
            device_id: "device".into(),
            device_stream_id: "stream".into(),
        };
        let mut replacement = binding.clone();
        replacement.device_stream_id = "replacement".into();
        let mut gate = TransmissionGate::default();
        let first = gate.begin_initial_sync(&binding).unwrap();
        assert_eq!(
            gate.blocked_status(&binding),
            WorkspaceSendStatus::SyncPending
        );
        for reason in [
            TransmissionBlock::EpochMismatch,
            TransmissionBlock::ProtocolMismatch,
            TransmissionBlock::ResyncRequired,
            TransmissionBlock::HistoryRegression,
            TransmissionBlock::BindingMismatch,
        ] {
            gate.block(&binding, reason);
            assert_eq!(
                gate.blocked_status(&binding),
                if reason == TransmissionBlock::ResyncRequired {
                    WorkspaceSendStatus::SyncPending
                } else {
                    WorkspaceSendStatus::NeedsReview
                }
            );
            assert!(gate.clear(&binding, reason));
            assert_eq!(
                gate.blocked_status(&binding),
                WorkspaceSendStatus::SyncPending
            );
        }
        assert!(!gate.clear(&binding, TransmissionBlock::InitialSync));
        assert!(gate.blocked(&binding));
        let second = gate.begin_initial_sync(&binding).unwrap();
        assert_ne!(first, second);
        assert!(!gate.complete_initial_sync(&binding, &first));
        let mut replacement_token = gate.begin_initial_sync(&replacement).unwrap();
        while replacement_token.generation < second.generation {
            replacement_token = gate.begin_initial_sync(&replacement).unwrap();
        }
        assert_eq!(replacement_token.generation, second.generation);
        assert!(!gate.complete_initial_sync(&replacement, &second));
        assert!(gate.complete_initial_sync(&replacement, &replacement_token));
        gate.block(&binding, TransmissionBlock::ProtocolMismatch);
        assert!(!gate.complete_initial_sync(&binding, &second));
        let third = gate.begin_initial_sync(&binding).unwrap();
        gate.block(&binding, TransmissionBlock::ProtocolMismatch);
        assert!(!gate.complete_initial_sync(&binding, &third));
        let current = gate.begin_initial_sync(&binding).unwrap();
        assert!(gate.complete_initial_sync(&binding, &current));
        assert!(gate.blocked(&binding));
        assert!(gate.clear(&binding, TransmissionBlock::ProtocolMismatch));
        assert!(!gate.blocked(&binding));
    }

    #[test]
    fn ack_failures_report_authentication_and_protocol_errors_distinctly() {
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::Unauthorized),
            WorkspaceSendStatus::AuthRequired
        );
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::GatewayAuthRequired),
            WorkspaceSendStatus::GatewayAuthRequired
        );
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::Network),
            WorkspaceSendStatus::AckPending
        );
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::Binding),
            WorkspaceSendStatus::NeedsReview
        );
    }

    #[test]
    fn send_failures_report_authentication_protocol_and_network_separately() {
        use notryx_client_runtime::Error;
        assert_eq!(
            send_error_status(Error::Unauthorized),
            WorkspaceSendStatus::AuthRequired
        );
        assert_eq!(
            send_error_status(Error::GatewayAuthRequired),
            WorkspaceSendStatus::GatewayAuthRequired
        );
        assert_eq!(
            send_error_status(Error::Network),
            WorkspaceSendStatus::Pending
        );
        assert_eq!(
            send_error_status(Error::Unavailable),
            WorkspaceSendStatus::Pending
        );
        assert_eq!(
            send_error_status(Error::EpochMismatch),
            WorkspaceSendStatus::NeedsReview
        );
        assert_eq!(
            send_error_status(Error::Contract),
            WorkspaceSendStatus::NeedsReview
        );
        assert_eq!(
            send_error_status(Error::IdempotencyMismatch),
            WorkspaceSendStatus::NeedsReview
        );
        assert_eq!(
            send_error_status(Error::OperationGap),
            WorkspaceSendStatus::NeedsReview
        );
    }

    #[tokio::test]
    async fn revoked_device_before_first_send_reports_auth_and_preserves_the_outbox() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let id = node.node_id.to_string();
        let edit = store
            .open_registered_document(&binding, &id, b"base\n", node.content_rev.unwrap().as_str())
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"unsent\n")
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        state
            .gate
            .open_session(edit.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            edit.edit_session_id.clone(),
            ProductSession {
                document_id: id.clone(),
                binding: binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let request = WorkspaceQueueRequest {
            renderer_session_id: "owner".into(),
            document_id: id.clone(),
            edit_session_id: edit.edit_session_id.clone(),
            expected_generation: "1".into(),
        };
        let mut injection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_queue BEFORE INSERT ON client_outbox BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&mut injection).await.unwrap();
        assert_eq!(state.queue_document(&request).await.unwrap_err(), "Storage");
        let stored = state
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.bytes, b"unsent\n");
        assert_eq!(stored.base_bytes, b"base\n");
        assert_eq!(state.store.receipt_ack_through(&binding).await.unwrap(), 0);
        let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_outbox")
            .fetch_one(&mut injection)
            .await
            .unwrap();
        assert_eq!(outbox_count, 0);
        sqlx::query("DROP TRIGGER fail_queue")
            .execute(&mut injection)
            .await
            .unwrap();
        injection.close().await.unwrap();
        let queued = state.queue_document(&request).await.unwrap();
        assert_eq!(queued.generation, "1");
        assert!(matches!(
            admin::request(
                &config.server.admin_socket,
                &AdminRequest::RevokeDevice {
                    device_id: binding.device_id.clone()
                }
            )
            .await
            .unwrap(),
            AdminResponse::Ok { .. }
        ));
        let state = Arc::new(Mutex::new(state));
        let (snapshot, session, item, generation) = {
            let mut locked = state.lock().await;
            let snapshot = locked.gate.begin_send("owner").unwrap();
            let session = locked.session.clone().unwrap();
            let item = locked.store.next_outbox(&binding).await.unwrap().unwrap();
            let generation = locked.transmission_gate.response_generation(&binding);
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::ProtocolMismatch);
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
            (snapshot, session, item, generation)
        };
        for error in [
            notryx_client_runtime::Error::Unauthorized,
            notryx_client_runtime::Error::GatewayAuthRequired,
        ] {
            assert_eq!(
                request_failure(
                    &state, &session, &snapshot, &binding, generation, &item, error
                )
                .await
                .unwrap()
                .status,
                WorkspaceSendStatus::NeedsReview
            );
        }
        let generation = {
            let mut locked = state.lock().await;
            let generation = locked.transmission_gate.response_generation(&binding);
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::ResyncRequired);
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ResyncRequired)
            );
            generation
        };
        assert_eq!(
            request_failure(
                &state,
                &session,
                &snapshot,
                &binding,
                generation,
                &item,
                notryx_client_runtime::Error::Unauthorized
            )
            .await
            .unwrap()
            .status,
            WorkspaceSendStatus::AuthRequired
        );
        assert_eq!(
            request_failure(
                &state,
                &session,
                &snapshot,
                &binding,
                generation,
                &item,
                notryx_client_runtime::Error::Contract
            )
            .await
            .unwrap()
            .status,
            WorkspaceSendStatus::NeedsReview
        );
        assert!(
            !state
                .lock()
                .await
                .transmission_gate
                .has_block(&binding, TransmissionBlock::ProtocolMismatch)
        );
        {
            let mut locked = state.lock().await;
            assert!(
                locked
                    .transmission_gate
                    .has_block(&binding, TransmissionBlock::UnverifiedResponse)
            );
            let draft = locked
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.bytes, b"unsent\n");
            assert_eq!(draft.base_bytes, b"base\n");
            assert_eq!(
                locked
                    .store
                    .read_outbox_operation(&binding, &queued.operation_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .bytes,
                b"unsent\n"
            );
            let mut other = binding.clone();
            other.workspace_id = uuid::Uuid::new_v4().to_string();
            assert!(
                !locked
                    .transmission_gate
                    .has_block(&other, TransmissionBlock::UnverifiedResponse)
            );
            locked.gate.replace_owner("reloaded-owner".into());
            assert!(
                locked
                    .transmission_gate
                    .has_block(&binding, TransmissionBlock::UnverifiedResponse)
            );
            locked.gate.replace_owner("owner".into());
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::UnverifiedResponse)
            );
        }
        state.lock().await.gate.finish_send(&snapshot).unwrap();
        for (error, reason) in [
            (
                notryx_client_runtime::Error::Contract,
                TransmissionBlock::ProtocolMismatch,
            ),
            (
                notryx_client_runtime::Error::TooLarge,
                TransmissionBlock::ProtocolMismatch,
            ),
            (
                notryx_client_runtime::Error::EpochMismatch,
                TransmissionBlock::EpochMismatch,
            ),
            (
                notryx_client_runtime::Error::Binding,
                TransmissionBlock::BindingMismatch,
            ),
        ] {
            let (snapshot, generation) = {
                let mut locked = state.lock().await;
                let snapshot = locked.gate.begin_send("owner").unwrap();
                (
                    snapshot,
                    locked.transmission_gate.response_generation(&binding),
                )
            };
            assert_eq!(
                request_failure(
                    &state, &session, &snapshot, &binding, generation, &item, error
                )
                .await
                .unwrap()
                .status,
                WorkspaceSendStatus::NeedsReview
            );
            let mut locked = state.lock().await;
            assert!(locked.transmission_gate.has_block(&binding, reason));
            locked.gate.replace_owner("reloaded-owner".into());
            assert!(locked.transmission_gate.has_block(&binding, reason));
            locked.gate.replace_owner("owner".into());
            assert!(locked.transmission_gate.clear(&binding, reason));
            locked.gate.finish_send(&snapshot).unwrap();
        }
        let initial = state
            .lock()
            .await
            .transmission_gate
            .begin_initial_sync(&binding)
            .unwrap();
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::SyncPending
        );
        {
            let mut locked = state.lock().await;
            assert!(
                locked
                    .transmission_gate
                    .has_block(&binding, TransmissionBlock::InitialSync)
            );
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::ProtocolMismatch);
        }
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        {
            let mut locked = state.lock().await;
            assert!(
                !locked
                    .transmission_gate
                    .complete_initial_sync(&binding, &initial)
            );
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
            let current = locked
                .transmission_gate
                .begin_initial_sync(&binding)
                .unwrap();
            assert!(
                locked
                    .transmission_gate
                    .complete_initial_sync(&binding, &current)
            );
        }
        for reason in [
            TransmissionBlock::ProtocolMismatch,
            TransmissionBlock::EpochMismatch,
            TransmissionBlock::BindingMismatch,
            TransmissionBlock::ResyncRequired,
            TransmissionBlock::HistoryRegression,
        ] {
            state.lock().await.transmission_gate.block(&binding, reason);
            assert_eq!(
                send_one(state.clone(), "owner").await.unwrap().status,
                if reason == TransmissionBlock::ResyncRequired {
                    WorkspaceSendStatus::SyncPending
                } else {
                    WorkspaceSendStatus::NeedsReview
                }
            );
            assert!(state.lock().await.transmission_gate.clear(&binding, reason));
        }
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::AuthRequired);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(queued.operation_id.as_str())
        );
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
        let stored = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.bytes, b"unsent\n");
        assert_eq!(stored.base_bytes, b"base\n");
        let operation = locked
            .store
            .read_outbox_operation(&binding, &queued.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.status, OutboxStatus::Queued);
        assert_eq!(operation.bytes, b"unsent\n");
        drop(locked);
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_restore_obeys_send_gate_and_records_real_server_receipt() {
        use notryx_protocol::RestoreNodeRequest;
        use notryx_server::{DeleteNode, NodeSnapshot, Outcome};

        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"synthetic base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        let node = NodeSnapshot::from(&document);
        let stream = workspace.register_stream().await.unwrap();
        let receipt = workspace
            .delete_node(DeleteNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: "019a1000-0000-7000-8000-000000000003".parse().unwrap(),
                node_id: node.node_id,
                expected_node_etag: node.etag().unwrap(),
            })
            .await
            .unwrap();
        let Outcome::Deleted(tombstone) = receipt.outcome else {
            panic!("delete")
        };
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let request = RestoreNodeRequest {
            expected_meta_rev: tombstone.meta_rev,
            parent_id: tombstone.parent_id.unwrap(),
            name: "Restored.md".into(),
        };
        let queued = store
            .queue_restore(&binding, tombstone.node_id, &request)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session);
        runtime
            .transmission_gate
            .block(&binding, TransmissionBlock::ProtocolMismatch);
        let state = Arc::new(Mutex::new(runtime));
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        assert!(!notes.join("Restored.md").exists());
        {
            let mut locked = state.lock().await;
            let pending = locked
                .store
                .next_pending_outbox(&binding)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(pending, notryx_client_store::PendingOutboxOperation::Restore(ref item) if item.status == OutboxStatus::Queued)
            );
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
        }
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let (snapshot, session) = {
            let mut locked = state.lock().await;
            (
                locked.gate.begin_send("owner").unwrap(),
                locked.session.clone().unwrap(),
            )
        };
        let pending_state = state.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: pending_state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &pending_state,
                &session,
                &snapshot,
                None,
                Some((ready, resumed)),
                None,
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        {
            let mut locked = state.lock().await;
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::EpochMismatch);
        }
        resume.send(()).unwrap();
        assert_eq!(
            sending.await.unwrap().unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        assert!(!notes.join("Restored.md").exists());
        {
            let mut locked = state.lock().await;
            let pending = locked
                .store
                .next_pending_outbox(&binding)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(pending, PendingOutboxOperation::Restore(ref item) if item.status == OutboxStatus::Queued)
            );
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::EpochMismatch)
            );
        }
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let (snapshot, session) = {
            let mut locked = state.lock().await;
            (
                locked.gate.begin_send("owner").unwrap(),
                locked.session.clone().unwrap(),
            )
        };
        let pending_state = state.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: pending_state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &pending_state,
                &session,
                &snapshot,
                None,
                None,
                Some((ready, resumed)),
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        {
            let mut locked = state.lock().await;
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::EpochMismatch);
        }
        resume.send(()).unwrap();
        assert_eq!(
            sending.await.unwrap().unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        assert_eq!(
            std::fs::read(notes.join("Restored.md")).unwrap(),
            b"synthetic base\n"
        );
        {
            let mut locked = state.lock().await;
            assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
            let pending = locked
                .store
                .next_pending_outbox(&binding)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(pending, PendingOutboxOperation::Restore(ref item) if item.status == OutboxStatus::Sent)
            );
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::EpochMismatch)
            );
        }
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            std::fs::read(notes.join("Restored.md")).unwrap(),
            b"synthetic base\n"
        );
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            1
        );
        assert!(
            locked
                .store
                .next_pending_outbox(&binding)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(queued.operation_seq, 1);
        drop(locked);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn committed_receipt_before_journal_catchup_searches_latest_draft_when_ack_pending() {
        use notryx_client_store::{SearchSource, search_contains};

        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"old cached text\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let id = node.node_id.to_string();
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session.clone());
        let state = Arc::new(Mutex::new(state));
        let runtime = crate::Runtime(state.clone());
        let snapshot = state.lock().await.gate.snapshot("owner").unwrap();
        crate::sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();

        let (queued, before_cursor, before_nodes) = {
            let mut locked = state.lock().await;
            assert!(
                locked
                    .store
                    .cache_content(&binding, &node, b"old cached text\n")
                    .await
                    .unwrap()
            );
            let opened = locked
                .store
                .open_registered_document(
                    &binding,
                    &id,
                    b"old cached text\n",
                    node.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            locked
                .store
                .save_registered_checkpoint(
                    &binding,
                    &id,
                    &opened.edit_session_id,
                    1,
                    b"new committed text\n",
                )
                .await
                .unwrap();
            let queued = locked
                .store
                .queue_checkpoint(&binding, &id, 1)
                .await
                .unwrap();
            let page = locked
                .store
                .search_candidates(&binding, None, 100)
                .await
                .unwrap();
            let candidate = page
                .candidates
                .iter()
                .find(|candidate| candidate.node_id == node.node_id)
                .unwrap();
            assert_eq!(candidate.source, SearchSource::Draft);
            assert_eq!(candidate.bytes(), b"new committed text\n");
            let (cursor, nodes) = locked.store.sync_index(&binding).await.unwrap().unwrap();
            (queued, cursor, nodes)
        };

        let mut injection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_confirm BEFORE UPDATE OF confirmed_through ON outbox_streams BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
            .execute(&mut injection).await.unwrap();
        injection.close().await.unwrap();
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::AckPending);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(queued.operation_id.as_str())
        );
        assert_eq!(
            std::fs::read(notes.join("Example.md")).unwrap(),
            b"new committed text\n"
        );
        let server_node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|current| current.node_id == node.node_id)
            .unwrap();
        let committed_revision = server_node.content_rev.as_ref().unwrap().as_str();
        assert_ne!(server_node.content_rev, node.content_rev);
        let mut locked = state.lock().await;
        assert_eq!(
            locked.store.sync_index(&binding).await.unwrap().unwrap(),
            (before_cursor, before_nodes)
        );
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"new committed text\n");
        assert_eq!(draft.base_bytes, b"new committed text\n");
        assert_eq!(draft.base_revision, committed_revision);
        let operation = locked
            .store
            .read_outbox_operation(&binding, &queued.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.status, OutboxStatus::Committed);
        assert_eq!(operation.bytes, b"new committed text\n");
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            0
        );
        let old_cache = locked
            .store
            .read_cached_content(&binding, node.node_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_cache.bytes, b"old cached text\n");
        let page = locked
            .store
            .search_candidates(&binding, None, 100)
            .await
            .unwrap();
        let candidate = page
            .candidates
            .iter()
            .find(|candidate| candidate.node_id == node.node_id)
            .unwrap();
        assert_eq!(
            candidate.source,
            SearchSource::Draft,
            "the locally committed draft must remain authoritative before journal catch-up"
        );
        assert!(search_contains(candidate.bytes(), "new committed"));
        assert!(!search_contains(candidate.bytes(), "old cached"));
        drop(locked);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retired_delete_blocks_registration_without_replaying_or_losing_intent() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        {
            let mut locked = state.lock().await;
            locked.sync_state = Some(notryx_protocol::WorkspaceSyncState {
                binding: crate::to_contract(binding.clone()),
                status: notryx_protocol::WorkspaceSyncStatus::Ready,
            });
            locked.store.prepare_pending_outbox(&binding).await.unwrap();
        }
        let request = DeleteMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            expected_node_etag: item.expected_node_etag.clone(),
        };
        assert!(matches!(
            session.delete(&request).await.unwrap(),
            OperationReceipt::Deleted { .. }
        ));
        session.acknowledge(1).await.unwrap();
        assert!(matches!(
            session.delete_operation(&request).await.unwrap(),
            OperationLookup::Retired
        ));
        let head = session.server_info().await.unwrap().head_seq;
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::NeedsReview);
        let runtime = crate::Runtime(state.clone());
        assert_eq!(
            crate::sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        let mut locked = state.lock().await;
        assert!(
            locked
                .transmission_gate
                .has_block(&binding, TransmissionBlock::UnverifiedResponse)
        );
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
        assert!(
            matches!(locked.store.next_pending_outbox(&binding).await.unwrap(), Some(PendingOutboxOperation::Delete(ref pending)) if pending.operation_id == item.operation_id && pending.expected_node_etag == item.expected_node_etag && pending.status == OutboxStatus::Sent)
        );
        drop(locked);
        assert_eq!(session.server_info().await.unwrap().head_seq, head);
        assert!(!notes.join("Example.md").exists());
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reader_delete_cancellation_advances_fifo_without_deleting_file() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result: AdminResult::Status { workspace_id, .. },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Principal { principal_id },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::CreatePrincipal {
                role: notryx_protocol::WorkspaceRole::Reader,
            },
        )
        .await
        .unwrap()
        else {
            panic!("reader");
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing { principal_id },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let request = DeleteMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            expected_node_etag: item.expected_node_etag.clone(),
        };
        let first = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(first.status, WorkspaceSendStatus::PermissionDenied);
        assert!(matches!(
            session.delete_operation(&request).await.unwrap(),
            OperationLookup::Missing
        ));
        {
            let mut locked = state.lock().await;
            let Some(PendingOutboxOperation::Delete(sent)) =
                locked.store.next_pending_outbox(&binding).await.unwrap()
            else {
                panic!("delete");
            };
            assert!(
                locked
                    .store
                    .request_delete_abandon_matching(&binding, &sent)
                    .await
                    .unwrap()
            );
        }
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Abandoned);
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            1
        );
        assert!(
            locked
                .store
                .next_pending_outbox(&binding)
                .await
                .unwrap()
                .is_none()
        );
        drop(locked);
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reader_delete_permission_denial_preserves_intent_and_server_file() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result: AdminResult::Status { workspace_id, .. },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Principal { principal_id },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::CreatePrincipal {
                role: notryx_protocol::WorkspaceRole::Reader,
            },
        )
        .await
        .unwrap()
        else {
            panic!("reader");
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing { principal_id },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let head = session.server_info().await.unwrap().head_seq;
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::PermissionDenied);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(item.operation_id.as_str())
        );
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
        assert!(
            matches!(locked.store.next_pending_outbox(&binding).await.unwrap(), Some(PendingOutboxOperation::Delete(ref pending)) if pending.operation_id == item.operation_id && pending.expected_node_etag == item.expected_node_etag && pending.status == OutboxStatus::Sent)
        );
        drop(locked);
        assert_eq!(session.server_info().await.unwrap().head_seq, head);
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stale_delete_sender_records_412_without_overwriting_peer_change() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("second pairing");
        };
        let peer = Session::pair(
            &format!("http://{}", server.local_addr()),
            session.binding().workspace_id,
            &pairing_secret,
        )
        .await
        .unwrap();
        let peer_request = ContentMutation {
            binding: peer.binding().clone(),
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_revision: node.content_rev.clone().unwrap(),
            bytes: b"peer edit\n".to_vec(),
        };
        assert!(matches!(
            peer.put_content(&peer_request).await.unwrap(),
            OperationReceipt::Committed { .. }
        ));
        let head = session.server_info().await.unwrap().head_seq;
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Failed);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(item.operation_id.as_str())
        );
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            1
        );
        let page = locked
            .store
            .list_delete_operations(&binding, 0, 10)
            .await
            .unwrap();
        assert!(matches!(
            page.entries[0].receipt,
            Some(OperationReceipt::NodePreconditionFailed { .. })
        ));
        assert_eq!(page.entries[0].operation.expected_node_etag, node.node_etag);
        assert!(
            locked
                .store
                .next_pending_outbox(&binding)
                .await
                .unwrap()
                .is_none()
        );
        drop(locked);
        assert_eq!(
            std::fs::read(notes.join("Example.md")).unwrap(),
            b"peer edit\n"
        );
        assert_eq!(session.server_info().await.unwrap().head_seq, head);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn admitted_delete_receipt_is_applied_without_discarding_frozen_recovery_edit() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut old = binding.clone();
        old.device_id = uuid::Uuid::new_v4().to_string();
        old.device_stream_id = uuid::Uuid::new_v4().to_string();
        let revision = node.content_rev.as_ref().unwrap().as_str();
        let hash = "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac";
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)").bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        sqlx::query("INSERT INTO client_registrations VALUES (2,?,?,?,?,?,'frozen')")
            .bind(&old.server_origin)
            .bind(&old.workspace_id)
            .bind(&old.workspace_epoch)
            .bind(&old.device_id)
            .bind(&old.device_stream_id)
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO registered_documents VALUES (2,?,'old-storage')")
            .bind(node.node_id.to_string())
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO draft_checkpoints VALUES ('old-storage',1,1,?)")
            .bind(&b"base\n"[..])
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO draft_bases VALUES ('old-storage',?,?)")
            .bind(&b"base\n"[..])
            .bind(revision)
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO outbox_streams VALUES (2,2,1,1)")
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,committed_revision,payload_len) VALUES ('old-content',2,1,?,'old-storage',1,'earlier',?,?,'committed',?,5)")
            .bind(node.node_id.to_string()).bind(&b"base\n"[..]).bind(hash).bind(revision).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let recovery = store
            .open_recovery_draft(&old, &node.node_id.to_string())
            .await
            .unwrap();
        let operation_id = uuid::Uuid::now_v7().to_string();
        let empty_hash = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO outbox_streams VALUES (1,2,0,0)")
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,1,1,'delete',?,?,X'',?,'queued',0)")
            .bind(&operation_id).bind(node.node_id.to_string()).bind(&node.node_etag).bind(empty_hash).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let pending = {
            let mut locked = state.lock().await;
            let mut db = SqliteConnection::connect_with(
                &SqliteConnectOptions::new()
                    .filename(&db_path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
            sqlx::query("UPDATE client_outbox SET status='sent' WHERE operation_id=?")
                .bind(&operation_id)
                .execute(&mut db)
                .await
                .unwrap();
            db.close().await.unwrap();
            let Some(PendingOutboxOperation::Delete(item)) =
                locked.store.next_pending_outbox(&binding).await.unwrap()
            else {
                panic!("delete");
            };
            item
        };
        let request = DeleteMutation {
            binding: session.binding().clone(),
            operation_id: pending.operation_id.parse().unwrap(),
            operation_seq: pending.operation_seq,
            node_id: pending.node_id,
            expected_node_etag: pending.expected_node_etag.clone(),
        };
        assert!(matches!(
            session.delete(&request).await.unwrap(),
            OperationReceipt::Deleted { .. }
        ));
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        locked
            .store
            .save_recovery_checkpoint(
                &old,
                &node.node_id.to_string(),
                &recovery.edit_session_id,
                2,
                b"recovered edit\n",
            )
            .await
            .unwrap();
        let draft = locked
            .store
            .read_registered_checkpoint(&old, &node.node_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"recovered edit\n");
        assert_eq!(draft.base_bytes, b"base\n");
        drop(locked);
        assert!(!notes.join("Example.md").exists());
        let trash = session.trash(None, 100).await.unwrap();
        assert!(
            trash
                .nodes
                .iter()
                .any(|entry| entry.node_id == node.node_id && entry.restorable)
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delete_send_rechecks_frozen_recovery_session_before_transmission() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut old = binding.clone();
        old.device_id = uuid::Uuid::new_v4().to_string();
        old.device_stream_id = uuid::Uuid::new_v4().to_string();
        let revision = node.content_rev.as_ref().unwrap().as_str();
        let hash = "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac";
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)").bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        sqlx::query("INSERT INTO client_registrations VALUES (2,?,?,?,?,?,'frozen')")
            .bind(&old.server_origin)
            .bind(&old.workspace_id)
            .bind(&old.workspace_epoch)
            .bind(&old.device_id)
            .bind(&old.device_stream_id)
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO registered_documents VALUES (2,?,'old-storage')")
            .bind(node.node_id.to_string())
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO draft_checkpoints VALUES ('old-storage',1,1,?)")
            .bind(&b"base\n"[..])
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO draft_bases VALUES ('old-storage',?,?)")
            .bind(&b"base\n"[..])
            .bind(revision)
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO outbox_streams VALUES (2,2,1,1)")
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,committed_revision,payload_len) VALUES ('old-content',2,1,?,'old-storage',1,'earlier',?,?,'committed',?,5)")
            .bind(node.node_id.to_string()).bind(&b"base\n"[..]).bind(hash).bind(revision).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let recovery = store
            .open_recovery_draft(&old, &node.node_id.to_string())
            .await
            .unwrap();
        let operation_id = uuid::Uuid::now_v7().to_string();
        let empty_hash = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO outbox_streams VALUES (1,2,0,0)")
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,1,1,'delete',?,?,X'',?,'queued',0)")
            .bind(&operation_id).bind(node.node_id.to_string()).bind(&node.node_etag).bind(empty_hash).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let result = send_one(state.clone(), "owner").await;
        assert!(matches!(result, Err(ref error) if error == "OutboxBlocked"));
        let mut locked = state.lock().await;
        locked
            .store
            .save_recovery_checkpoint(
                &old,
                &node.node_id.to_string(),
                &recovery.edit_session_id,
                2,
                b"recovered edit\n",
            )
            .await
            .unwrap();
        let draft = locked
            .store
            .read_registered_checkpoint(&old, &node.node_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"recovered edit\n");
        assert_eq!(draft.base_bytes, b"base\n");
        assert!(
            matches!(locked.store.next_pending_outbox(&binding).await.unwrap(), Some(PendingOutboxOperation::Delete(ref item)) if item.operation_id == operation_id && item.status == OutboxStatus::Queued)
        );
        drop(locked);
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delete_outbox_sends_to_real_server_acknowledges_and_preserves_restore_bytes() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(item.operation_id.as_str())
        );
        assert!(!notes.join("Example.md").exists());
        {
            let mut locked = state.lock().await;
            assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
            assert_eq!(
                locked
                    .store
                    .confirmed_receipt_through(&binding)
                    .await
                    .unwrap(),
                1
            );
            assert!(
                locked
                    .store
                    .next_pending_outbox(&binding)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let trash = session.trash(None, 100).await.unwrap();
        let deleted = trash
            .nodes
            .into_iter()
            .find(|entry| entry.node_id == node.node_id)
            .unwrap();
        assert!(deleted.restorable);
        let restore = notryx_client_runtime::RestoreMutation {
            binding: session.binding().clone(),
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: deleted.node_id,
            request: notryx_protocol::RestoreNodeRequest {
                expected_meta_rev: deleted.meta_rev,
                parent_id: deleted.parent_id,
                name: deleted.name,
            },
        };
        assert!(matches!(
            session.restore(&restore).await.unwrap(),
            notryx_protocol::OperationReceipt::Restored { .. }
        ));
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn durable_checkpoint_is_sent_to_real_server_and_receipt_is_recorded() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let id = node.node_id.to_string();
        let edit = store
            .open_registered_document(&binding, &id, b"base\n", node.content_rev.unwrap().as_str())
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"sent\n")
            .await
            .unwrap();
        let first = store.queue_checkpoint(&binding, &id, 1).await.unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        state
            .gate
            .open_session(edit.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            edit.edit_session_id.clone(),
            ProductSession {
                document_id: id.clone(),
                binding: binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let state = Arc::new(Mutex::new(state));
        {
            let mut locked = state.lock().await;
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::ProtocolMismatch);
        }
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        {
            let mut locked = state.lock().await;
            let pending = locked
                .store
                .read_outbox_operation(&binding, &first.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pending.status, OutboxStatus::Queued);
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
        }
        let (lookup_ready, lookup_observed) = tokio::sync::oneshot::channel();
        let (resume_lookup, lookup_resumed) = tokio::sync::oneshot::channel();
        let (snapshot, session) = {
            let mut locked = state.lock().await;
            let session = locked.session.clone().unwrap();
            let snapshot = locked.gate.begin_send("owner").unwrap();
            (snapshot, session)
        };
        let pending_state = state.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: pending_state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &pending_state,
                &session,
                &snapshot,
                None,
                Some((lookup_ready, lookup_resumed)),
                None,
            )
            .await;
            permit.finish().await;
            result
        });
        lookup_observed.await.unwrap();
        {
            let mut locked = state.lock().await;
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::EpochMismatch);
        }
        resume_lookup.send(()).unwrap();
        let blocked = sending.await.unwrap().unwrap();
        assert_eq!(blocked.status, WorkspaceSendStatus::NeedsReview);
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        {
            let mut locked = state.lock().await;
            let draft = locked
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.bytes, b"sent\n");
            assert_eq!(draft.base_bytes, b"base\n");
            let pending = locked
                .store
                .read_outbox_operation(&binding, &first.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pending.bytes, b"sent\n");
            assert_eq!(pending.status, OutboxStatus::Queued);
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::EpochMismatch)
            );
        }
        let mut injection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_confirm BEFORE UPDATE OF confirmed_through ON outbox_streams BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&mut injection).await.unwrap();
        injection.close().await.unwrap();
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::AckPending);
        assert_eq!(result.generation.as_deref(), Some("1"));
        assert_eq!(result.binding, crate::to_contract(binding.clone()));
        let earlier_binding = result.binding.clone();
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"sent\n");
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            0
        );
        let status = locked.document_state(&binding, &id).await.unwrap();
        assert_eq!(status.binding, crate::to_contract(binding.clone()));
        assert_eq!(status.confirmed_through, "0");
        assert_eq!(status.latest.unwrap().generation, "1");
        drop(locked);
        let mut injection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::query("DROP TRIGGER fail_confirm")
            .execute(&mut injection)
            .await
            .unwrap();
        injection.close().await.unwrap();
        for (reason, clear_before_response) in [
            (TransmissionBlock::ProtocolMismatch, true),
            (TransmissionBlock::ResyncRequired, true),
            (TransmissionBlock::ResyncRequired, false),
        ] {
            let (ready, observed) = tokio::sync::oneshot::channel();
            let (resume, resumed) = tokio::sync::oneshot::channel();
            let (snapshot, session, generation) = {
                let mut locked = state.lock().await;
                let snapshot = locked.gate.begin_send("owner").unwrap();
                let session = locked.session.clone().unwrap();
                let generation = locked.transmission_gate.response_generation(&binding);
                (snapshot, session, generation)
            };
            let sending_state = state.clone();
            let ack_binding = binding.clone();
            let receiving_ack = tokio::spawn(async move {
                let permit = SendPermit {
                    state: sending_state.clone(),
                    snapshot,
                    active: true,
                };
                let result = acknowledge_and_record(
                    &sending_state,
                    &session,
                    &snapshot,
                    &ack_binding,
                    1,
                    generation,
                    None,
                    Some((ready, resumed)),
                )
                .await;
                permit.finish().await;
                result
            });
            observed.await.unwrap();
            {
                let mut locked = state.lock().await;
                locked.transmission_gate.block(&binding, reason);
                if clear_before_response {
                    assert!(locked.transmission_gate.clear(&binding, reason));
                }
            }
            resume.send(()).unwrap();
            assert_eq!(
                receiving_ack.await.unwrap().unwrap(),
                Some(if reason == TransmissionBlock::ResyncRequired {
                    WorkspaceSendStatus::SyncPending
                } else {
                    WorkspaceSendStatus::NeedsReview
                })
            );
            if !clear_before_response {
                assert!(state.lock().await.transmission_gate.clear(&binding, reason));
            }
            assert_eq!(
                state
                    .lock()
                    .await
                    .store
                    .confirmed_receipt_through(&binding)
                    .await
                    .unwrap(),
                0
            );
        }
        {
            let mut locked = state.lock().await;
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::ProtocolMismatch);
        }
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        {
            let mut locked = state.lock().await;
            assert_eq!(
                locked
                    .store
                    .confirmed_receipt_through(&binding)
                    .await
                    .unwrap(),
                0
            );
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
        }
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Idle
        );
        let mut locked = state.lock().await;
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            locked
                .document_state(&binding, &id)
                .await
                .unwrap()
                .confirmed_through,
            "1"
        );
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.base_bytes, b"sent\n");
        let queue_request = WorkspaceQueueRequest {
            renderer_session_id: "owner".into(),
            document_id: id.clone(),
            edit_session_id: edit.edit_session_id.clone(),
            expected_generation: "1".into(),
        };
        let reused = locked.queue_document(&queue_request).await.unwrap();
        assert!(reused.committed);
        assert_eq!(reused.operation_id, first.operation_id);
        locked
            .store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 2, b"second\n")
            .await
            .unwrap();
        assert_eq!(
            locked.queue_document(&queue_request).await,
            Err("stale_checkpoint".into())
        );
        let next = locked
            .queue_document(&WorkspaceQueueRequest {
                expected_generation: "2".into(),
                ..queue_request
            })
            .await
            .unwrap();
        assert!(!next.committed);
        let pending = locked
            .store
            .prepare_outbox(&binding)
            .await
            .unwrap()
            .unwrap();
        let session = locked.session.clone().unwrap();
        let request = mutation(&session, &pending).unwrap();
        drop(locked);
        let receipt = session.put_content(&request).await.unwrap();
        assert!(matches!(receipt, OperationReceipt::Committed { .. }));
        let mut locked = state.lock().await;
        locked
            .store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 3, b"newer\n")
            .await
            .unwrap();
        drop(locked);
        for (reason, clear_before_response) in [
            (TransmissionBlock::ProtocolMismatch, true),
            (TransmissionBlock::EpochMismatch, true),
            (TransmissionBlock::BindingMismatch, true),
            (TransmissionBlock::ResyncRequired, true),
            (TransmissionBlock::ResyncRequired, false),
            (TransmissionBlock::HistoryRegression, true),
        ] {
            let (ready, observed) = tokio::sync::oneshot::channel();
            let (resume, resumed) = tokio::sync::oneshot::channel();
            let (snapshot, session) = {
                let mut locked = state.lock().await;
                (
                    locked.gate.begin_send("owner").unwrap(),
                    locked.session.clone().unwrap(),
                )
            };
            let sending_state = state.clone();
            let sending = tokio::spawn(async move {
                let permit = SendPermit {
                    state: sending_state.clone(),
                    snapshot,
                    active: true,
                };
                let result = send_inner(
                    &sending_state,
                    &session,
                    &snapshot,
                    None,
                    Some((ready, resumed)),
                    None,
                )
                .await;
                permit.finish().await;
                result
            });
            observed.await.unwrap();
            {
                let mut locked = state.lock().await;
                locked.transmission_gate.block(&binding, reason);
                if clear_before_response {
                    assert!(locked.transmission_gate.clear(&binding, reason));
                }
            }
            resume.send(()).unwrap();
            assert_eq!(
                sending.await.unwrap().unwrap().status,
                if reason == TransmissionBlock::ResyncRequired {
                    WorkspaceSendStatus::SyncPending
                } else {
                    WorkspaceSendStatus::NeedsReview
                }
            );
            if !clear_before_response {
                assert!(state.lock().await.transmission_gate.clear(&binding, reason));
            }
        }
        let mut locked = state.lock().await;
        let retained = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.bytes, b"newer\n");
        assert_eq!(retained.base_bytes, b"sent\n");
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        assert_eq!(
            locked
                .store
                .read_outbox_operation(&binding, &pending.operation_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            OutboxStatus::Sent
        );
        locked
            .transmission_gate
            .block(&binding, TransmissionBlock::EpochMismatch);
        drop(locked);
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::NeedsReview);
        let mut locked = state.lock().await;
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, 3);
        assert_eq!(draft.bytes, b"newer\n");
        assert_eq!(draft.base_bytes, b"sent\n");
        assert!(
            locked
                .transmission_gate
                .clear(&binding, TransmissionBlock::EpochMismatch)
        );
        drop(locked);
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Committed
        );
        let mut locked = state.lock().await;
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.base_bytes, b"second\n");
        assert_eq!(draft.bytes, b"newer\n");
        let queued = locked
            .store
            .queue_checkpoint(&binding, &id, 3)
            .await
            .unwrap();
        let through = locked.store.receipt_ack_through(&binding).await.unwrap();
        let session = locked.session.clone().unwrap();
        drop(locked);
        let ack = session.acknowledge(through).await.unwrap();
        let mut locked = state.lock().await;
        locked
            .store
            .confirm_receipt_ack(&binding, &ack)
            .await
            .unwrap();
        locked
            .store
            .request_outbox_abandon(&binding, &queued.operation_id)
            .await
            .unwrap();
        drop(locked);
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Abandoned);
        assert_eq!(result.generation.as_deref(), Some("3"));
        assert_eq!(
            std::fs::read(notes.join("Example.md")).unwrap(),
            b"second\n"
        );
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 3);
        assert_eq!(
            locked
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"newer\n"
        );
        locked
            .store
            .close_registered_document(&binding, &id, &edit.edit_session_id)
            .await
            .unwrap();
        locked.gate.close_session(&edit.edit_session_id).unwrap();
        locked.product_sessions.remove(&edit.edit_session_id);
        drop(locked);
        assert!(matches!(
            admin::request(
                &config.server.admin_socket,
                &AdminRequest::RevokeDevice {
                    device_id: binding.device_id.clone()
                }
            )
            .await
            .unwrap(),
            AdminResponse::Ok { .. }
        ));
        let revoked = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(revoked.status, WorkspaceSendStatus::AuthRequired);
        assert_eq!(revoked.binding, earlier_binding);
        let mut locked = state.lock().await;
        assert_eq!(
            locked
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"newer\n"
        );
        drop(locked);
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
        };
        let replacement = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let next_binding = registration_binding(replacement.binding());
        assert_ne!(earlier_binding, crate::to_contract(next_binding.clone()));
        let node = replacement
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let mut locked = state.lock().await;
        locked
            .store
            .activate_registration(&next_binding)
            .await
            .unwrap();
        locked.session = Some(replacement);
        let next = locked
            .store
            .open_registered_document(
                &next_binding,
                &id,
                b"second\n",
                node.content_rev.unwrap().as_str(),
            )
            .await
            .unwrap();
        locked
            .store
            .save_registered_checkpoint(
                &next_binding,
                &id,
                &next.edit_session_id,
                1,
                b"new registration\n",
            )
            .await
            .unwrap();
        locked
            .gate
            .open_session(next.edit_session_id.clone())
            .unwrap();
        locked.product_sessions.insert(
            next.edit_session_id.clone(),
            ProductSession {
                document_id: id.clone(),
                binding: next_binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let blocked = locked
            .queue_document(&WorkspaceQueueRequest {
                renderer_session_id: "owner".into(),
                document_id: id.clone(),
                edit_session_id: next.edit_session_id,
                expected_generation: "1".into(),
            })
            .await;
        assert_eq!(blocked, Err("ReconciliationRequired".into()));
        assert!(
            locked
                .store
                .list_outbox(&next_binding, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        drop(locked);
        let idle = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(idle.status, WorkspaceSendStatus::Idle);
        assert_eq!(idle.binding, crate::to_contract(next_binding));
        assert_ne!(idle.binding, earlier_binding);
        assert_eq!(
            std::fs::read(notes.join("Example.md")).unwrap(),
            b"second\n"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn changed_queued_payload_and_blocked_abandon_do_not_change_server() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let id = node.node_id.to_string();
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let edit = store
            .open_registered_document(&binding, &id, b"base\n", node.content_rev.unwrap().as_str())
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"old\n")
            .await
            .unwrap();
        let first = store.queue_checkpoint(&binding, &id, 1).await.unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let snapshot = state.lock().await.gate.begin_send("owner").unwrap();
        let sending_state = state.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: sending_state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &sending_state,
                &session,
                &snapshot,
                None,
                Some((ready, resumed)),
                None,
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        {
            let mut locked = state.lock().await;
            locked
                .store
                .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 2, b"new\n")
                .await
                .unwrap();
            let updated = locked
                .store
                .queue_checkpoint(&binding, &id, 2)
                .await
                .unwrap();
            assert_eq!(updated.operation_id, first.operation_id);
        }
        resume.send(()).unwrap();
        assert_eq!(
            sending.await.unwrap().unwrap().status,
            WorkspaceSendStatus::Pending
        );
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        {
            let mut locked = state.lock().await;
            let item = locked
                .store
                .read_outbox_operation(&binding, &first.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(item.status, OutboxStatus::Queued);
            assert_eq!(item.bytes, b"new\n");
            locked
                .store
                .request_outbox_abandon(&binding, &first.operation_id)
                .await
                .unwrap();
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::ProtocolMismatch);
        }
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        {
            let mut locked = state.lock().await;
            let item = locked
                .store
                .read_outbox_operation(&binding, &first.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                item.abandon_state,
                notryx_client_store::OutboxAbandonState::Requested
            );
            assert_eq!(item.bytes, b"new\n");
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
        }
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Abandoned
        );
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retired_operation_during_resync_still_requires_review() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let id = node.node_id.to_string();
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let edit = store
            .open_registered_document(&binding, &id, b"base\n", node.content_rev.unwrap().as_str())
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"sent\n")
            .await
            .unwrap();
        let queued = store.queue_checkpoint(&binding, &id, 1).await.unwrap();
        let item = store.prepare_outbox(&binding).await.unwrap().unwrap();
        let request = mutation(&session, &item).unwrap();
        assert!(matches!(
            session.put_content(&request).await.unwrap(),
            OperationReceipt::Committed { .. }
        ));
        session.acknowledge(1).await.unwrap();
        assert_eq!(
            session.operation(&request).await.unwrap(),
            OperationLookup::Retired
        );
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        runtime.sync_state = Some(notryx_protocol::WorkspaceSyncState {
            binding: crate::to_contract(binding.clone()),
            status: notryx_protocol::WorkspaceSyncStatus::Ready,
        });
        let state = Arc::new(Mutex::new(runtime));
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let snapshot = state.lock().await.gate.begin_send("owner").unwrap();
        let sending_state = state.clone();
        let sending = tokio::spawn(async move {
            let permit = SendPermit {
                state: sending_state.clone(),
                snapshot,
                active: true,
            };
            let result = send_inner(
                &sending_state,
                &session,
                &snapshot,
                None,
                Some((ready, resumed)),
                None,
            )
            .await;
            permit.finish().await;
            result
        });
        observed.await.unwrap();
        {
            let mut locked = state.lock().await;
            locked
                .transmission_gate
                .block(&binding, TransmissionBlock::ResyncRequired);
            assert!(
                locked
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ResyncRequired)
            );
        }
        resume.send(()).unwrap();
        assert_eq!(
            sending.await.unwrap().unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        let mut locked = state.lock().await;
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"sent\n");
        assert_eq!(draft.base_bytes, b"base\n");
        assert!(
            locked
                .transmission_gate
                .has_block(&binding, TransmissionBlock::UnverifiedResponse)
        );
        assert_eq!(
            locked
                .store
                .read_outbox_operation(&binding, &queued.operation_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            OutboxStatus::Sent
        );
        drop(locked);
        let runtime = crate::Runtime(state.clone());
        assert_eq!(
            crate::sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        state
            .lock()
            .await
            .gate
            .replace_owner("reloaded-owner".into());
        assert_eq!(
            crate::sync_lifecycle::sync_state_for_renderer(&runtime, "reloaded-owner")
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retired_content_receipt_blocks_current_registration_and_preserves_draft() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let id = node.node_id.to_string();
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let edit = store
            .open_registered_document(&binding, &id, b"base\n", node.content_rev.unwrap().as_str())
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"sent\n")
            .await
            .unwrap();
        let queued = store.queue_checkpoint(&binding, &id, 1).await.unwrap();
        let item = store.prepare_outbox(&binding).await.unwrap().unwrap();
        let request = mutation(&session, &item).unwrap();
        assert!(matches!(
            session.put_content(&request).await.unwrap(),
            OperationReceipt::Committed { .. }
        ));
        session.acknowledge(1).await.unwrap();
        assert_eq!(
            session.operation(&request).await.unwrap(),
            OperationLookup::Retired
        );
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 2, b"newer draft\n")
            .await
            .unwrap();
        let before = session.server_info().await.unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        runtime.sync_state = Some(notryx_protocol::WorkspaceSyncState {
            binding: crate::to_contract(binding.clone()),
            status: notryx_protocol::WorkspaceSyncStatus::Ready,
        });
        let state = Arc::new(Mutex::new(runtime));
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        let runtime = crate::Runtime(state.clone());
        assert_eq!(
            crate::sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::NeedsReview
        );
        {
            let mut locked = state.lock().await;
            assert!(
                locked
                    .transmission_gate
                    .has_block(&binding, TransmissionBlock::UnverifiedResponse)
            );
            assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
            let draft = locked
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.bytes, b"newer draft\n");
            assert_eq!(draft.base_bytes, b"base\n");
            let pending = locked
                .store
                .read_outbox_operation(&binding, &queued.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert!(pending == item);
            assert_eq!(pending.status, OutboxStatus::Sent);
            assert_eq!(pending.bytes, b"sent\n");
        }
        assert_eq!(
            session.server_info().await.unwrap().head_seq,
            before.head_seq
        );
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"sent\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn put_response_during_or_after_resync_is_rechecked_without_losing_draft() {
        for clear_before_response in [true, false] {
            let directory = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let notes = directory.path().join("notes");
            let server_state = directory.path().join("server-state");
            std::fs::create_dir(&notes).unwrap();
            std::fs::create_dir(&server_state).unwrap();
            let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
            std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
            let document = workspace.adopt_root_document("Example.md").await.unwrap();
            workspace.close().await.unwrap();
            let config = Config {
                sync: Default::default(),
                server: ServerConfig {
                    listen: "127.0.0.1:0".parse().unwrap(),
                    admin_socket: directory.path().join("admin.sock"),
                },
                workspace: WorkspaceConfig {
                    notes_root: notes.clone(),
                    state_root: server_state,
                    uploads: Default::default(),
                },
            };
            let server = RunningServer::start(config.clone()).await.unwrap();
            let AdminResponse::Ok {
                result:
                    AdminResult::Status {
                        workspace_id,
                        owner_principal_id,
                        ..
                    },
            } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
                .await
                .unwrap()
            else {
                panic!("status")
            };
            let AdminResponse::Ok {
                result: AdminResult::Pairing { pairing_secret, .. },
            } = admin::request(
                &config.server.admin_socket,
                &AdminRequest::IssuePairing {
                    principal_id: owner_principal_id,
                },
            )
            .await
            .unwrap()
            else {
                panic!("pairing")
            };
            let session = Arc::new(
                Session::pair(
                    &format!("http://{}", server.local_addr()),
                    workspace_id,
                    &pairing_secret,
                )
                .await
                .unwrap(),
            );
            let binding = registration_binding(session.binding());
            let node = session
                .inventory(None)
                .await
                .unwrap()
                .nodes
                .into_iter()
                .find(|node| node.node_id == document.node_id)
                .unwrap();
            let id = node.node_id.to_string();
            let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
                .await
                .unwrap();
            store.activate_registration(&binding).await.unwrap();
            let edit = store
                .open_registered_document(
                    &binding,
                    &id,
                    b"base\n",
                    node.content_rev.unwrap().as_str(),
                )
                .await
                .unwrap();
            store
                .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"sent\n")
                .await
                .unwrap();
            let queued = store.queue_checkpoint(&binding, &id, 1).await.unwrap();
            let mut runtime = RuntimeState::new(store, "owner".into());
            runtime.session = Some(session.clone());
            let state = Arc::new(Mutex::new(runtime));
            let (ready, observed) = tokio::sync::oneshot::channel();
            let (resume, resumed) = tokio::sync::oneshot::channel();
            let snapshot = state.lock().await.gate.begin_send("owner").unwrap();
            let sending_state = state.clone();
            let sending = tokio::spawn(async move {
                let permit = SendPermit {
                    state: sending_state.clone(),
                    snapshot,
                    active: true,
                };
                let result = send_inner(
                    &sending_state,
                    &session,
                    &snapshot,
                    None,
                    None,
                    Some((ready, resumed)),
                )
                .await;
                permit.finish().await;
                result
            });
            observed.await.unwrap();
            assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"sent\n");
            {
                let mut locked = state.lock().await;
                locked
                    .transmission_gate
                    .block(&binding, TransmissionBlock::ResyncRequired);
                if clear_before_response {
                    assert!(
                        locked
                            .transmission_gate
                            .clear(&binding, TransmissionBlock::ResyncRequired)
                    );
                }
            }
            resume.send(()).unwrap();
            assert_eq!(
                sending.await.unwrap().unwrap().status,
                WorkspaceSendStatus::SyncPending
            );
            {
                let mut locked = state.lock().await;
                let draft = locked
                    .store
                    .read_registered_checkpoint(&binding, &id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(draft.bytes, b"sent\n");
                assert_eq!(draft.base_bytes, b"base\n");
                assert_eq!(
                    locked
                        .store
                        .read_outbox_operation(&binding, &queued.operation_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    OutboxStatus::Sent
                );
                if !clear_before_response {
                    assert!(
                        locked
                            .transmission_gate
                            .clear(&binding, TransmissionBlock::ResyncRequired)
                    );
                }
            }
            assert_eq!(
                send_one(state.clone(), "owner").await.unwrap().status,
                WorkspaceSendStatus::Committed
            );
            server.shutdown().await.unwrap();
        }
    }
}
