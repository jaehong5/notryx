use crate::auth::AuthError;
use crate::config::WorkspaceConfig;
use crate::{Document, Workspace};
use notryx_domain::NodeId;
use notryx_protocol::{
    AdminRequest, AdminResult, ContentPrecondition, NodeMetadata, PairRequest, PairResponse,
    ServerInfo,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

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
            Self::PreconditionFailed => "content_revision_mismatch",
            Self::Workspace(crate::Error::MigrationRequired) => "migration_required",
            Self::Workspace(crate::Error::InvalidRequest) => "invalid_request",
            Self::Workspace(crate::Error::NotFound) => "not_found",
            Self::Workspace(crate::Error::Locked) => "locked",
            Self::Workspace(crate::Error::RecoveryRequired) => "recovery_required",
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
}

enum Command {
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
}

impl WorkspaceHandle {
    pub async fn server_info(&self, token: String) -> Result<ServerInfo, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::ServerInfo { token, reply })?;
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

async fn execute(command: Command, workspace: &mut Workspace, shutting_down: bool) {
    match command {
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
    }
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
        self.stop_worker();
        let worker = self.worker.take().ok_or(RuntimeError::Closed)?;
        tokio::task::spawn_blocking(move || worker.join().map_err(|_| RuntimeError::Worker)?)
            .await
            .map_err(|_| RuntimeError::Worker)?
    }

    pub(crate) fn begin_shutdown(&mut self) {
        self.stop_worker();
    }

    async fn start_with(config: WorkspaceConfig, startup: Startup) -> Result<Self, RuntimeError> {
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
        self.stop_worker();
    }
}
