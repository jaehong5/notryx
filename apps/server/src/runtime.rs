use crate::auth::AuthError;
use crate::config::WorkspaceConfig;
use crate::{
    AbandonOperation, AckOperations, CreateNode, DeleteNode, Document, MoveNode, OperationRecord,
    OperationStreamState, RestoreNode, SaveContent, Workspace,
};
use notryx_domain::{NodeId, OperationId};
use notryx_protocol::{
    AdminRecoverySummary, AdminRequest, AdminResult, ContentPrecondition, InventoryPage,
    NodeMetadata, NodeState, PairRequest, PairResponse, ServerInfo, TrashPage, WorkspaceRole,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use uuid::Uuid;

mod upload;
pub(crate) use upload::{CHUNK_BYTES, UploadSession, UploadState};

const COMMAND_CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeError {
    Start,
    Workspace(crate::Error),
    Auth(AuthError),
    PreconditionFailed,
    Closed,
    Busy,
    Worker,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for RuntimeError {}

impl RuntimeError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Auth(AuthError::Unauthorized) => "unauthorized",
            Self::Auth(AuthError::InvalidRequest) => "invalid_request",
            Self::Auth(AuthError::Forbidden) => "forbidden",
            Self::Auth(AuthError::NotFound) => "not_found",
            Self::Auth(AuthError::Storage) => "storage_error",
            Self::Auth(AuthError::NoSpace) => "insufficient_storage",
            Self::PreconditionFailed => "content_revision_mismatch",
            Self::Workspace(crate::Error::MigrationRequired) => "migration_required",
            Self::Workspace(crate::Error::InvalidRequest) => "invalid_request",
            Self::Workspace(crate::Error::NotFound) => "not_found",
            Self::Workspace(crate::Error::NodeDeleted) => "node_deleted",
            Self::Workspace(crate::Error::Locked) => "locked",
            Self::Workspace(crate::Error::RecoveryRequired) => "recovery_required",
            Self::Workspace(crate::Error::WorkspaceMismatch) => "epoch_mismatch",
            Self::Workspace(crate::Error::UnknownStream) => "unknown_stream",
            Self::Workspace(crate::Error::OperationGap) => "operation_gap",
            Self::Workspace(crate::Error::IdempotencyMismatch) => "idempotency_mismatch",
            Self::Workspace(crate::Error::ReceiptLimit) => "receipt_limit",
            Self::Workspace(crate::Error::ExternalChange) => "external_change",
            Self::Workspace(crate::Error::DigestMismatch) => "digest_mismatch",
            Self::Workspace(crate::Error::OperationRetired) => "operation_retired",
            Self::Workspace(crate::Error::InvalidAck) => "invalid_ack",
            Self::Workspace(crate::Error::OperationPending) => "operation_pending",
            Self::Workspace(crate::Error::NoSpace) => "insufficient_storage",
            Self::Workspace(crate::Error::UploadQuota) => "upload_quota",
            Self::Workspace(crate::Error::UploadTooLarge) => "upload_too_large",
            Self::Workspace(crate::Error::UploadUnavailable) => "upload_unavailable",
            Self::Workspace(_) => "storage_error",
            Self::Start | Self::Worker => "runtime_error",
            Self::Closed => "closed",
            Self::Busy => "busy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub ready: bool,
}

#[derive(Clone)]
pub struct WorkspaceHandle {
    commands: mpsc::Sender<Command>,
    stopping: Arc<AtomicBool>,
    uploads: Arc<UploadState>,
}

enum Command {
    Authenticate {
        token: String,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    #[cfg(test)]
    Barrier {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    BeginUpload {
        token: String,
        hash: String,
        size: Option<u64>,
        reply: oneshot::Sender<Result<crate::UploadGrant, RuntimeError>>,
    },
    FinishUpload {
        token: String,
        id: Uuid,
        sealed: notryx_server_storage::SealedUpload,
        reply: oneshot::Sender<Result<crate::Uploaded, RuntimeError>>,
    },
    AbortUpload {
        id: Uuid,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    DeleteNode {
        token: String,
        request: DeleteNode,
        reply: oneshot::Sender<Result<OperationRecord, RuntimeError>>,
    },
    RestoreNode {
        token: String,
        request: RestoreNode,
        reply: oneshot::Sender<Result<OperationRecord, RuntimeError>>,
    },
    MoveNode {
        token: String,
        request: MoveNode,
        reply: oneshot::Sender<Result<OperationRecord, RuntimeError>>,
    },
    NodeState {
        token: String,
        node_id: NodeId,
        reply: oneshot::Sender<Result<NodeState, RuntimeError>>,
    },
    Trash {
        token: String,
        after_id: Option<NodeId>,
        limit: u16,
        reply: oneshot::Sender<Result<TrashPage, RuntimeError>>,
    },
    Inventory {
        token: String,
        after_id: Option<NodeId>,
        limit: u16,
        reply: oneshot::Sender<Result<InventoryPage, RuntimeError>>,
    },
    CreateNode {
        token: String,
        request: CreateNode,
        reply: oneshot::Sender<Result<OperationRecord, RuntimeError>>,
    },
    AckOperations {
        token: String,
        request: AckOperations,
        reply: oneshot::Sender<Result<OperationStreamState, RuntimeError>>,
    },
    AbandonOperation {
        token: String,
        request: AbandonOperation,
        reply: oneshot::Sender<Result<OperationRecord, RuntimeError>>,
    },
    Health {
        reply: oneshot::Sender<Health>,
    },
    Admin {
        request: AdminRequest,
        reply: oneshot::Sender<Result<AdminResult, RuntimeError>>,
    },
    Pair {
        request: PairRequest,
        reply: oneshot::Sender<Result<PairResponse, RuntimeError>>,
    },
    ServerInfo {
        token: String,
        reply: oneshot::Sender<Result<ServerInfo, RuntimeError>>,
    },
    Metadata {
        token: String,
        node_id: NodeId,
        reply: oneshot::Sender<Result<NodeMetadata, RuntimeError>>,
    },
    Content {
        token: String,
        node_id: NodeId,
        precondition: Option<ContentPrecondition>,
        reply: oneshot::Sender<Result<(Document, Vec<u8>), RuntimeError>>,
    },
    SaveContent {
        token: String,
        request: SaveContent,
        reply: oneshot::Sender<Result<OperationRecord, RuntimeError>>,
        body_permit: Option<Arc<OwnedSemaphorePermit>>,
    },
    Operation {
        token: String,
        operation_id: OperationId,
        reply: oneshot::Sender<Result<OperationRecord, RuntimeError>>,
    },
}

impl WorkspaceHandle {
    #[cfg(test)]
    pub(crate) async fn test_barrier(&self) -> oneshot::Sender<()> {
        let (entered, ready) = oneshot::channel();
        let (release, held) = oneshot::channel();
        self.enqueue(Command::Barrier {
            entered,
            release: held,
        })
        .unwrap();
        ready.await.unwrap();
        release
    }

    #[cfg(test)]
    pub(crate) fn pending_commands(&self) -> usize {
        COMMAND_CAPACITY - self.commands.capacity()
    }

    pub async fn delete_node(
        &self,
        token: String,
        request: DeleteNode,
    ) -> Result<OperationRecord, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::DeleteNode {
            token,
            request,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn restore_node(
        &self,
        token: String,
        request: RestoreNode,
    ) -> Result<OperationRecord, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::RestoreNode {
            token,
            request,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn move_node(
        &self,
        token: String,
        request: MoveNode,
    ) -> Result<OperationRecord, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::MoveNode {
            token,
            request,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn node_state(
        &self,
        token: String,
        node_id: NodeId,
    ) -> Result<NodeState, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::NodeState {
            token,
            node_id,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn trash(
        &self,
        token: String,
        after_id: Option<NodeId>,
        limit: u16,
    ) -> Result<TrashPage, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Trash {
            token,
            after_id,
            limit,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn inventory(
        &self,
        token: String,
        after_id: Option<NodeId>,
        limit: u16,
    ) -> Result<InventoryPage, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Inventory {
            token,
            after_id,
            limit,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn create_node(
        &self,
        token: String,
        request: CreateNode,
    ) -> Result<OperationRecord, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::CreateNode {
            token,
            request,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }
    pub async fn ack_operations(
        &self,
        token: String,
        request: AckOperations,
    ) -> Result<OperationStreamState, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::AckOperations {
            token,
            request,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn abandon_operation(
        &self,
        token: String,
        request: AbandonOperation,
    ) -> Result<OperationRecord, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::AbandonOperation {
            token,
            request,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn save_content(
        &self,
        token: String,
        request: SaveContent,
    ) -> Result<OperationRecord, RuntimeError> {
        self.save_content_inner(token, request, None).await
    }

    pub(crate) async fn save_content_bounded(
        &self,
        token: String,
        request: SaveContent,
        permit: Arc<OwnedSemaphorePermit>,
    ) -> Result<OperationRecord, RuntimeError> {
        self.save_content_inner(token, request, Some(permit)).await
    }

    async fn save_content_inner(
        &self,
        token: String,
        request: SaveContent,
        body_permit: Option<Arc<OwnedSemaphorePermit>>,
    ) -> Result<OperationRecord, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::SaveContent {
            token,
            request,
            reply,
            body_permit,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn operation(
        &self,
        token: String,
        operation_id: OperationId,
    ) -> Result<OperationRecord, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Operation {
            token,
            operation_id,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn server_info(&self, token: String) -> Result<ServerInfo, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::ServerInfo { token, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub(crate) async fn authenticate(&self, token: String) -> Result<(), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Authenticate { token, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn metadata(
        &self,
        token: String,
        node_id: NodeId,
    ) -> Result<NodeMetadata, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Metadata {
            token,
            node_id,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn content(
        &self,
        token: String,
        node_id: NodeId,
        precondition: Option<ContentPrecondition>,
    ) -> Result<(Document, Vec<u8>), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Content {
            token,
            node_id,
            precondition,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn admin(&self, request: AdminRequest) -> Result<AdminResult, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Admin { request, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn pair(&self, request: PairRequest) -> Result<PairResponse, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Pair { request, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn health(&self) -> Result<Health, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Health { reply })?;
        let health = response.await.map_err(|_| RuntimeError::Closed)?;
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        Ok(health)
    }

    fn enqueue(&self, command: Command) -> Result<(), RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RuntimeError::Busy,
                mpsc::error::TrySendError::Closed(_) => RuntimeError::Closed,
            })
    }
}

pub struct WorkspaceRuntime {
    handle: WorkspaceHandle,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<Result<(), RuntimeError>>>,
}

enum Startup {
    Open,
    Initialize,
}

pub fn parse_admin_id(value: &str) -> Result<Uuid, RuntimeError> {
    let id = Uuid::parse_str(value).map_err(|_| RuntimeError::Auth(AuthError::InvalidRequest))?;
    if id.get_version_num() != 4 || id.get_variant() != uuid::Variant::RFC4122 {
        return Err(RuntimeError::Auth(AuthError::InvalidRequest));
    }
    Ok(id)
}

fn now() -> Result<i64, RuntimeError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| RuntimeError::Worker)?
        .as_secs()
        .try_into()
        .map_err(|_| RuntimeError::Worker)
}

async fn execute_admin(
    workspace: &mut Workspace,
    request: AdminRequest,
) -> Result<AdminResult, RuntimeError> {
    match request {
        AdminRequest::RecoveryList {
            after_operation_id,
            limit,
        } => {
            if !(1..=20).contains(&limit) {
                return Err(RuntimeError::Workspace(crate::Error::InvalidRequest));
            }
            let mut entries = workspace
                .list_recovery(after_operation_id, limit + 1)
                .await
                .map_err(RuntimeError::Workspace)?;
            let more = entries.len() > usize::from(limit);
            entries.truncate(usize::from(limit));
            let next_after_operation_id = more.then(|| entries.last().unwrap().operation_id);
            Ok(AdminResult::RecoveryList {
                entries: entries.into_iter().map(recovery_summary).collect(),
                next_after_operation_id,
            })
        }
        AdminRequest::RecoveryInspect {
            operation_id,
            after_hash,
        } => {
            let detail = workspace
                .inspect_recovery_page(operation_id, after_hash.as_deref())
                .await
                .map_err(RuntimeError::Workspace)?;
            Ok(AdminResult::RecoveryDetail {
                objects_truncated: detail.next_after_hash.is_some(),
                next_after_hash: detail.next_after_hash,
                summary: recovery_summary(detail.summary),
                paths: detail.paths,
                objects: detail
                    .objects
                    .into_iter()
                    .map(|hash| format!("sha256:{hash}"))
                    .collect(),
            })
        }
        AdminRequest::RecoveryResolve { operation_id } => {
            workspace
                .resolve_recovery(operation_id)
                .await
                .map_err(RuntimeError::Workspace)?;
            Ok(AdminResult::RecoveryResolved {
                operation_id,
                ready: workspace.ready(),
            })
        }
        AdminRequest::RecoveryRelease { operation_id } => {
            workspace
                .release_recovery(operation_id)
                .await
                .map_err(RuntimeError::Workspace)?;
            Ok(AdminResult::Done {})
        }
        AdminRequest::Status {} => {
            let status = workspace.auth_status().await.map_err(RuntimeError::Auth)?;
            Ok(AdminResult::Status {
                workspace_id: status.workspace_id,
                workspace_epoch: status.workspace_epoch,
                owner_principal_id: status.owner_principal_id.to_string(),
            })
        }
        AdminRequest::CreatePrincipal { role } => {
            let principal = workspace
                .create_principal(role)
                .await
                .map_err(RuntimeError::Auth)?;
            Ok(AdminResult::Principal {
                principal_id: principal.to_string(),
            })
        }
        AdminRequest::SetRole { principal_id, role } => {
            workspace
                .set_role(parse_admin_id(&principal_id)?, role)
                .await
                .map_err(RuntimeError::Auth)?;
            Ok(AdminResult::Done {})
        }
        AdminRequest::TransferOwner {
            principal_id,
            previous_role,
        } => {
            workspace
                .transfer_owner(parse_admin_id(&principal_id)?, previous_role)
                .await
                .map_err(RuntimeError::Auth)?;
            Ok(AdminResult::Done {})
        }
        AdminRequest::IssuePairing { principal_id } => {
            let grant = workspace
                .issue_pairing(parse_admin_id(&principal_id)?, now()?)
                .await
                .map_err(RuntimeError::Auth)?;
            Ok(AdminResult::Pairing {
                workspace_id: grant.workspace_id,
                principal_id: grant.principal_id.to_string(),
                pairing_secret: grant.pairing_secret,
                expires_at: grant.expires_at,
            })
        }
        AdminRequest::RevokeDevice { device_id } => {
            workspace
                .revoke_device(parse_admin_id(&device_id)?, now()?)
                .await
                .map_err(RuntimeError::Auth)?;
            Ok(AdminResult::Done {})
        }
    }
}

fn recovery_summary(summary: crate::RecoverySummary) -> AdminRecoverySummary {
    AdminRecoverySummary {
        operation_id: summary.operation_id,
        resolved: summary.resolved,
        retained_objects: summary.retained_objects.to_string(),
    }
}

async fn execute_save_content(
    workspace: &mut Workspace,
    token: &str,
    request: SaveContent,
) -> Result<OperationRecord, RuntimeError> {
    let identity = workspace
        .authenticate(token, now()?)
        .await
        .map_err(RuntimeError::Auth)?;
    if request.epoch != workspace.epoch() {
        return Err(RuntimeError::Workspace(crate::Error::WorkspaceMismatch));
    }
    if request.stream_id != identity.stream_id {
        return Err(RuntimeError::Auth(AuthError::Forbidden));
    }
    let stream_id = request.stream_id;
    let (fingerprint, previous) = workspace
        .prepare_save(&request)
        .await
        .map_err(RuntimeError::Workspace)?;
    let receipt = if let Some(previous) = previous {
        previous
    } else {
        if identity.role == WorkspaceRole::Reader {
            return Err(RuntimeError::Auth(AuthError::Forbidden));
        }
        workspace
            .save_new(request, &fingerprint, Some(token))
            .await
            .map_err(workspace_error)?
    };
    workspace
        .state
        .operation(stream_id, receipt.operation_id)
        .await
        .map_err(RuntimeError::Workspace)
}

async fn execute(command: Command, workspace: &mut Workspace, shutting_down: bool) {
    match command {
        Command::Authenticate { token, reply } => {
            if !reply.is_closed() {
                let result = match now() {
                    Ok(now) if !shutting_down => workspace
                        .authenticate(&token, now)
                        .await
                        .map(|_| ())
                        .map_err(RuntimeError::Auth),
                    Ok(_) => Err(RuntimeError::Closed),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
        }
        #[cfg(test)]
        Command::Barrier { entered, release } => {
            let _ = entered.send(());
            let _ = release.await;
        }
        Command::BeginUpload {
            token,
            hash,
            size,
            reply,
        } => {
            if !reply.is_closed() {
                let result = match now() {
                    Ok(now) if !shutting_down => workspace
                        .begin_upload(&token, hash, size, now)
                        .await
                        .map_err(workspace_error),
                    Ok(_) => Err(RuntimeError::Closed),
                    Err(error) => Err(error),
                };
                if let Err(Ok(grant)) = reply.send(result) {
                    let id = grant.upload_id;
                    drop(grant);
                    let _ = workspace.abort_upload(id).await;
                }
            }
        }
        Command::FinishUpload {
            token,
            id,
            sealed,
            reply,
        } => {
            let result = match now() {
                Ok(now) => workspace
                    .finish_upload(&token, id, sealed, now)
                    .await
                    .map_err(workspace_error),
                Err(error) => Err(error),
            };
            let _ = reply.send(result);
        }
        Command::AbortUpload { id, reply } => {
            let _ = reply.send(workspace.abort_upload(id).await.map_err(workspace_error));
        }
        Command::DeleteNode {
            token,
            request,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    authenticate_stream(workspace, &token, request.epoch, request.stream_id)
                        .await?;
                    let stream_id = request.stream_id;
                    let receipt = workspace
                        .delete_node_authorized(request, Some(&token))
                        .await
                        .map_err(workspace_error)?;
                    workspace
                        .state
                        .operation(stream_id, receipt.operation_id)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::RestoreNode {
            token,
            request,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    authenticate_stream(workspace, &token, request.epoch, request.stream_id)
                        .await?;
                    let stream_id = request.stream_id;
                    let receipt = workspace
                        .restore_node_authorized(request, Some(&token))
                        .await
                        .map_err(workspace_error)?;
                    workspace
                        .state
                        .operation(stream_id, receipt.operation_id)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::MoveNode {
            token,
            request,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    authenticate_stream(workspace, &token, request.epoch, request.stream_id)
                        .await?;
                    let stream_id = request.stream_id;
                    let receipt = workspace
                        .move_node_authorized(request, Some(&token))
                        .await
                        .map_err(workspace_error)?;
                    workspace
                        .state
                        .operation(stream_id, receipt.operation_id)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::NodeState {
            token,
            node_id,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    workspace
                        .node_state(node_id)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::Trash {
            token,
            after_id,
            limit,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    workspace
                        .trash(after_id, limit)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::Inventory {
            token,
            after_id,
            limit,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    workspace
                        .inventory(after_id, limit)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::CreateNode {
            token,
            request,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    authenticate_stream(workspace, &token, request.epoch, request.stream_id)
                        .await?;
                    let stream_id = request.stream_id;
                    let receipt = workspace
                        .create_node_authorized(request, Some(&token))
                        .await
                        .map_err(workspace_error)?;
                    workspace
                        .state
                        .operation(stream_id, receipt.operation_id)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::AckOperations {
            token,
            request,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    authenticate_stream(workspace, &token, request.epoch, request.stream_id)
                        .await?;
                    workspace
                        .ack_operations(request)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::AbandonOperation {
            token,
            request,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    authenticate_stream(workspace, &token, request.epoch, request.stream_id)
                        .await?;
                    workspace
                        .abandon_operation(request)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::Health { reply } => {
            let _ = reply.send(Health {
                ready: !shutting_down && workspace.ready(),
            });
        }
        Command::Admin { request, reply } => {
            if !reply.is_closed() {
                let _ = reply.send(execute_admin(workspace, request).await);
            }
        }
        Command::Pair { request, reply } => {
            if !reply.is_closed() {
                let result = match now() {
                    Ok(now) => workspace
                        .pair(request, now)
                        .await
                        .map_err(RuntimeError::Auth),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
        }
        Command::ServerInfo { token, reply } => {
            if !reply.is_closed() {
                let result = async {
                    workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    let mut info = workspace
                        .server_info()
                        .await
                        .map_err(RuntimeError::Workspace)?;
                    info.ready &= !shutting_down;
                    Ok(info)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::Metadata {
            token,
            node_id,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    workspace
                        .node_metadata(node_id)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::Content {
            token,
            node_id,
            precondition,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    let (document, bytes) = workspace
                        .read(node_id)
                        .await
                        .map_err(RuntimeError::Workspace)?;
                    if let Some(precondition) = precondition
                        && !precondition
                            .matches(&document.content_rev)
                            .map_err(|_| RuntimeError::Workspace(crate::Error::InvalidFormat))?
                    {
                        return Err(RuntimeError::PreconditionFailed);
                    }
                    Ok((document, bytes))
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::SaveContent {
            token,
            request,
            reply,
            body_permit: _body_permit,
        } => {
            if reply.is_closed() {
                drop(request);
            } else {
                // Caller cancellation cannot interrupt a write after durable admission.
                let result = execute_save_content(workspace, &token, request).await;
                let _ = reply.send(result);
            }
        }
        Command::Operation {
            token,
            operation_id,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    let identity = workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    workspace
                        .state
                        .operation(identity.stream_id, operation_id)
                        .await
                        .map_err(RuntimeError::Workspace)
                }
                .await;
                let _ = reply.send(result);
            }
        }
    }
}

async fn authenticate_stream(
    workspace: &mut Workspace,
    token: &str,
    epoch: notryx_domain::WorkspaceEpoch,
    stream_id: Uuid,
) -> Result<(), RuntimeError> {
    let identity = workspace
        .authenticate(token, now()?)
        .await
        .map_err(RuntimeError::Auth)?;
    if epoch != workspace.epoch() {
        return Err(RuntimeError::Workspace(crate::Error::WorkspaceMismatch));
    }
    if stream_id != identity.stream_id {
        return Err(RuntimeError::Auth(AuthError::Forbidden));
    }
    Ok(())
}

async fn run_worker(
    config: WorkspaceConfig,
    startup: Startup,
    started: oneshot::Sender<Result<(), RuntimeError>>,
    mut commands: mpsc::Receiver<Command>,
    mut stop: oneshot::Receiver<()>,
) -> Result<(), RuntimeError> {
    let opened = match startup {
        Startup::Open => Workspace::open(&config.notes_root, &config.state_root).await,
        Startup::Initialize => Workspace::initialize(&config.notes_root, &config.state_root).await,
    };
    let mut workspace = match opened {
        Ok(workspace) => workspace,
        Err(error) => {
            let error = RuntimeError::Workspace(error);
            let _ = started.send(Err(error));
            return Err(error);
        }
    };
    if let Err(error) = workspace.set_upload_limits(config.uploads) {
        let error = RuntimeError::Workspace(error);
        let _ = started.send(Err(error));
        workspace.close().await.map_err(RuntimeError::Workspace)?;
        return Err(error);
    }
    if started.send(Ok(())).is_err() {
        return workspace.close().await.map_err(RuntimeError::Workspace);
    }
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                commands.close();
                while let Some(command) = commands.recv().await { execute(command, &mut workspace, true).await; }
                break;
            }
            command = commands.recv() => match command {
                Some(command) => execute(command, &mut workspace, false).await,
                None => break,
            },
        }
    }
    workspace.close().await.map_err(RuntimeError::Workspace)
}

impl WorkspaceRuntime {
    pub async fn start(config: WorkspaceConfig) -> Result<Self, RuntimeError> {
        Self::start_with(config, Startup::Open).await
    }

    pub async fn initialize(config: WorkspaceConfig) -> Result<(), RuntimeError> {
        Self::start_with(config, Startup::Initialize)
            .await?
            .shutdown()
            .await
    }

    pub async fn migrate(config: WorkspaceConfig) -> Result<(), RuntimeError> {
        tokio::task::spawn_blocking(move || {
            current_thread()?
                .block_on(Workspace::migrate(&config.notes_root, &config.state_root))
                .map_err(RuntimeError::Workspace)
        })
        .await
        .map_err(|_| RuntimeError::Worker)?
    }

    pub async fn adopt(config: WorkspaceConfig, name: String) -> Result<NodeId, RuntimeError> {
        tokio::task::spawn_blocking(move || {
            current_thread()?.block_on(async {
                let mut workspace = Workspace::open(&config.notes_root, &config.state_root)
                    .await
                    .map_err(RuntimeError::Workspace)?;
                let result = workspace
                    .adopt_root_document(&name)
                    .await
                    .map(|document| document.node_id)
                    .map_err(RuntimeError::Workspace);
                workspace.close().await.map_err(RuntimeError::Workspace)?;
                result
            })
        })
        .await
        .map_err(|_| RuntimeError::Worker)?
    }

    pub fn handle(&self) -> WorkspaceHandle {
        self.handle.clone()
    }

    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        self.begin_shutdown();
        self.handle.uploads.drain().await;
        self.stop_worker();
        let worker = self.worker.take().ok_or(RuntimeError::Closed)?;
        tokio::task::spawn_blocking(move || worker.join().map_err(|_| RuntimeError::Worker)?)
            .await
            .map_err(|_| RuntimeError::Worker)?
    }

    pub(crate) fn begin_shutdown(&mut self) {
        self.handle.stopping.store(true, Ordering::Release);
        self.handle.uploads.cancel();
    }

    async fn start_with(config: WorkspaceConfig, startup: Startup) -> Result<Self, RuntimeError> {
        let uploads = Arc::new(UploadState::new(config.uploads.max_receiving));
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (started, response) = oneshot::channel();
        let (stop, stopped) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("notryx-workspace".into())
            .spawn(move || {
                let runtime = match current_thread() {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ = started.send(Err(RuntimeError::Start));
                        return Err(RuntimeError::Start);
                    }
                };
                runtime.block_on(run_worker(config, startup, started, receiver, stopped))
            })
            .map_err(|_| RuntimeError::Start)?;
        let runtime = Self {
            handle: WorkspaceHandle {
                commands,
                stopping: Arc::new(AtomicBool::new(false)),
                uploads,
            },
            stop: Some(stop),
            worker: Some(worker),
        };
        match response.await {
            Ok(Ok(())) => Ok(runtime),
            Ok(Err(error)) => {
                let _ = runtime.shutdown().await;
                Err(error)
            }
            Err(_) => {
                let _ = runtime.shutdown().await;
                Err(RuntimeError::Worker)
            }
        }
    }

    fn stop_worker(&mut self) {
        self.handle.stopping.store(true, Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn current_thread() -> Result<tokio::runtime::Runtime, RuntimeError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| RuntimeError::Start)
}

impl Drop for WorkspaceRuntime {
    fn drop(&mut self) {
        // Handle clones must not retain workspace locks after the runtime owner is gone.
        self.handle.uploads.cancel();
        self.stop_worker();
    }
}

fn workspace_error(error: crate::Error) -> RuntimeError {
    match error {
        crate::Error::Auth(error) => RuntimeError::Auth(error),
        error => RuntimeError::Workspace(error),
    }
}
