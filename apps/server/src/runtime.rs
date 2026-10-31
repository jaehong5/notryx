use crate::auth::{AuthError, AuthIdentity};
use crate::config::{JournalRetentionConfig, WorkspaceConfig};
use crate::{
    AbandonOperation, AckOperations, CreateNode, DeleteNode, Document, MoveNode, OperationRecord,
    OperationStreamState, RestoreNode, SaveContent, Workspace,
};
use notryx_domain::{ChangeSeq, NodeId, OperationId};
use notryx_protocol::{
    AdminRecoverySummary, AdminRequest, AdminResult, ChangesPage, ContentPrecondition,
    DeviceCurrentRole, DeviceIdentity, DeviceRenewal, InventoryPage, JournalAccounting,
    NodeMetadata, NodeState, PairRequest, PairResponse, ServerInfo, TrashPage, WorkspaceRole,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot, watch};
use uuid::Uuid;

pub(crate) mod backup;
pub(crate) mod events;
#[cfg(test)]
mod journal_retention_changes_tests;
#[cfg(test)]
mod journal_retention_fairness_tests;
#[cfg(test)]
mod journal_retention_invalid_tests;
#[cfg(test)]
mod journal_retention_release_tests;
#[cfg(test)]
mod journal_retention_shutdown_tests;
#[cfg(test)]
pub(crate) mod journal_retention_test_support;
#[cfg(test)]
mod journal_retention_timer_tests;
#[cfg(test)]
mod journal_status_invalid_tests;
mod unsupported;
mod upload;
mod watcher;
#[cfg(test)]
type WorkerRetentionControl = Option<journal_retention_test_support::RetentionTestControl>;
#[cfg(not(test))]
type WorkerRetentionControl = ();
pub(crate) use upload::{CHUNK_BYTES, UploadSession, UploadState};

const COMMAND_CAPACITY: usize = 32;
const JOURNAL_MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeError {
    Start,
    Workspace(crate::Error),
    Auth(AuthError),
    PreconditionFailed,
    Closed,
    Busy,
    Worker,
    Backup(&'static str),
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
            Self::Workspace(crate::Error::ResyncRequired) => "resync_required",
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
            Self::Backup(code) => code,
            Self::Closed => "closed",
            Self::Busy => "busy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub ready: bool,
}

#[cfg(test)]
type TestSlot<T> = Arc<std::sync::Mutex<Option<T>>>;

#[cfg(test)]
type WatchRetryCleanupPause = (
    NodeId,
    oneshot::Sender<()>,
    oneshot::Receiver<()>,
    oneshot::Sender<bool>,
);

#[cfg(test)]
type DeletedFolderRegistrationPause = (
    NodeId,
    oneshot::Sender<()>,
    oneshot::Receiver<()>,
    oneshot::Sender<Option<tokio::time::Instant>>,
);

#[cfg(test)]
type ExternalFolderPause = (
    oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
    Option<oneshot::Sender<Result<crate::NodeSnapshot, RuntimeError>>>,
);

#[derive(Clone)]
pub struct WorkspaceHandle {
    commands: mpsc::Sender<Command>,
    // Published objects have no collector yet; any future collector must share this barrier.
    pub(crate) backup_permit: Arc<Semaphore>,
    stopping: Arc<AtomicBool>,
    uploads: Arc<UploadState>,
    notices: broadcast::Sender<()>,
    event_permits: Arc<events::EventPermits>,
    observation_permits: Arc<Semaphore>,
    observation_worker_failed: Arc<AtomicBool>,
    known_file_scan_incomplete: Arc<AtomicBool>,
    known_file_watch_degraded: Arc<AtomicBool>,
    watch_scan_registration_attempts:
        Arc<std::sync::Mutex<std::collections::HashMap<NodeId, tokio::time::Instant>>>,
    watch_scan_retry_interval: std::time::Duration,
    // Low bits track unreadable and unscanned watcher state; higher bits invalidate observations.
    known_file_watch_generation: Arc<std::sync::atomic::AtomicU64>,
    known_file_scan_retry: Arc<tokio::sync::Notify>,
    diagnostic_scan_retry: Arc<tokio::sync::Notify>,
    watch_folder_requests: mpsc::Sender<NodeId>,
    watch_folder_recovery: Arc<tokio::sync::Notify>,
    watch_folder_recovery_epoch: Arc<std::sync::atomic::AtomicU64>,
    watch_folder_recovery_completed: Arc<std::sync::atomic::AtomicU64>,
    watch_enabled: bool,
    unsupported_items: Arc<std::sync::Mutex<unsupported::State>>,
    scan_lock: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    known_file_scan_events: Arc<std::sync::Mutex<Vec<&'static str>>>,
    #[cfg(test)]
    known_file_watch_max_batch: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    known_file_watch_scan_waiting: Arc<AtomicBool>,
    #[cfg(test)]
    known_file_watch_scan_panic: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    known_file_watch_job_fault: Arc<std::sync::atomic::AtomicU8>,
    #[cfg(test)]
    watch_folder_failure: TestSlot<(NodeId, usize, usize)>,
    #[cfg(test)]
    watch_scan_retry_override: TestSlot<std::time::Duration>,
    #[cfg(test)]
    stale_watch_retry_cleanup_pause: TestSlot<WatchRetryCleanupPause>,
    #[cfg(test)]
    deleted_folder_registration_pause: TestSlot<DeletedFolderRegistrationPause>,
    #[cfg(test)]
    deleted_folder_registration_observation:
        TestSlot<(NodeId, oneshot::Sender<Option<tokio::time::Instant>>)>,
    #[cfg(test)]
    successful_folder_registration_pause:
        TestSlot<(NodeId, oneshot::Sender<i32>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    watch_self_event: TestSlot<(i32, oneshot::Sender<()>)>,
    #[cfg(test)]
    known_file_watch_max_pending: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    known_file_watch_lookup_pause: TestSlot<(
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
    )>,
    #[cfg(test)]
    known_file_watch_suppressed_name: TestSlot<String>,
    #[cfg(test)]
    known_file_watch_test_overflow: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    known_file_watch_test_read_error: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    known_file_watch_test_read_error_ack: TestSlot<(bool, oneshot::Sender<()>)>,
    #[cfg(test)]
    known_file_watch_injected_read_errors: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    known_file_watch_observed_read_errors: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    known_file_watch_read_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    known_file_scan_finish_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    diagnostic_scan_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    diagnostic_scan_starts: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    known_file_watch_recovered_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    known_file_watch_folder_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    known_file_missing_confirm_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    known_file_watch_recovery_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    known_file_watch_folder_jobs_done: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    confirm_missing_fault: TestSlot<(NodeId, oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    known_new_file_scan_max_buffer: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    known_new_file_scan_pause: TestSlot<(
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
    )>,
    #[cfg(test)]
    external_delete_pause: TestSlot<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    #[cfg(test)]
    external_folder_pause: TestSlot<ExternalFolderPause>,
    #[cfg(test)]
    external_folder_trace: TestSlot<Vec<(bool, Result<crate::NodeSnapshot, RuntimeError>)>>,
    #[cfg(test)]
    external_delete_commit_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    external_delete_transaction_pause: TestSlot<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    #[cfg(test)]
    watch_name_signal: TestSlot<(String, oneshot::Sender<()>)>,
    #[cfg(test)]
    test_save_enqueued: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalObjectRecheckStage {
    BeforeVerify,
    AfterVerify,
}

#[cfg(test)]
type RecheckHook = Arc<
    std::sync::Mutex<Box<dyn FnMut(ExternalObjectRecheckStage) -> Result<(), crate::Error> + Send>>,
>;
#[cfg(not(test))]
type RecheckHook = ();

enum RecheckReply {
    Existing(oneshot::Sender<Result<Option<Document>, RuntimeError>>),
    New(oneshot::Sender<Result<crate::NodeSnapshot, RuntimeError>>),
    Delete(oneshot::Sender<Result<(), RuntimeError>>),
}

impl RecheckReply {
    fn finish(self, error: crate::Error) {
        match self {
            Self::Existing(reply) => {
                let _ = reply.send(Err(RuntimeError::Workspace(error)));
            }
            Self::New(reply) => {
                let _ = reply.send(Err(RuntimeError::Workspace(error)));
            }
            Self::Delete(reply) => {
                let _ = reply.send(Err(RuntimeError::Workspace(error)));
            }
        }
    }
}

struct RecheckJob {
    hash: String,
    epoch: notryx_domain::WorkspaceEpoch,
    attempts: u8,
    preparation_error: Option<crate::Error>,
    reply: RecheckReply,
    _permit: OwnedSemaphorePermit,
    hook: Option<RecheckHook>,
    commands: mpsc::Sender<Command>,
}

enum Command {
    BackupSnapshot {
        target_state: std::fs::File,
        control: backup::Control,
        observation: String,
        reply: oneshot::Sender<Result<backup::Snapshot, RuntimeError>>,
    },
    WatchFolderPage {
        after: Option<NodeId>,
        reply: oneshot::Sender<Result<WatchFolderPage, RuntimeError>>,
    },
    WatchFolder {
        node_id: NodeId,
        reply: oneshot::Sender<Result<WatchFolder, RuntimeError>>,
    },
    WatchFolderDeleted {
        node_id: NodeId,
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
    },
    PruneWatchRetry {
        node_id: NodeId,
        attempted_at: tokio::time::Instant,
        attempts: Arc<std::sync::Mutex<std::collections::HashMap<NodeId, tokio::time::Instant>>>,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    WatchLookup {
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
        name: Option<String>,
        reply: oneshot::Sender<Result<Option<NodeId>, RuntimeError>>,
    },
    ScanKnownPage {
        after: Option<NodeId>,
        reply: oneshot::Sender<Result<KnownPage, RuntimeError>>,
    },
    ScanHead {
        reply: oneshot::Sender<Result<ChangeSeq, RuntimeError>>,
    },
    PrepareFolderScan {
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
        reply: oneshot::Sender<
            Result<(String, notryx_server_storage::ObservationReader), RuntimeError>,
        >,
    },
    PrepareExternal {
        node_id: NodeId,
        watch_parent: Option<(NodeId, notryx_server_storage::FolderIdentity)>,
        reply: oneshot::Sender<Result<crate::reconcile::ExternalProbe, RuntimeError>>,
    },
    CommitExternal {
        candidate: crate::reconcile::ExternalCandidate,
        reply: oneshot::Sender<Result<Option<Document>, RuntimeError>>,
        notices: broadcast::Sender<()>,
        stopping: Arc<AtomicBool>,
        _permit: OwnedSemaphorePermit,
        commands: mpsc::Sender<Command>,
    },
    PrepareExternalCreate {
        parent_id: NodeId,
        name: String,
        watch_parent: Option<notryx_server_storage::FolderIdentity>,
        reply: oneshot::Sender<Result<crate::reconcile::ExternalCreateProbe, RuntimeError>>,
    },
    PrepareExternalFolder {
        parent_id: NodeId,
        name: String,
        identity: notryx_server_storage::FolderIdentity,
        reply: oneshot::Sender<Result<Option<crate::reconcile::ExternalCreateProbe>, RuntimeError>>,
    },
    CommitExternalFolder {
        candidate: crate::reconcile::ExternalFolderCandidate,
        reply: oneshot::Sender<Result<Option<crate::NodeSnapshot>, RuntimeError>>,
        watch_handle: WorkspaceHandle,
        #[cfg(test)]
        original_done: Option<oneshot::Sender<Result<crate::NodeSnapshot, RuntimeError>>>,
        _permit: OwnedSemaphorePermit,
    },
    PrepareExternalDelete {
        node_id: NodeId,
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
        reply: oneshot::Sender<Result<crate::reconcile::ExternalDeleteProbe, RuntimeError>>,
    },
    CommitExternalDelete {
        candidate: crate::reconcile::ExternalDeleteCandidate,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
        notices: broadcast::Sender<()>,
        stopping: Arc<AtomicBool>,
        _permit: OwnedSemaphorePermit,
        commands: mpsc::Sender<Command>,
        watch_generation: Arc<std::sync::atomic::AtomicU64>,
        expected_watch_generation: u64,
        #[cfg(test)]
        commit_pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
        #[cfg(test)]
        transaction_pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    },
    CommitExternalCreate {
        candidate: crate::reconcile::ExternalCreateCandidate,
        reply: oneshot::Sender<Result<crate::NodeSnapshot, RuntimeError>>,
        notices: broadcast::Sender<()>,
        stopping: Arc<AtomicBool>,
        _permit: OwnedSemaphorePermit,
        commands: mpsc::Sender<Command>,
        recheck_hook: Option<RecheckHook>,
    },
    ReferencedObjectRechecked {
        job: RecheckJob,
        result: Result<notryx_server_storage::FileStamp, notryx_server_storage::StorageError>,
    },
    EventInfo {
        token: String,
        reply: oneshot::Sender<Result<(AuthIdentity, ServerInfo), RuntimeError>>,
    },
    Authenticate {
        token: String,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    #[cfg(test)]
    Barrier {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    #[cfg(test)]
    InstallExternalCommitPause {
        entered: oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        reply: oneshot::Sender<()>,
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
    Changes {
        token: String,
        after: ChangeSeq,
        through: Option<ChangeSeq>,
        reply: oneshot::Sender<Result<ChangesPage, RuntimeError>>,
    },
    CreateNode {
        token: String,
        request: CreateNode,
        watch_handle: WorkspaceHandle,
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
    RenewDevice {
        token: String,
        reply: oneshot::Sender<Result<DeviceRenewal, RuntimeError>>,
    },
    ServerInfo {
        token: String,
        reply: oneshot::Sender<Result<ServerInfo, RuntimeError>>,
    },
    DeviceIdentity {
        token: String,
        reply: oneshot::Sender<Result<DeviceIdentity, RuntimeError>>,
    },
    DeviceCurrentRole {
        token: String,
        reply: oneshot::Sender<Result<DeviceCurrentRole, RuntimeError>>,
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
    pub fn observation_worker_failed(&self) -> bool {
        self.observation_worker_failed.load(Ordering::Acquire)
    }

    pub fn known_file_scan_incomplete(&self) -> bool {
        self.known_file_scan_incomplete.load(Ordering::Acquire)
            || self.known_file_watch_generation.load(Ordering::Acquire) & 3 != 0
            || self.watch_folder_recovery_epoch.load(Ordering::Acquire)
                != self.watch_folder_recovery_completed.load(Ordering::Acquire)
    }

    pub fn known_file_watch_degraded(&self) -> bool {
        self.known_file_watch_degraded.load(Ordering::Acquire)
    }

    fn mark_known_file_watch_degraded(&self) {
        if !self.known_file_watch_degraded.swap(true, Ordering::AcqRel) {
            self.log_known_file_scan_event("known_file_watch_degraded");
        }
    }

    fn mark_known_file_watch_read_error(&self) {
        let mut current = self.known_file_watch_generation.load(Ordering::Acquire);
        loop {
            let next = current.wrapping_add(4) | 3;
            match self.known_file_watch_generation.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        self.known_file_scan_incomplete
            .store(true, Ordering::Release);
        self.mark_known_file_watch_degraded();
    }

    fn mark_known_file_watch_read_recovered(&self) {
        if self
            .known_file_watch_generation
            .fetch_and(!1, Ordering::AcqRel)
            & 1
            != 0
        {
            self.known_file_scan_retry.notify_one();
        }
    }

    fn mark_watch_folder_registration_problem(&self, event: &'static str) {
        let mut current = self.known_file_watch_generation.load(Ordering::Acquire);
        loop {
            let next = current.wrapping_add(4) | 2;
            match self.known_file_watch_generation.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        self.known_file_scan_incomplete
            .store(true, Ordering::Release);
        self.mark_known_file_watch_degraded();
        self.log_known_file_scan_event(event);
    }

    fn request_watch_folder(&self, node_id: NodeId) {
        if self.watch_enabled {
            self.known_file_watch_generation
                .fetch_add(4, Ordering::AcqRel);
            self.known_file_scan_incomplete
                .store(true, Ordering::Release);
        }
        if let Err(error) = self.watch_folder_requests.try_send(node_id) {
            match error {
                mpsc::error::TrySendError::Full(_) => {
                    self.watch_folder_recovery_epoch
                        .fetch_add(1, Ordering::AcqRel);
                    self.mark_watch_folder_registration_problem(
                        "known_file_watch_registration_overflow",
                    );
                    self.watch_folder_recovery.notify_one();
                }
                mpsc::error::TrySendError::Closed(_) if self.watch_enabled => {
                    self.mark_watch_folder_registration_problem(
                        "known_file_watch_registration_closed",
                    );
                }
                mpsc::error::TrySendError::Closed(_) => {}
            }
        }
    }

    fn request_watch_folder_from_scan(&self, node_id: NodeId) {
        let Ok(mut attempts) = self.watch_scan_registration_attempts.lock() else {
            self.mark_watch_folder_registration_problem("known_file_watch_registration_failed");
            return;
        };
        let now = tokio::time::Instant::now();
        if attempts
            .get(&node_id)
            .is_some_and(|previous| now.duration_since(*previous) < self.watch_scan_retry_delay())
            || (!attempts.contains_key(&node_id) && attempts.len() >= 10_000)
        {
            return;
        }
        attempts.insert(node_id, now);
        drop(attempts);
        self.request_watch_folder(node_id);
    }

    fn watch_scan_retry_delay(&self) -> std::time::Duration {
        #[cfg(test)]
        if let Some(delay) = *self.watch_scan_retry_override.lock().unwrap() {
            return delay;
        }
        self.watch_scan_retry_interval
    }

    async fn watch_folder_page(
        &self,
        after: Option<NodeId>,
    ) -> Result<WatchFolderPage, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::WatchFolderPage { after, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn watch_folder(&self, node_id: NodeId) -> Result<WatchFolder, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::WatchFolder { node_id, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn watch_folder_deleted(&self, node_id: NodeId) -> Result<bool, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::WatchFolderDeleted { node_id, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn prune_watch_retry(
        &self,
        node_id: NodeId,
        attempted_at: tokio::time::Instant,
    ) -> Result<(), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::PruneWatchRetry {
            node_id,
            attempted_at,
            attempts: self.watch_scan_registration_attempts.clone(),
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn watch_lookup(
        &self,
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
        name: Option<String>,
    ) -> Result<Option<NodeId>, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::WatchLookup {
            parent_id,
            identity,
            name,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    #[cfg(test)]
    pub(crate) fn test_known_file_scan_events(&self) -> Vec<&'static str> {
        self.known_file_scan_events.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub(crate) fn test_reset_known_file_watch_max_batch(&self) {
        self.known_file_watch_max_batch.store(0, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_known_file_watch_max_batch(&self) -> usize {
        self.known_file_watch_max_batch.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) async fn test_lock_known_file_scan(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.scan_lock.clone().lock_owned().await
    }

    #[cfg(test)]
    pub(crate) fn test_known_file_watch_scan_waiting(&self) -> bool {
        self.known_file_watch_scan_waiting.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_panic_next_watch_scan(&self) {
        self.test_panic_watch_scans(1);
    }

    #[cfg(test)]
    pub(crate) fn test_panic_watch_scans(&self, count: usize) {
        self.known_file_watch_scan_panic
            .store(count, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_watch_scan_panics_remaining(&self) -> usize {
        self.known_file_watch_scan_panic.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_watch_job_fault(&self, fault: u8) {
        self.known_file_watch_job_fault
            .store(fault, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_fail_watch_folder(&self, node_id: NodeId, count: usize) {
        *self.watch_folder_failure.lock().unwrap() = Some((node_id, count, 0));
    }

    #[cfg(test)]
    pub(crate) fn test_watch_folder_failures(&self) -> usize {
        self.watch_folder_failure
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |entry| entry.2)
    }

    #[cfg(test)]
    pub(crate) fn test_watch_scan_pending(&self, node_id: NodeId) -> bool {
        self.watch_scan_registration_attempts
            .lock()
            .unwrap()
            .contains_key(&node_id)
    }

    #[cfg(test)]
    pub(crate) fn test_set_watch_scan_retry_delay(&self, delay: std::time::Duration) {
        *self.watch_scan_retry_override.lock().unwrap() = Some(delay);
    }

    #[cfg(test)]
    pub(crate) fn test_request_watch_folder_from_scan(&self, node_id: NodeId) {
        self.request_watch_folder_from_scan(node_id);
    }

    #[cfg(test)]
    pub(crate) fn test_watch_scan_pending_at(
        &self,
        node_id: NodeId,
    ) -> Option<tokio::time::Instant> {
        self.watch_scan_registration_attempts
            .lock()
            .unwrap()
            .get(&node_id)
            .copied()
    }

    #[cfg(test)]
    pub(crate) fn test_pause_deleted_folder_registration_result(
        &self,
        node_id: NodeId,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<Option<tokio::time::Instant>>,
    ) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        let (observed, observation) = oneshot::channel();
        *self.deleted_folder_registration_pause.lock().unwrap() =
            Some((node_id, entered, release, observed));
        (receiver, sender, observation)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_successful_folder_registration(
        &self,
        node_id: NodeId,
    ) -> (oneshot::Receiver<i32>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.successful_folder_registration_pause.lock().unwrap() =
            Some((node_id, entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_watch_self_event(&self, wd: i32) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        *self.watch_self_event.lock().unwrap() = Some((wd, sender));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn test_pause_stale_watch_retry_cleanup(
        &self,
        node_id: NodeId,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<bool>,
    ) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        let (observed, observation) = oneshot::channel();
        *self.stale_watch_retry_cleanup_pause.lock().unwrap() =
            Some((node_id, entered, release, observed));
        (receiver, sender, observation)
    }

    #[cfg(test)]
    fn test_take_watch_folder_failure(&self, node_id: NodeId) -> bool {
        let mut failure = self.watch_folder_failure.lock().unwrap();
        let Some((target, remaining, attempts)) = failure.as_mut() else {
            return false;
        };
        if *target != node_id || *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        *attempts += 1;
        true
    }

    #[cfg(test)]
    pub(crate) fn test_watch_max_pending(&self) -> usize {
        self.known_file_watch_max_pending.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_watch_lookup(
        &self,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (entered, entered_rx) = oneshot::channel();
        let (release_tx, release) = oneshot::channel();
        let (done, done_rx) = oneshot::channel();
        *self.known_file_watch_lookup_pause.lock().unwrap() = Some((entered, release, done));
        (entered_rx, release_tx, done_rx)
    }

    #[cfg(test)]
    pub(crate) fn test_suppress_watch_name(&self, name: &str) {
        *self.known_file_watch_suppressed_name.lock().unwrap() = Some(name.into());
    }

    #[cfg(test)]
    pub(crate) fn test_request_watcher_overflow_scan(&self) {
        self.known_file_watch_test_overflow.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn test_request_diagnostic_scan(&self) {
        self.diagnostic_scan_retry.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn test_diagnostic_scan_starts(&self) -> usize {
        self.diagnostic_scan_starts.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_watch_read_error(&self, readable_error: bool) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        *self.known_file_watch_test_read_error_ack.lock().unwrap() = Some((readable_error, sender));
        self.known_file_watch_test_read_error.notify_one();
        receiver
    }

    #[cfg(test)]
    pub(crate) fn test_watch_inject_read_errors(&self, count: usize) {
        self.known_file_watch_injected_read_errors
            .store(count, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_watch_observed_read_errors(&self) -> usize {
        self.known_file_watch_observed_read_errors
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_watch_generation(&self) -> u64 {
        self.known_file_watch_generation.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_scan_finish(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.known_file_scan_finish_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_diagnostic_scan(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.diagnostic_scan_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_watch_recovered(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.known_file_watch_recovered_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_watch_folder(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.known_file_watch_folder_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_missing_confirmation(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.known_file_missing_confirm_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_watch_recovery(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.known_file_watch_recovery_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_watch_folder_jobs_done(&self) -> usize {
        self.known_file_watch_folder_jobs_done
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_fill_watch_folder_queue(&self, node_id: NodeId) -> usize {
        let mut count = 0;
        while self.watch_folder_requests.try_send(node_id).is_ok() {
            count += 1;
        }
        count
    }

    #[cfg(test)]
    pub(crate) fn test_watch_folder_queue_closed(&self) -> bool {
        self.watch_folder_requests.is_closed()
    }

    #[cfg(test)]
    pub(crate) fn test_pause_next_watch_read(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.known_file_watch_read_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_confirm_missing_fault(
        &self,
        node_id: NodeId,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.confirm_missing_fault.lock().unwrap() = Some((node_id, entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_new_file_scan_max_buffer(&self) -> usize {
        self.known_new_file_scan_max_buffer.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_new_file_scan(
        &self,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (entered, entered_rx) = oneshot::channel();
        let (release_tx, release) = oneshot::channel();
        let (done, done_rx) = oneshot::channel();
        *self.known_new_file_scan_pause.lock().unwrap() = Some((entered, release, done));
        (entered_rx, release_tx, done_rx)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_external_delete(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *self.external_delete_pause.lock().unwrap() = Some((entered, release_rx));
        (entered_rx, release)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_external_folder(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = std::sync::mpsc::channel();
        *self.external_folder_pause.lock().unwrap() = Some((entered, release, None));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_trace_external_folder(
        &self,
    ) -> (
        oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        oneshot::Receiver<Result<crate::NodeSnapshot, RuntimeError>>,
    ) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = std::sync::mpsc::channel();
        let (done, done_receiver) = oneshot::channel();
        *self.external_folder_trace.lock().unwrap() = Some(Vec::new());
        *self.external_folder_pause.lock().unwrap() = Some((entered, release, Some(done)));
        (receiver, sender, done_receiver)
    }

    #[cfg(test)]
    pub(crate) fn test_external_folder_trace(
        &self,
    ) -> Vec<(bool, Result<crate::NodeSnapshot, RuntimeError>)> {
        self.external_folder_trace
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn test_pause_external_delete_commit(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.external_delete_commit_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_pause_external_delete_transaction(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, receiver) = oneshot::channel();
        let (sender, release) = oneshot::channel();
        *self.external_delete_transaction_pause.lock().unwrap() = Some((entered, release));
        (receiver, sender)
    }

    #[cfg(test)]
    pub(crate) fn test_signal_watch_name(&self, name: &str) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        *self.watch_name_signal.lock().unwrap() = Some((name.into(), sender));
        receiver
    }

    #[cfg(test)]
    pub(crate) async fn test_watch_registration_with_pause(
        &self,
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    ) -> Result<usize, RuntimeError> {
        watcher::test_register_with_pause(self.clone(), entered, release).await
    }

    fn log_known_file_scan_event(&self, event: &'static str) {
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "{event}");
        #[cfg(test)]
        if (event.starts_with("known_file_scan_")
            || event.starts_with("known_file_watch_registration_"))
            && let Ok(mut events) = self.known_file_scan_events.lock()
        {
            events.push(event);
        }
    }

    async fn scan_known_page(&self, after: Option<NodeId>) -> Result<KnownPage, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::ScanKnownPage { after, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn scan_head(&self) -> Result<ChangeSeq, RuntimeError> {
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(RuntimeError::Closed);
            }
            let (reply, response) = oneshot::channel();
            match self.enqueue(Command::ScanHead { reply }) {
                Ok(()) => return response.await.map_err(|_| RuntimeError::Closed)?,
                Err(RuntimeError::Busy) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn prepare_folder_scan(
        &self,
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
    ) -> Result<(String, notryx_server_storage::ObservationReader), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::PrepareFolderScan {
            parent_id,
            identity,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn scan_new_markdown_in_folder(
        &self,
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
        issues: &mut unsupported::Collector,
    ) -> Result<bool, RuntimeError> {
        let (path, reader) = loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(RuntimeError::Closed);
            }
            match self.prepare_folder_scan(parent_id, identity).await {
                Ok(value) => break value,
                Err(RuntimeError::Busy) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                Err(error) => return Err(error),
            }
        };
        let parent_path = path.clone();
        let (sender, mut names) = tokio::sync::mpsc::channel::<ScanChild>(8);
        let stopping = self.stopping.clone();
        #[cfg(test)]
        let max_buffer = self.known_new_file_scan_max_buffer.clone();
        let worker = tokio::task::spawn_blocking(move || {
            reader.stream_watch_children(&path, identity, |child| {
                if stopping.load(Ordering::Acquire) {
                    return Err(notryx_server_storage::StorageError::Changed);
                }
                let entry = match child {
                    notryx_server_storage::WatchChild::Candidate { name, is_directory }
                        if !watcher::excluded(name)
                            && (is_directory || name.to_ascii_lowercase().ends_with(".md")) =>
                    {
                        ScanChild::Candidate(name.to_owned(), is_directory)
                    }
                    notryx_server_storage::WatchChild::Candidate { .. } => return Ok(()),
                    notryx_server_storage::WatchChild::Unsupported { name, reason } => {
                        ScanChild::Unsupported(name.to_vec(), reason)
                    }
                };
                sender
                    .blocking_send(entry)
                    .map_err(|_| notryx_server_storage::StorageError::Changed)?;
                #[cfg(test)]
                max_buffer.fetch_max(8 - sender.capacity(), Ordering::AcqRel);
                Ok(())
            })
        });
        let mut incomplete = false;
        while let Some(entry) = names.recv().await {
            if self.stopping.load(Ordering::Acquire) {
                drop(names);
                let _ = worker.await;
                return Err(RuntimeError::Closed);
            }
            let (name, is_directory) = match entry {
                ScanChild::Candidate(name, is_directory) => (name, is_directory),
                ScanChild::Unsupported(name, reason) => {
                    use notryx_protocol::admin::UnsupportedItemReason as Reason;
                    let reason = match reason {
                        notryx_server_storage::WatchUnsupportedReason::Symlink => Reason::Symlink,
                        notryx_server_storage::WatchUnsupportedReason::HardLink => Reason::HardLink,
                        notryx_server_storage::WatchUnsupportedReason::SpecialFile => {
                            Reason::SpecialFile
                        }
                        notryx_server_storage::WatchUnsupportedReason::InvalidName => {
                            Reason::InvalidName
                        }
                    };
                    issues.add(&parent_path, &name, reason);
                    continue;
                }
            };
            let mut failures = 0;
            loop {
                if self.stopping.load(Ordering::Acquire) {
                    drop(names);
                    let _ = worker.await;
                    return Err(RuntimeError::Closed);
                }
                let result = if is_directory {
                    match self
                        .reconcile_watched_folder(parent_id, name.clone(), identity)
                        .await
                    {
                        Ok(Some(_)) => {
                            incomplete = true;
                            Ok(())
                        }
                        Ok(None) | Err(RuntimeError::Workspace(crate::Error::NotFound)) => Ok(()),
                        Err(error) => Err(error),
                    }
                } else {
                    match self
                        .watch_lookup(parent_id, identity, Some(name.clone()))
                        .await
                    {
                        Ok(Some(_)) => Ok(()),
                        Ok(None) => {
                            #[cfg(test)]
                            let done = {
                                let pause = self.known_new_file_scan_pause.lock().unwrap().take();
                                if let Some((entered, release, done)) = pause {
                                    let _ = entered.send(());
                                    let _ = release.await;
                                    Some(done)
                                } else {
                                    None
                                }
                            };
                            let result = match self
                                .reconcile_watched_new_file(parent_id, name.clone(), identity)
                                .await
                            {
                                Ok(_) | Err(RuntimeError::Workspace(crate::Error::NotFound)) => {
                                    Ok(())
                                }
                                Err(error) => Err(error),
                            };
                            #[cfg(test)]
                            if let Some(done) = done {
                                let _ = done.send(());
                            }
                            result
                        }
                        Err(error) => Err(error),
                    }
                };
                match result {
                    Ok(()) => break,
                    Err(RuntimeError::Closed) => {
                        drop(names);
                        let _ = worker.await;
                        return Err(RuntimeError::Closed);
                    }
                    Err(RuntimeError::Busy) => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await
                    }
                    Err(RuntimeError::Workspace(crate::Error::ExternalChange)) if failures < 2 => {
                        failures += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(_) => {
                        incomplete = true;
                        break;
                    }
                }
            }
            tokio::task::yield_now().await;
        }
        match worker.await {
            Ok(Ok(scan)) if !incomplete && scan.coverage_complete => Ok(scan.has_unsupported),
            _ => Err(RuntimeError::Workspace(crate::Error::ExternalChange)),
        }
    }

    async fn confirm_single_missing(
        &self,
        folders: &std::collections::HashMap<NodeId, notryx_server_storage::FolderIdentity>,
        candidate: NodeId,
        candidate_is_folder: bool,
    ) -> Result<bool, RuntimeError> {
        let mut after = None;
        let mut found = false;
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(RuntimeError::Closed);
            }
            let page = match self.scan_known_page(after).await {
                Ok(page) => page,
                Err(RuntimeError::Busy) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            for (node_id, _) in &page.folders {
                if let Some(identity) = folders.get(node_id) {
                    if candidate_is_folder && *node_id == candidate {
                        return Ok(false);
                    }
                    loop {
                        if self.stopping.load(Ordering::Acquire) {
                            return Err(RuntimeError::Closed);
                        }
                        match self.watch_lookup(*node_id, *identity, None).await {
                            Ok(_) => break,
                            Err(RuntimeError::Busy) => {
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await
                            }
                            Err(error) => return Err(error),
                        }
                    }
                } else {
                    if candidate_is_folder && *node_id == candidate {
                        found = true;
                    } else {
                        return Ok(false);
                    }
                }
            }
            for (node_id, parent_id) in page.files {
                #[cfg(test)]
                {
                    let fault = {
                        let mut fault = self.confirm_missing_fault.lock().unwrap();
                        if fault
                            .as_ref()
                            .is_some_and(|(target, _, _)| *target == node_id)
                        {
                            fault.take()
                        } else {
                            None
                        }
                    };
                    if let Some((_, entered, release)) = fault {
                        let _ = entered.send(());
                        let _ = release.await;
                        return Err(RuntimeError::Workspace(crate::Error::ExternalChange));
                    }
                }
                let parent =
                    parent_id.ok_or(RuntimeError::Workspace(crate::Error::ExternalChange))?;
                let identity = *folders
                    .get(&parent)
                    .ok_or(RuntimeError::Workspace(crate::Error::ExternalChange))?;
                let result = loop {
                    if self.stopping.load(Ordering::Acquire) {
                        return Err(RuntimeError::Closed);
                    }
                    match self.reconcile_watched_file(node_id, parent, identity).await {
                        Err(RuntimeError::Busy) => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await
                        }
                        other => break other,
                    }
                };
                match result {
                    Err(RuntimeError::Workspace(crate::Error::NotFound))
                        if !candidate_is_folder && node_id == candidate =>
                    {
                        found = true
                    }
                    Err(RuntimeError::Workspace(crate::Error::NotFound)) => return Ok(false),
                    Ok(_) if !candidate_is_folder && node_id == candidate => return Ok(false),
                    Ok(_) => {}
                    Err(error) => return Err(error),
                }
                tokio::task::yield_now().await;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(found),
            }
        }
    }

    async fn scan_known_files(
        &self,
        watch_folders: Option<
            Arc<std::collections::HashMap<NodeId, notryx_server_storage::FolderIdentity>>,
        >,
        diagnostic_only: bool,
    ) {
        let _scan = self.scan_lock.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let scan_guard = unsupported::ScanGuard::begin(self.unsupported_items.clone());
        let mut issues = unsupported::Collector::default();
        let head_start = match self.scan_head().await {
            Ok(head) => head,
            Err(_) => {
                self.known_file_scan_incomplete
                    .store(true, Ordering::Release);
                self.log_known_file_scan_event(if diagnostic_only {
                    "unsupported_items_scan_incomplete"
                } else {
                    "known_file_scan_incomplete"
                });
                return;
            }
        };
        let scan_start_generation = self.known_file_watch_generation.load(Ordering::Acquire);
        let scan_folder_recovery_epoch = self.watch_folder_recovery_epoch.load(Ordering::Acquire);
        let scan_folder_recovery_completed =
            self.watch_folder_recovery_completed.load(Ordering::Acquire);
        if !diagnostic_only {
            self.known_file_scan_incomplete
                .store(true, Ordering::Release);
        }
        #[cfg(test)]
        if diagnostic_only {
            self.diagnostic_scan_starts.fetch_add(1, Ordering::AcqRel);
        }
        self.log_known_file_scan_event(if diagnostic_only {
            "unsupported_items_scan_started"
        } else {
            "known_file_scan_started"
        });
        let stopped_event = if diagnostic_only {
            "unsupported_items_scan_stopped"
        } else {
            "known_file_scan_stopped"
        };
        let incomplete_event = if diagnostic_only {
            "unsupported_items_scan_incomplete"
        } else {
            "known_file_scan_incomplete"
        };
        let mut after = None;
        let mut incomplete = false;
        let mut stale_retry_nodes = match self.watch_scan_registration_attempts.lock() {
            Ok(attempts) => attempts.clone(),
            Err(_) => {
                incomplete = true;
                std::collections::HashMap::new()
            }
        };
        let mut coverage_complete = watch_folders.is_some();
        let mut folder_count = 0usize;
        let mut missing = None;
        let mut multiple_missing = false;
        loop {
            if self.stopping.load(Ordering::Acquire) {
                self.log_known_file_scan_event(stopped_event);
                return;
            }
            let page = match self.scan_known_page(after).await {
                Ok(page) => page,
                Err(RuntimeError::Busy) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
                Err(RuntimeError::Closed) if self.stopping.load(Ordering::Acquire) => {
                    self.log_known_file_scan_event(stopped_event);
                    return;
                }
                Err(_) => {
                    self.known_file_scan_incomplete
                        .store(true, Ordering::Release);
                    self.log_known_file_scan_event(incomplete_event);
                    return;
                }
            };
            for (folder_id, parent_id) in &page.folders {
                stale_retry_nodes.remove(folder_id);
                folder_count += 1;
                if let Some(folders) = &watch_folders
                    && !folders.contains_key(folder_id)
                {
                    coverage_complete = false;
                    let Some((parent, identity)) = parent_id.and_then(|parent| {
                        folders.get(&parent).map(|identity| (parent, *identity))
                    }) else {
                        incomplete = true;
                        continue;
                    };
                    let observed = loop {
                        match self
                            .observe_missing_folder(*folder_id, parent, identity)
                            .await
                        {
                            Err(RuntimeError::Busy) if !self.stopping.load(Ordering::Acquire) => {
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await
                            }
                            result => break result,
                        }
                    };
                    match observed {
                        Ok(true) if missing.is_some() => multiple_missing = true,
                        Ok(true) => missing = Some((*folder_id, parent, identity, true)),
                        Ok(false) => {
                            self.request_watch_folder_from_scan(*folder_id);
                            incomplete = true;
                        }
                        Err(_) => incomplete = true,
                    }
                }
            }
            for (node_id, parent_id) in page.files {
                let mut failures = 0;
                loop {
                    if self.stopping.load(Ordering::Acquire) {
                        self.log_known_file_scan_event(stopped_event);
                        return;
                    }
                    let result = if let Some(folders) = &watch_folders {
                        match parent_id.and_then(|parent| {
                            folders.get(&parent).map(|identity| (parent, *identity))
                        }) {
                            Some((parent, identity)) => {
                                self.reconcile_watched_file(node_id, parent, identity).await
                            }
                            None => {
                                incomplete = true;
                                break;
                            }
                        }
                    } else {
                        self.reconcile_file(node_id).await
                    };
                    match result {
                        Ok(_) => break,
                        Err(RuntimeError::Workspace(crate::Error::NotFound))
                            if watch_folders.is_some() =>
                        {
                            if let Some((parent, identity)) = parent_id.and_then(|parent| {
                                watch_folders.as_ref().and_then(|folders| {
                                    folders.get(&parent).map(|identity| (parent, *identity))
                                })
                            }) {
                                if missing.is_some() {
                                    multiple_missing = true;
                                } else {
                                    missing = Some((node_id, parent, identity, false));
                                }
                            } else {
                                incomplete = true;
                            }
                            break;
                        }
                        Err(RuntimeError::Closed) => {
                            self.known_file_scan_incomplete
                                .store(true, Ordering::Release);
                            self.log_known_file_scan_event(
                                if self.stopping.load(Ordering::Acquire) {
                                    stopped_event
                                } else {
                                    incomplete_event
                                },
                            );
                            return;
                        }
                        Err(RuntimeError::Busy) => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(RuntimeError::Workspace(crate::Error::ExternalChange))
                            if failures < 2 =>
                        {
                            failures += 1;
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(_) => {
                            incomplete = true;
                            break;
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        #[cfg(test)]
        let cleanup_observation = {
            let pause = {
                let mut slot = self.stale_watch_retry_cleanup_pause.lock().unwrap();
                if slot
                    .as_ref()
                    .is_some_and(|(node_id, _, _, _)| stale_retry_nodes.contains_key(node_id))
                {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some((node_id, entered, release, observed)) = pause {
                let _ = entered.send(());
                let _ = release.await;
                Some((node_id, observed))
            } else {
                None
            }
        };
        for (node_id, attempted_at) in stale_retry_nodes {
            if self.prune_watch_retry(node_id, attempted_at).await.is_err() {
                incomplete = true;
            }
        }
        #[cfg(test)]
        if let Some((node_id, observed)) = cleanup_observation {
            let pending = self
                .watch_scan_registration_attempts
                .lock()
                .unwrap()
                .contains_key(&node_id);
            let _ = observed.send(pending);
        }
        if multiple_missing {
            incomplete = true;
        }
        if let (Some((node_id, parent_id, identity, is_folder)), Some(folders)) =
            (missing, &watch_folders)
        {
            if scan_folder_recovery_epoch != scan_folder_recovery_completed
                || self.watch_folder_recovery_epoch.load(Ordering::Acquire)
                    != scan_folder_recovery_epoch
                || self.watch_folder_recovery_completed.load(Ordering::Acquire)
                    != scan_folder_recovery_completed
            {
                incomplete = true;
            }
            if !incomplete {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                #[cfg(test)]
                {
                    let pause = self.known_file_missing_confirm_pause.lock().unwrap().take();
                    if let Some((entered, release)) = pause {
                        let _ = entered.send(());
                        let _ = release.await;
                    }
                }
                let generation = self.known_file_watch_generation.load(Ordering::Acquire);
                if self.watch_folder_recovery_epoch.load(Ordering::Acquire)
                    != scan_folder_recovery_epoch
                    || self.watch_folder_recovery_completed.load(Ordering::Acquire)
                        != scan_folder_recovery_completed
                {
                    incomplete = true;
                }
                if incomplete {
                    self.known_file_scan_incomplete
                        .store(true, Ordering::Release);
                    self.log_known_file_scan_event(incomplete_event);
                    return;
                }
                match self
                    .confirm_single_missing(folders, node_id, is_folder)
                    .await
                {
                    Ok(true)
                        if self.known_file_watch_generation.load(Ordering::Acquire)
                            == generation =>
                    {
                        match self
                            .reconcile_watched_delete(node_id, parent_id, identity, generation)
                            .await
                        {
                            Ok(()) => {
                                if is_folder {
                                    self.known_file_scan_retry.notify_one();
                                }
                            }
                            Err(RuntimeError::Closed) if self.stopping.load(Ordering::Acquire) => {
                                self.log_known_file_scan_event(stopped_event);
                                return;
                            }
                            Err(_) => {
                                incomplete = true;
                                if self.known_file_watch_generation.load(Ordering::Acquire)
                                    != generation
                                {
                                    self.known_file_scan_retry.notify_one();
                                }
                            }
                        }
                    }
                    Ok(_) => {
                        incomplete = true;
                        if self.known_file_watch_generation.load(Ordering::Acquire) != generation {
                            self.known_file_scan_retry.notify_one();
                        }
                    }
                    Err(RuntimeError::Closed) if self.stopping.load(Ordering::Acquire) => {
                        self.log_known_file_scan_event(stopped_event);
                        return;
                    }
                    Err(_) => {
                        incomplete = true;
                        if self.known_file_watch_generation.load(Ordering::Acquire) != generation {
                            self.known_file_scan_retry.notify_one();
                        }
                    }
                }
            }
        }
        coverage_complete &= !incomplete;
        if let Some(folders) = &watch_folders {
            if folder_count != folders.len() {
                coverage_complete = false;
                incomplete = true;
            }
            for (&parent_id, &identity) in folders.iter() {
                match self
                    .scan_new_markdown_in_folder(parent_id, identity, &mut issues)
                    .await
                {
                    Ok(has_unsupported) => {
                        incomplete |= has_unsupported;
                    }
                    Err(RuntimeError::Closed) if self.stopping.load(Ordering::Acquire) => {
                        self.log_known_file_scan_event(stopped_event);
                        return;
                    }
                    Err(_) => {
                        incomplete = true;
                        coverage_complete = false;
                    }
                }
                tokio::task::yield_now().await;
            }
        }
        let current_generation = self.known_file_watch_generation.load(Ordering::Acquire);
        let head_end = self.scan_head().await.ok();
        if head_end != Some(head_start)
            || current_generation >> 2 != scan_start_generation >> 2
            || self.watch_folder_recovery_epoch.load(Ordering::Acquire)
                != scan_folder_recovery_epoch
            || self.watch_folder_recovery_completed.load(Ordering::Acquire)
                != scan_folder_recovery_completed
            || scan_folder_recovery_epoch != scan_folder_recovery_completed
        {
            coverage_complete = false;
            self.diagnostic_scan_retry.notify_one();
        }
        if current_generation & 1 != 0 {
            coverage_complete = false;
        }
        #[cfg(test)]
        if diagnostic_only {
            let pause = self.diagnostic_scan_pause.lock().unwrap().take();
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.await;
            }
        }
        if self.known_file_watch_generation.load(Ordering::Acquire) >> 2
            != scan_start_generation >> 2
        {
            incomplete = true;
        }
        scan_guard.finish(issues, coverage_complete, current_generation);
        if coverage_complete
            && scan_start_generation & 1 == 0
            && scan_start_generation & 2 != 0
            && self.watch_folder_recovery_epoch.load(Ordering::Acquire)
                == self.watch_folder_recovery_completed.load(Ordering::Acquire)
            && let Err(current) = self.known_file_watch_generation.compare_exchange(
                scan_start_generation,
                scan_start_generation & !2,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            && current & 2 != 0
            && current & 1 == 0
        {
            self.known_file_scan_retry.notify_one();
        }
        incomplete |= self.known_file_watch_generation.load(Ordering::Acquire) & 3 != 0;
        #[cfg(test)]
        {
            let pause = self.known_file_scan_finish_pause.lock().unwrap().take();
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.await;
            }
        }
        self.known_file_scan_incomplete
            .store(incomplete, Ordering::Release);
        self.log_known_file_scan_event(if incomplete {
            incomplete_event
        } else if diagnostic_only {
            "unsupported_items_scan_complete"
        } else {
            "known_file_scan_complete"
        });
    }
    #[cfg(test)]
    pub(crate) fn test_observation_active(&self) -> usize {
        2 - self.observation_permits.available_permits()
    }

    #[cfg(test)]
    pub(crate) async fn test_external_commit_pause(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, response) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let (reply, installed) = oneshot::channel();
        self.enqueue(Command::InstallExternalCommitPause {
            entered,
            release: held,
            reply,
        })
        .unwrap();
        installed.await.unwrap();
        (response, release)
    }
    pub async fn reconcile_file(&self, node_id: NodeId) -> Result<Option<Document>, RuntimeError> {
        self.reconcile_with(node_id, None, |probe| probe.observe())
            .await
    }

    async fn reconcile_watched_file(
        &self,
        node_id: NodeId,
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
    ) -> Result<Option<Document>, RuntimeError> {
        self.reconcile_with(node_id, Some((parent_id, identity)), |probe| {
            probe.observe()
        })
        .await
    }

    pub async fn reconcile_new_file(
        &self,
        parent_id: NodeId,
        name: String,
    ) -> Result<crate::NodeSnapshot, RuntimeError> {
        self.reconcile_new_with(parent_id, name, |probe| probe.observe(), None, None)
            .await
    }

    async fn reconcile_watched_new_file(
        &self,
        parent_id: NodeId,
        name: String,
        identity: notryx_server_storage::FolderIdentity,
    ) -> Result<crate::NodeSnapshot, RuntimeError> {
        self.reconcile_new_with(
            parent_id,
            name,
            |probe| probe.observe(),
            None,
            Some(identity),
        )
        .await
    }

    async fn reconcile_watched_folder(
        &self,
        parent_id: NodeId,
        name: String,
        identity: notryx_server_storage::FolderIdentity,
    ) -> Result<Option<crate::NodeSnapshot>, RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let permit = Arc::clone(&self.observation_permits)
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Busy)?;
        let (mut reply, response) = oneshot::channel();
        let handle = self.clone();
        tokio::spawn(async move {
            let (probe_reply, probe_response) = oneshot::channel();
            if let Err(error) = handle.enqueue(Command::PrepareExternalFolder {
                parent_id,
                name,
                identity,
                reply: probe_reply,
            }) {
                let _ = reply.send(Err(error));
                return;
            }
            let probe = tokio::select! {
                _ = reply.closed() => return,
                result = probe_response => match result {
                    Ok(Ok(Some(probe))) => probe,
                    Ok(Ok(None)) => { let _ = reply.send(Ok(None)); return; },
                    Ok(Err(error)) => { let _ = reply.send(Err(error)); return; },
                    Err(_) => { let _ = reply.send(Err(RuntimeError::Closed)); return; }
                }
            };
            #[cfg(test)]
            let (pause, original_done) = match handle.external_folder_pause.lock().unwrap().take() {
                Some((entered, release, done)) => (Some((entered, release)), done),
                None => (None, None),
            };
            #[cfg(test)]
            let require_observation = original_done.is_some();
            let candidate = match tokio::task::spawn_blocking(move || {
                let result = probe.observe_folder();
                #[cfg(test)]
                if let Some((entered, release)) = pause
                    && (!require_observation || result.is_ok())
                {
                    let _ = entered.send(());
                    let _ = release.recv();
                }
                result
            })
            .await
            {
                Ok(Ok(candidate)) => candidate,
                Ok(Err(error)) => {
                    #[cfg(test)]
                    if let Some(done) = original_done {
                        let _ = done.send(Err(RuntimeError::Workspace(error)));
                    }
                    let _ = reply.send(Err(RuntimeError::Workspace(error)));
                    return;
                }
                Err(_) => {
                    handle
                        .observation_worker_failed
                        .store(true, Ordering::Release);
                    #[cfg(test)]
                    if let Some(done) = original_done {
                        let _ = done.send(Err(RuntimeError::Worker));
                    }
                    let _ = reply.send(Err(RuntimeError::Worker));
                    return;
                }
            };
            let command = Command::CommitExternalFolder {
                candidate,
                reply,
                watch_handle: handle.clone(),
                #[cfg(test)]
                original_done,
                _permit: permit,
            };
            let _ = handle.commands.send(command).await;
        });
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn observe_missing_folder(
        &self,
        node_id: NodeId,
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
    ) -> Result<bool, RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let permit = Arc::clone(&self.observation_permits)
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Busy)?;
        let (mut reply, response) = oneshot::channel();
        let handle = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let (probe_reply, probe_response) = oneshot::channel();
            if let Err(error) = handle.enqueue(Command::PrepareExternalDelete {
                node_id,
                parent_id,
                identity,
                reply: probe_reply,
            }) {
                let _ = reply.send(Err(error));
                return;
            }
            let probe = tokio::select! {
                _ = reply.closed() => return,
                result = probe_response => match result {
                    Ok(Ok(probe)) => probe,
                    Ok(Err(error)) => { let _ = reply.send(Err(error)); return; },
                    Err(_) => { let _ = reply.send(Err(RuntimeError::Closed)); return; }
                }
            };
            let result =
                match tokio::task::spawn_blocking(move || probe.observe_folder_absence()).await {
                    Ok(result) => result.map_err(RuntimeError::Workspace),
                    Err(_) => {
                        handle
                            .observation_worker_failed
                            .store(true, Ordering::Release);
                        Err(RuntimeError::Worker)
                    }
                };
            let _ = reply.send(result);
        });
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    async fn reconcile_watched_delete(
        &self,
        node_id: NodeId,
        parent_id: NodeId,
        identity: notryx_server_storage::FolderIdentity,
        expected_watch_generation: u64,
    ) -> Result<(), RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let permit = Arc::clone(&self.observation_permits)
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Busy)?;
        let (mut reply, response) = oneshot::channel();
        let handle = self.clone();
        tokio::spawn(async move {
            let (probe_reply, probe_response) = oneshot::channel();
            if let Err(error) = handle.enqueue(Command::PrepareExternalDelete {
                node_id,
                parent_id,
                identity,
                reply: probe_reply,
            }) {
                let _ = reply.send(Err(error));
                return;
            }
            let probe = tokio::select! {
                _ = reply.closed() => return,
                result = probe_response => match result {
                    Ok(Ok(probe)) => probe,
                    Ok(Err(error)) => { let _ = reply.send(Err(error)); return; },
                    Err(_) => { let _ = reply.send(Err(RuntimeError::Closed)); return; }
                }
            };
            #[cfg(test)]
            let pause = handle.external_delete_pause.lock().unwrap().take();
            let candidate = match tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some((entered, release)) = pause {
                    return probe.observe_with_hook(|| {
                        let _ = entered.send(());
                        let _ = release.recv();
                    });
                }
                probe.observe()
            })
            .await
            {
                Ok(candidate) => candidate,
                Err(_) => {
                    handle
                        .observation_worker_failed
                        .store(true, Ordering::Release);
                    let _ = reply.send(Err(RuntimeError::Worker));
                    return;
                }
            };
            let command = Command::CommitExternalDelete {
                candidate,
                reply,
                notices: handle.notices.clone(),
                stopping: handle.stopping.clone(),
                _permit: permit,
                commands: handle.commands.clone(),
                watch_generation: handle.known_file_watch_generation.clone(),
                expected_watch_generation,
                #[cfg(test)]
                commit_pause: handle.external_delete_commit_pause.lock().unwrap().take(),
                #[cfg(test)]
                transaction_pause: handle
                    .external_delete_transaction_pause
                    .lock()
                    .unwrap()
                    .take(),
            };
            let _ = handle.commands.send(command).await;
        });
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    #[cfg(test)]
    pub(crate) async fn test_reconcile_new_file(
        &self,
        parent_id: NodeId,
        name: String,
        hook: impl FnMut(notryx_server_storage::StorageStage) -> Result<(), crate::Error>
        + Send
        + 'static,
    ) -> Result<crate::NodeSnapshot, RuntimeError> {
        self.reconcile_new_with(
            parent_id,
            name,
            |probe| probe.observe_with_storage_hook(hook),
            None,
            None,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn test_reconcile_new_file_with_recheck(
        &self,
        parent_id: NodeId,
        name: String,
        observe_hook: impl FnMut(notryx_server_storage::StorageStage) -> Result<(), crate::Error>
        + Send
        + 'static,
        recheck_hook: impl FnMut(ExternalObjectRecheckStage) -> Result<(), crate::Error>
        + Send
        + 'static,
    ) -> Result<crate::NodeSnapshot, RuntimeError> {
        self.reconcile_new_with(
            parent_id,
            name,
            |probe| probe.observe_with_storage_hook(observe_hook),
            Some(Arc::new(std::sync::Mutex::new(Box::new(recheck_hook)))),
            None,
        )
        .await
    }

    async fn reconcile_new_with(
        &self,
        parent_id: NodeId,
        name: String,
        observe: impl FnOnce(
            crate::reconcile::ExternalCreateProbe,
        ) -> Result<crate::reconcile::ExternalCreateCandidate, crate::Error>
        + Send
        + 'static,
        recheck_hook: Option<RecheckHook>,
        watch_parent: Option<notryx_server_storage::FolderIdentity>,
    ) -> Result<crate::NodeSnapshot, RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let permit = Arc::clone(&self.observation_permits)
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Busy)?;
        let (mut reply, response) = oneshot::channel();
        let handle = self.clone();
        tokio::spawn(async move {
            let (probe_reply, probe_response) = oneshot::channel();
            if let Err(error) = handle.enqueue(Command::PrepareExternalCreate {
                parent_id,
                name,
                watch_parent,
                reply: probe_reply,
            }) {
                let _ = reply.send(Err(error));
                return;
            }
            let probe = tokio::select! {
                _ = reply.closed() => return,
                result = probe_response => match result {
                    Ok(Ok(probe)) => probe,
                    Ok(Err(error)) => { let _ = reply.send(Err(error)); return; }
                    Err(_) => { let _ = reply.send(Err(RuntimeError::Closed)); return; }
                }
            };
            let result = match tokio::task::spawn_blocking(move || observe(probe)).await {
                Ok(result) => result,
                Err(_) => {
                    handle
                        .observation_worker_failed
                        .store(true, Ordering::Release);
                    let _ = reply.send(Err(RuntimeError::Worker));
                    return;
                }
            };
            let candidate = match result {
                Ok(candidate) => candidate,
                Err(error) => {
                    let _ = reply.send(Err(RuntimeError::Workspace(error)));
                    return;
                }
            };
            let command = Command::CommitExternalCreate {
                candidate,
                reply,
                notices: handle.notices.clone(),
                stopping: Arc::clone(&handle.stopping),
                _permit: permit,
                commands: handle.commands.clone(),
                recheck_hook,
            };
            let _ = handle.commands.send(command).await;
        });
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    #[cfg(test)]
    pub(crate) async fn test_reconcile_file(
        &self,
        node_id: NodeId,
        hook: impl FnMut(notryx_server_storage::StorageStage) -> Result<(), crate::Error>
        + Send
        + 'static,
    ) -> Result<Option<Document>, RuntimeError> {
        self.reconcile_with(node_id, None, |probe| probe.observe_with_storage_hook(hook))
            .await
    }

    async fn reconcile_with(
        &self,
        node_id: NodeId,
        watch_parent: Option<(NodeId, notryx_server_storage::FolderIdentity)>,
        observe: impl FnOnce(
            crate::reconcile::ExternalProbe,
        ) -> Result<crate::reconcile::ExternalCandidate, crate::Error>
        + Send
        + 'static,
    ) -> Result<Option<Document>, RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let permit = Arc::clone(&self.observation_permits)
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Busy)?;
        let (mut reply, response) = oneshot::channel();
        let handle = self.clone();
        tokio::spawn(async move {
            let (probe_reply, probe_response) = oneshot::channel();
            if let Err(error) = handle.enqueue(Command::PrepareExternal {
                node_id,
                watch_parent,
                reply: probe_reply,
            }) {
                let _ = reply.send(Err(error));
                return;
            }
            let probe = tokio::select! {
                _ = reply.closed() => return,
                result = probe_response => match result {
                    Ok(Ok(probe)) => probe,
                    Ok(Err(error)) => { let _ = reply.send(Err(error)); return; }
                    Err(_) => { let _ = reply.send(Err(RuntimeError::Closed)); return; }
                }
            };
            let result = match tokio::task::spawn_blocking(move || observe(probe)).await {
                Ok(result) => result,
                Err(_) => {
                    handle
                        .observation_worker_failed
                        .store(true, Ordering::Release);
                    let _ = reply.send(Err(RuntimeError::Worker));
                    return;
                }
            };
            let candidate = match result {
                Ok(candidate) => candidate,
                Err(error) => {
                    let _ = reply.send(Err(RuntimeError::Workspace(error)));
                    return;
                }
            };
            let command = Command::CommitExternal {
                candidate,
                reply,
                notices: handle.notices.clone(),
                stopping: Arc::clone(&handle.stopping),
                _permit: permit,
                commands: handle.commands.clone(),
            };
            let _ = handle.commands.send(command).await;
        });
        response.await.map_err(|_| RuntimeError::Closed)?
    }
    #[cfg(test)]
    pub(crate) fn test_notice(&self) {
        let _ = self.notices.send(());
    }

    pub(crate) fn event_subscription(&self) -> broadcast::Receiver<()> {
        self.notices.subscribe()
    }

    pub(crate) fn event_permit(
        &self,
        identity: &AuthIdentity,
    ) -> Result<events::EventPermit, RuntimeError> {
        self.event_permits
            .acquire(identity.device_id, identity.principal_id)
    }

    pub(crate) async fn event_info(
        &self,
        token: String,
    ) -> Result<(AuthIdentity, ServerInfo), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::EventInfo { token, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn changes(
        &self,
        token: String,
        after: ChangeSeq,
        through: Option<ChangeSeq>,
    ) -> Result<ChangesPage, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::Changes {
            token,
            after,
            through,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

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

    #[cfg(test)]
    pub(crate) fn test_save_enqueued(&self) -> usize {
        self.test_save_enqueued.load(Ordering::Acquire)
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
            watch_handle: self.clone(),
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
        #[cfg(test)]
        self.test_save_enqueued.fetch_add(1, Ordering::Release);
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

    pub async fn device_identity(&self, token: String) -> Result<DeviceIdentity, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::DeviceIdentity { token, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn device_current_role(
        &self,
        token: String,
    ) -> Result<DeviceCurrentRole, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::DeviceCurrentRole { token, reply })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    pub async fn renew_device(&self, token: String) -> Result<DeviceRenewal, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::RenewDevice { token, reply })?;
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
        if let AdminRequest::OnlineBackup {
            config_path,
            destination,
        } = &request
        {
            return crate::cold_backup::create_online(
                self,
                std::path::Path::new(config_path),
                std::path::Path::new(destination),
            )
            .await;
        }
        if let AdminRequest::UnsupportedItems {
            snapshot_id,
            offset,
            limit,
        } = &request
        {
            return unsupported::page(self, snapshot_id.as_deref(), *offset, *limit);
        }
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
    startup_scan: Option<tokio::task::JoinHandle<()>>,
    watcher: Option<tokio::task::JoinHandle<()>>,
    watcher_stop: watch::Sender<bool>,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<Result<(), RuntimeError>>>,
}

enum Startup {
    Open,
    Initialize,
}

struct KnownPage {
    files: Vec<(NodeId, Option<NodeId>)>,
    folders: Vec<(NodeId, Option<NodeId>)>,
    next: Option<NodeId>,
}

enum ScanChild {
    Candidate(String, bool),
    Unsupported(Vec<u8>, notryx_server_storage::WatchUnsupportedReason),
}

struct WatchFolder {
    node_id: NodeId,
    identity: notryx_server_storage::FolderIdentity,
    file: std::fs::File,
}

struct WatchFolderPage {
    folders: Vec<WatchFolder>,
    next: Option<NodeId>,
    incomplete: bool,
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
    journal_budget_bytes: u64,
) -> Result<AdminResult, RuntimeError> {
    match request {
        AdminRequest::JournalStatus {} => {
            let status = workspace
                .journal_status(journal_budget_bytes)
                .await
                .map_err(RuntimeError::Workspace)?;
            Ok(AdminResult::JournalStatus {
                workspace_epoch: status.workspace_epoch,
                head_seq: status.head_seq,
                journal_floor_seq: status.journal_floor_seq,
                logical_bytes: status.logical_bytes.to_string(),
                budget_bytes: status.budget_bytes.to_string(),
                budget_exceeded: status.budget_exceeded,
                accounting: JournalAccounting::SqliteBlobLengthsV1,
            })
        }
        AdminRequest::UnsupportedItems { .. } | AdminRequest::OnlineBackup { .. } => {
            Err(RuntimeError::Workspace(crate::Error::InvalidRequest))
        }
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

fn schedule_object_recheck(workspace: &mut Workspace, job: RecheckJob) {
    let reader = match workspace.files.observation_reader() {
        Ok(reader) => reader,
        Err(_) => {
            workspace.object_rechecks -= 1;
            workspace.ready = false;
            job.reply.finish(crate::Error::Storage);
            return;
        }
    };
    tokio::spawn(async move {
        let hash = job.hash.clone();
        #[cfg(test)]
        let hook = job.hook.clone();
        #[cfg(not(test))]
        let hook = job.hook;
        let result = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(hook) = &hook {
                (hook
                    .lock()
                    .map_err(|_| notryx_server_storage::StorageError::Io)?)(
                    ExternalObjectRecheckStage::BeforeVerify,
                )
                .map_err(|_| notryx_server_storage::StorageError::Io)?;
            }
            #[cfg(not(test))]
            let _ = hook;
            let observed = reader.observe_object(&hash);
            #[cfg(test)]
            if let Some(hook) = &hook {
                (hook
                    .lock()
                    .map_err(|_| notryx_server_storage::StorageError::Io)?)(
                    ExternalObjectRecheckStage::AfterVerify,
                )
                .map_err(|_| notryx_server_storage::StorageError::Io)?;
            }
            observed
        })
        .await
        .unwrap_or(Err(notryx_server_storage::StorageError::Io));
        let commands = job.commands.clone();
        let _ = commands
            .send(Command::ReferencedObjectRechecked { job, result })
            .await;
    });
}

async fn finish_object_recheck(
    workspace: &mut Workspace,
    mut job: RecheckJob,
    result: Result<notryx_server_storage::FileStamp, notryx_server_storage::StorageError>,
) {
    use notryx_server_storage::StorageError;
    enum Outcome {
        Complete(crate::Error),
        Retry,
    }
    let outcome = if workspace.epoch() != job.epoch {
        Outcome::Complete(crate::Error::ExternalChange)
    } else {
        match workspace
            .state
            .retained_object_is_referenced(&job.hash)
            .await
        {
            Ok(false) => Outcome::Complete(
                job.preparation_error
                    .unwrap_or(crate::Error::ExternalChange),
            ),
            Err(_) => {
                workspace.ready = false;
                Outcome::Complete(crate::Error::Storage)
            }
            Ok(true) => match result {
                Ok(stamp) => match workspace
                    .files
                    .object_matches_observation(&job.hash, &stamp)
                {
                    Ok(true) if stamp.hash == job.hash => Outcome::Complete(
                        job.preparation_error
                            .unwrap_or(crate::Error::ExternalChange),
                    ),
                    Ok(true) => {
                        workspace.ready = false;
                        Outcome::Complete(crate::Error::InvalidFormat)
                    }
                    Ok(false) | Err(StorageError::Changed) => Outcome::Retry,
                    Err(_) => {
                        workspace.ready = false;
                        Outcome::Complete(crate::Error::Storage)
                    }
                },
                Err(StorageError::NotFound) => match workspace.files.object_file_present(&job.hash)
                {
                    Ok(true) => Outcome::Retry,
                    _ => {
                        workspace.ready = false;
                        Outcome::Complete(crate::Error::Storage)
                    }
                },
                Err(StorageError::UnsafeFile) => {
                    match workspace.files.object_file_present(&job.hash) {
                        Ok(true) => Outcome::Retry,
                        _ => {
                            workspace.ready = false;
                            Outcome::Complete(crate::Error::Storage)
                        }
                    }
                }
                Err(StorageError::Changed) => Outcome::Retry,
                Err(_) => {
                    workspace.ready = false;
                    Outcome::Complete(crate::Error::Storage)
                }
            },
        }
    };
    match outcome {
        Outcome::Retry if job.attempts < 2 => {
            job.attempts += 1;
            schedule_object_recheck(workspace, job);
        }
        Outcome::Retry => {
            workspace.ready = false;
            workspace.object_rechecks -= 1;
            job.reply.finish(crate::Error::Storage);
        }
        Outcome::Complete(error) => {
            workspace.object_rechecks -= 1;
            job.reply.finish(error);
        }
    }
}

async fn execute(
    command: Command,
    workspace: &mut Workspace,
    shutting_down: bool,
    journal_budget_bytes: u64,
) {
    match command {
        Command::BackupSnapshot {
            target_state,
            control,
            observation,
            reply,
        } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    backup::capture(workspace, target_state, control, observation).await
                };
                let _ = reply.send(result);
            }
        }
        Command::ScanHead { reply } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    workspace
                        .state
                        .head_seq()
                        .await
                        .map_err(RuntimeError::Workspace)
                };
                let _ = reply.send(result);
            }
        }
        Command::PruneWatchRetry {
            node_id,
            attempted_at,
            attempts,
            reply,
        } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    match workspace.state.node(node_id).await {
                        Ok(Some(node))
                            if node.deleted && node.kind == notryx_protocol::NodeKind::Folder =>
                        {
                            match attempts.lock() {
                                Ok(mut attempts) => {
                                    if attempts.get(&node_id) == Some(&attempted_at) {
                                        attempts.remove(&node_id);
                                    }
                                    Ok(())
                                }
                                Err(_) => Err(RuntimeError::Closed),
                            }
                        }
                        Ok(_) => Ok(()),
                        Err(error) => Err(RuntimeError::Workspace(error)),
                    }
                };
                let _ = reply.send(result);
            }
        }
        Command::WatchFolderDeleted { node_id, reply } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    workspace
                        .state
                        .node(node_id)
                        .await
                        .map(|node| {
                            node.is_some_and(|node| {
                                node.deleted && node.kind == notryx_protocol::NodeKind::Folder
                            })
                        })
                        .map_err(RuntimeError::Workspace)
                };
                let _ = reply.send(result);
            }
        }
        Command::WatchFolder { node_id, reply } => {
            if !reply.is_closed() {
                let result = async {
                    if shutting_down {
                        return Err(RuntimeError::Closed);
                    }
                    let node = workspace
                        .state
                        .node(node_id)
                        .await
                        .map_err(RuntimeError::Workspace)?
                        .ok_or(RuntimeError::Workspace(crate::Error::NotFound))?;
                    if node.deleted || node.kind != notryx_protocol::NodeKind::Folder {
                        return Err(RuntimeError::Workspace(crate::Error::ExternalChange));
                    }
                    let path = workspace
                        .state
                        .node_path(node_id)
                        .await
                        .map_err(RuntimeError::Workspace)?;
                    let (file, identity) = workspace
                        .files
                        .open_watch_folder(&path)
                        .map_err(|_| RuntimeError::Workspace(crate::Error::ExternalChange))?;
                    Ok(WatchFolder {
                        node_id,
                        identity,
                        file,
                    })
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::WatchFolderPage { after, reply } => {
            if !reply.is_closed() {
                let result = async {
                    if shutting_down {
                        return Err(RuntimeError::Closed);
                    }
                    let mut nodes = workspace
                        .state
                        .inventory_nodes(after, 8)
                        .await
                        .map_err(RuntimeError::Workspace)?;
                    let has_more = nodes.len() > 8;
                    nodes.truncate(8);
                    let next = has_more.then(|| nodes.last().expect("nonempty page").node_id);
                    let mut folders = Vec::new();
                    let mut incomplete = false;
                    for node in nodes
                        .into_iter()
                        .filter(|node| node.kind == notryx_protocol::NodeKind::Folder)
                    {
                        let path = match workspace.state.node_path(node.node_id).await {
                            Ok(path) => path,
                            Err(_) => {
                                incomplete = true;
                                continue;
                            }
                        };
                        match workspace.files.open_watch_folder(&path) {
                            Ok((file, identity)) => folders.push(WatchFolder {
                                node_id: node.node_id,
                                identity,
                                file,
                            }),
                            Err(_error) => {
                                #[cfg(test)]
                                if std::env::var_os("NOTRYX_TEST_WATCH_LOW_FD").is_some() {
                                    let mut probe = Vec::new();
                                    for _ in 0..8 {
                                        match std::fs::File::open("/dev/null") {
                                            Ok(file) => probe.push(file),
                                            Err(error) => {
                                                eprintln!(
                                                    "low-fd stage: folder open {_error:?}; fd probe errno {:?}",
                                                    error.raw_os_error()
                                                );
                                                break;
                                            }
                                        }
                                    }
                                }
                                incomplete = true;
                            }
                        }
                    }
                    Ok(WatchFolderPage {
                        folders,
                        next,
                        incomplete,
                    })
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::WatchLookup {
            parent_id,
            identity,
            name,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    if shutting_down {
                        return Err(RuntimeError::Closed);
                    }
                    let parent = workspace
                        .state
                        .node(parent_id)
                        .await
                        .map_err(RuntimeError::Workspace)?
                        .ok_or(RuntimeError::Workspace(crate::Error::ExternalChange))?;
                    if parent.deleted || parent.kind != notryx_protocol::NodeKind::Folder {
                        return Err(RuntimeError::Workspace(crate::Error::ExternalChange));
                    }
                    let path = workspace
                        .state
                        .node_path(parent_id)
                        .await
                        .map_err(RuntimeError::Workspace)?;
                    if !workspace
                        .files
                        .watch_folder_matches(&path, identity)
                        .map_err(|_| RuntimeError::Workspace(crate::Error::ExternalChange))?
                    {
                        return Err(RuntimeError::Workspace(crate::Error::ExternalChange));
                    }
                    match name {
                        Some(name) => workspace
                            .state
                            .known_file_child(parent_id, &name)
                            .await
                            .map_err(RuntimeError::Workspace),
                        None => Ok(None),
                    }
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::PrepareFolderScan {
            parent_id,
            identity,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    if shutting_down {
                        return Err(RuntimeError::Closed);
                    }
                    let path = workspace
                        .state
                        .node_path(parent_id)
                        .await
                        .map_err(RuntimeError::Workspace)?;
                    if !workspace
                        .files
                        .watch_folder_matches(&path, identity)
                        .map_err(|_| RuntimeError::Workspace(crate::Error::ExternalChange))?
                    {
                        return Err(RuntimeError::Workspace(crate::Error::ExternalChange));
                    }
                    let reader = workspace
                        .files
                        .observation_reader()
                        .map_err(crate::Error::from)
                        .map_err(RuntimeError::Workspace)?;
                    Ok((path, reader))
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::ScanKnownPage { after, reply } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    workspace
                        .state
                        .inventory_nodes(after, 64)
                        .await
                        .map(|mut nodes| {
                            let has_more = nodes.len() > 64;
                            nodes.truncate(64);
                            let next =
                                has_more.then(|| nodes.last().expect("nonempty page").node_id);
                            let folders = nodes
                                .iter()
                                .filter(|node| node.kind == notryx_protocol::NodeKind::Folder)
                                .map(|node| (node.node_id, node.parent_id))
                                .collect();
                            let files = nodes
                                .into_iter()
                                .filter(|node| node.kind == notryx_protocol::NodeKind::File)
                                .map(|node| (node.node_id, node.parent_id))
                                .collect();
                            KnownPage {
                                files,
                                folders,
                                next,
                            }
                        })
                        .map_err(RuntimeError::Workspace)
                };
                let _ = reply.send(result);
            }
        }
        Command::PrepareExternal {
            node_id,
            watch_parent,
            reply,
        } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    match watch_parent {
                        Some((parent_id, identity)) => {
                            workspace
                                .prepare_watched_external_file(node_id, parent_id, identity)
                                .await
                        }
                        None => workspace.prepare_external_file(node_id).await,
                    }
                    .map_err(RuntimeError::Workspace)
                };
                let _ = reply.send(result);
            }
        }
        Command::CommitExternal {
            candidate,
            reply,
            notices,
            stopping,
            _permit,
            commands,
        } => {
            let diagnostic = workspace
                .diagnose_external_file(&candidate)
                .await
                .map_err(RuntimeError::Workspace);
            let result = match diagnostic {
                Err(error) => Err(error),
                Ok(crate::reconcile::ExternalDiagnostic::VerifyObject(hash, preparation_error)) => {
                    let epoch = workspace.epoch();
                    workspace.object_rechecks += 1;
                    schedule_object_recheck(
                        workspace,
                        RecheckJob {
                            hash,
                            epoch,
                            attempts: 0,
                            preparation_error,
                            reply: RecheckReply::Existing(reply),
                            _permit,
                            hook: None,
                            commands,
                        },
                    );
                    return;
                }
                Ok(crate::reconcile::ExternalDiagnostic::Clear)
                    if shutting_down || stopping.load(Ordering::Acquire) || reply.is_closed() =>
                {
                    Err(RuntimeError::Closed)
                }
                Ok(crate::reconcile::ExternalDiagnostic::Clear) => workspace
                    .commit_external_file(candidate)
                    .await
                    .map_err(RuntimeError::Workspace),
            };
            if result.as_ref().is_ok_and(|document| document.is_some()) {
                let _ = notices.send(());
            }
            let _ = reply.send(result);
        }
        Command::PrepareExternalDelete {
            node_id,
            parent_id,
            identity,
            reply,
        } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    workspace
                        .prepare_external_delete(node_id, parent_id, identity)
                        .await
                        .map_err(RuntimeError::Workspace)
                };
                let _ = reply.send(result);
            }
        }
        Command::CommitExternalDelete {
            candidate,
            reply,
            notices,
            stopping,
            _permit,
            commands,
            watch_generation,
            expected_watch_generation,
            #[cfg(test)]
            commit_pause,
            #[cfg(test)]
            transaction_pause,
        } => {
            let diagnostic = workspace
                .diagnose_external_delete(&candidate)
                .await
                .map_err(RuntimeError::Workspace);
            let result = match diagnostic {
                Err(error) => Err(error),
                Ok(crate::reconcile::ExternalDiagnostic::VerifyObject(hash, preparation_error)) => {
                    let epoch = workspace.epoch();
                    workspace.object_rechecks += 1;
                    schedule_object_recheck(
                        workspace,
                        RecheckJob {
                            hash,
                            epoch,
                            attempts: 0,
                            preparation_error,
                            reply: RecheckReply::Delete(reply),
                            _permit,
                            hook: None,
                            commands,
                        },
                    );
                    return;
                }
                Ok(crate::reconcile::ExternalDiagnostic::Clear)
                    if shutting_down || stopping.load(Ordering::Acquire) || reply.is_closed() =>
                {
                    Err(RuntimeError::Closed)
                }
                Ok(crate::reconcile::ExternalDiagnostic::Clear)
                    if expected_watch_generation & 1 != 0
                        || watch_generation.load(Ordering::Acquire)
                            != expected_watch_generation =>
                {
                    Err(RuntimeError::Workspace(crate::Error::ExternalChange))
                }
                Ok(crate::reconcile::ExternalDiagnostic::Clear) => workspace
                    .commit_external_delete(
                        candidate,
                        &watch_generation,
                        expected_watch_generation,
                        #[cfg(test)]
                        commit_pause,
                        #[cfg(test)]
                        transaction_pause,
                    )
                    .await
                    .map_err(RuntimeError::Workspace),
            };
            if result.is_ok() {
                let _ = notices.send(());
            }
            let _ = reply.send(result);
        }
        Command::PrepareExternalFolder {
            parent_id,
            name,
            identity,
            reply,
        } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    workspace
                        .prepare_watched_external_folder(parent_id, &name, identity)
                        .await
                        .map_err(RuntimeError::Workspace)
                };
                let _ = reply.send(result);
            }
        }
        Command::CommitExternalFolder {
            candidate,
            reply,
            watch_handle,
            #[cfg(test)]
            original_done,
            _permit,
        } => {
            if !shutting_down
                && !watch_handle.stopping.load(Ordering::Acquire)
                && !reply.is_closed()
            {
                let result = workspace
                    .commit_external_folder(candidate)
                    .await
                    .map_err(RuntimeError::Workspace);
                #[cfg(test)]
                {
                    if let Some(events) =
                        watch_handle.external_folder_trace.lock().unwrap().as_mut()
                    {
                        events.push((original_done.is_some(), result.clone()));
                    }
                    if let Some(done) = original_done {
                        let _ = done.send(result.clone());
                    }
                }
                if let Ok(node) = &result {
                    watch_handle.request_watch_folder(node.node_id);
                    let _ = watch_handle.notices.send(());
                }
                let _ = reply.send(result.map(Some));
            }
        }
        Command::PrepareExternalCreate {
            parent_id,
            name,
            watch_parent,
            reply,
        } => {
            if !reply.is_closed() {
                let result = if shutting_down {
                    Err(RuntimeError::Closed)
                } else {
                    match watch_parent {
                        Some(identity) => {
                            workspace
                                .prepare_watched_external_create(parent_id, &name, identity)
                                .await
                        }
                        None => workspace.prepare_external_create(parent_id, &name).await,
                    }
                    .map_err(RuntimeError::Workspace)
                };
                let _ = reply.send(result);
            }
        }
        Command::CommitExternalCreate {
            candidate,
            reply,
            notices,
            stopping,
            _permit,
            commands,
            recheck_hook,
        } => {
            let diagnostic = workspace
                .diagnose_external_create(&candidate)
                .await
                .map_err(RuntimeError::Workspace);
            let result = match diagnostic {
                Err(error) => Err(error),
                Ok(crate::reconcile::ExternalDiagnostic::VerifyObject(hash, preparation_error)) => {
                    let epoch = workspace.epoch();
                    workspace.object_rechecks += 1;
                    schedule_object_recheck(
                        workspace,
                        RecheckJob {
                            hash,
                            epoch,
                            attempts: 0,
                            preparation_error,
                            reply: RecheckReply::New(reply),
                            _permit,
                            hook: recheck_hook,
                            commands,
                        },
                    );
                    return;
                }
                Ok(crate::reconcile::ExternalDiagnostic::Clear)
                    if shutting_down || stopping.load(Ordering::Acquire) || reply.is_closed() =>
                {
                    Err(RuntimeError::Closed)
                }
                Ok(crate::reconcile::ExternalDiagnostic::Clear) => workspace
                    .commit_external_create(candidate)
                    .await
                    .map_err(RuntimeError::Workspace),
            };
            if result.is_ok() {
                let _ = notices.send(());
            }
            let _ = reply.send(result);
        }
        Command::ReferencedObjectRechecked { job, result } => {
            finish_object_recheck(workspace, job, result).await;
        }
        Command::EventInfo { token, reply } => {
            if !reply.is_closed() {
                let result = async {
                    if shutting_down {
                        return Err(RuntimeError::Closed);
                    }
                    let identity = workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    let info = workspace
                        .server_info()
                        .await
                        .map_err(RuntimeError::Workspace)?;
                    Ok((identity, info))
                }
                .await;
                let _ = reply.send(result);
            }
        }
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
        #[cfg(test)]
        Command::InstallExternalCommitPause {
            entered,
            release,
            reply,
        } => {
            let mut entered = Some(entered);
            workspace.hook = Some(Box::new(move |phase| {
                if phase == crate::Phase::BeforeRecoveryFinish
                    && let Some(entered) = entered.take()
                {
                    let _ = entered.send(());
                    release
                        .recv_timeout(std::time::Duration::from_secs(15))
                        .map_err(|_| crate::Error::Storage)?;
                }
                Ok(())
            }));
            let _ = reply.send(());
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
        Command::Changes {
            token,
            after,
            through,
            reply,
        } => {
            if !reply.is_closed() {
                let result = async {
                    workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    workspace
                        .changes(after, through)
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
            watch_handle,
            reply,
        } => {
            if !reply.is_closed() {
                let folder_id =
                    (request.kind == notryx_protocol::NodeKind::Folder).then_some(request.node_id);
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
                if let (Some(node_id), Ok(record)) = (folder_id, &result)
                    && matches!(&record.outcome, crate::Outcome::Created(_))
                {
                    watch_handle.request_watch_folder(node_id);
                }
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
                let _ = reply.send(execute_admin(workspace, request, journal_budget_bytes).await);
            }
        }
        Command::RenewDevice { token, reply } => {
            if !reply.is_closed() {
                let result = match now() {
                    Ok(now) if !shutting_down => workspace
                        .renew_device(&token, now)
                        .await
                        .map_err(RuntimeError::Auth),
                    Ok(_) => Err(RuntimeError::Closed),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
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
        Command::DeviceIdentity { token, reply } => {
            if !reply.is_closed() {
                let result = async {
                    if shutting_down {
                        return Err(RuntimeError::Closed);
                    }
                    let identity = workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    Ok(DeviceIdentity {
                        protocol: notryx_protocol::CONTRACT.into(),
                        workspace_id: workspace.workspace_id(),
                        workspace_epoch: workspace.epoch(),
                        device_id: identity.device_id.to_string(),
                        device_stream_id: identity.stream_id.to_string(),
                    })
                }
                .await;
                let _ = reply.send(result);
            }
        }
        Command::DeviceCurrentRole { token, reply } => {
            if !reply.is_closed() {
                let result = async {
                    if shutting_down {
                        return Err(RuntimeError::Closed);
                    }
                    let identity = workspace
                        .authenticate(&token, now()?)
                        .await
                        .map_err(RuntimeError::Auth)?;
                    Ok(DeviceCurrentRole {
                        protocol: notryx_protocol::CONTRACT.into(),
                        workspace_id: workspace.workspace_id(),
                        workspace_epoch: workspace.epoch(),
                        device_id: identity.device_id.to_string(),
                        device_stream_id: identity.stream_id.to_string(),
                        role: identity.role,
                    })
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

async fn run_journal_retention_tick(
    workspace: &mut Workspace,
    retention_seconds: u64,
    budget_bytes: u64,
    control: &mut WorkerRetentionControl,
) -> Result<crate::retention::RetentionReport, RuntimeError> {
    #[cfg(test)]
    if let Some(control) = control.as_mut() {
        use journal_retention_test_support::RetentionTestEvent;
        let _ = control.events.send(RetentionTestEvent::Started);
        let result = match control.before_clock().await {
            Ok(()) => {
                let now_seconds = control.now_seconds();
                let before_commit = control.take_before_commit();
                workspace
                    .retain_journal_test(
                        control.retention_seconds,
                        control.budget_bytes,
                        now_seconds,
                        before_commit,
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        let event = match &result {
            Ok(_) => RetentionTestEvent::Success,
            Err(error) => RetentionTestEvent::Failure(*error),
        };
        let _ = control.events.send(event);
        return result.map_err(RuntimeError::Workspace);
    }
    #[cfg(not(test))]
    let _ = control;
    let now_seconds = now()?;
    workspace
        .retain_journal_report(retention_seconds, budget_bytes, now_seconds)
        .await
        .map_err(RuntimeError::Workspace)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn run_worker(
    config: WorkspaceConfig,
    startup: Startup,
    journal_policy: JournalRetentionConfig,
    retention_seconds: u64,
    started: oneshot::Sender<Result<(), RuntimeError>>,
    mut commands: mpsc::Receiver<Command>,
    mut stop: oneshot::Receiver<()>,
    stopping: Arc<AtomicBool>,
    notices: broadcast::Sender<()>,
    mut retention_test: WorkerRetentionControl,
) -> Result<(), RuntimeError> {
    let retention_enabled = matches!(&startup, Startup::Open);
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
    #[cfg(test)]
    let retention_interval = retention_test
        .as_ref()
        .map_or(JOURNAL_MAINTENANCE_INTERVAL, |control| control.interval);
    #[cfg(not(test))]
    let retention_interval = JOURNAL_MAINTENANCE_INTERVAL;
    if retention_interval.is_zero() {
        let _ = started.send(Err(RuntimeError::Start));
        workspace.close().await.map_err(RuntimeError::Workspace)?;
        return Err(RuntimeError::Start);
    }
    let mut retention_timer = tokio::time::interval(retention_interval);
    retention_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    if started.send(Ok(())).is_err() {
        return workspace.close().await.map_err(RuntimeError::Workspace);
    }
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                commands.close();
                while let Some(command) = commands.recv().await { execute(command, &mut workspace, true, journal_policy.journal_budget_bytes).await; }
                break;
            }
            _ = retention_timer.tick(), if retention_enabled && !stopping.load(Ordering::Acquire) => {
                if stopping.load(Ordering::Acquire) {
                    continue;
                }
                match run_journal_retention_tick(
                    &mut workspace,
                    retention_seconds,
                    journal_policy.journal_budget_bytes,
                    &mut retention_test,
                ).await {
                    Ok(report) => eprintln!("{}", serde_json::json!({
                        "event": "journal_retention_completed",
                        "before_floor": report.before_floor.to_string(),
                        "after_floor": report.after_floor.to_string(),
                        "logical_bytes": report.logical_bytes.to_string(),
                        "budget_bytes": report.budget_bytes.to_string(),
                        "budget_exceeded": report.budget_exceeded,
                        "expired_count": report.expired_count.to_string(),
                        "budget_forced_count": report.budget_forced_count.to_string(),
                        "accounting": "sqlite_blob_lengths_v1"
                    })),
                    Err(error) => eprintln!("{{\"event\":\"journal_retention_failed\",\"code\":\"{}\"}}", error.code()),
                }
                retention_timer.reset();
            }
            command = commands.recv() => match command {
                Some(command) => {
                    let notify = matches!(&command,
                        Command::DeleteNode { .. } | Command::RestoreNode { .. } |
                        Command::MoveNode { .. } | Command::CreateNode { .. } |
                        Command::SaveContent { .. } |
                        Command::Admin { request: AdminRequest::SetRole { .. } |
                            AdminRequest::TransferOwner { .. } | AdminRequest::RevokeDevice { .. } |
                            AdminRequest::RecoveryResolve { .. } | AdminRequest::RecoveryRelease { .. }, .. });
                    execute(command, &mut workspace, false, journal_policy.journal_budget_bytes).await;
                    if notify { let _ = notices.send(()); }
                }
                None => break,
            },
        }
    }
    workspace.close().await.map_err(RuntimeError::Workspace)
}

#[cfg(test)]
struct StartupTestControl {
    fail_watch: bool,
    disconnect_ready: bool,
    pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    sweep_interval: Option<std::time::Duration>,
    retention: Option<journal_retention_test_support::RetentionTestControl>,
}

#[cfg(not(test))]
type StartupTestControl = ();

impl WorkspaceRuntime {
    pub async fn start(config: WorkspaceConfig) -> Result<Self, RuntimeError> {
        Self::start_with(config, Startup::Open).await
    }

    pub async fn start_with_policy(
        config: WorkspaceConfig,
        journal_policy: JournalRetentionConfig,
    ) -> Result<Self, RuntimeError> {
        Self::start_with_policy_options(config, Startup::Open, journal_policy, true, None).await
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

    #[cfg(test)]
    pub(crate) async fn test_wait_startup_scan(&mut self) {
        if let Some(scan) = self.startup_scan.take() {
            scan.await.expect("startup scan task failed");
        }
    }

    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        self.begin_shutdown();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.await;
        }
        if let Some(scan) = self.startup_scan.take() {
            let _ = scan.await;
        }
        self.handle.uploads.drain().await;
        let _observations = Arc::clone(&self.handle.observation_permits)
            .acquire_many_owned(2)
            .await
            .map_err(|_| RuntimeError::Closed)?;
        self.stop_worker();
        let worker = self.worker.take().ok_or(RuntimeError::Closed)?;
        tokio::task::spawn_blocking(move || worker.join().map_err(|_| RuntimeError::Worker)?)
            .await
            .map_err(|_| RuntimeError::Worker)?
    }

    pub(crate) fn begin_shutdown(&mut self) {
        self.handle.stopping.store(true, Ordering::Release);
        self.watcher_stop.send_replace(true);
        let _ = self.handle.notices.send(());
        self.handle.uploads.cancel();
    }

    #[cfg(test)]
    pub(crate) async fn test_start_without_known_file_watch(
        config: WorkspaceConfig,
    ) -> Result<Self, RuntimeError> {
        Self::start_with_options(config, Startup::Open, false, None).await
    }

    #[cfg(test)]
    pub(crate) async fn test_start_with_watch_control(
        config: WorkspaceConfig,
        fail_watch: bool,
        disconnect_ready: bool,
        pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    ) -> Result<Self, RuntimeError> {
        Self::start_with_options(
            config,
            Startup::Open,
            true,
            Some(StartupTestControl {
                fail_watch,
                disconnect_ready,
                pause,
                sweep_interval: None,
                retention: None,
            }),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn test_start_with_sweep_interval(
        config: WorkspaceConfig,
        interval: std::time::Duration,
    ) -> Result<Self, RuntimeError> {
        Self::start_with_options(
            config,
            Startup::Open,
            true,
            Some(StartupTestControl {
                fail_watch: false,
                disconnect_ready: false,
                pause: None,
                sweep_interval: Some(interval),
                retention: None,
            }),
        )
        .await
    }

    async fn start_with(config: WorkspaceConfig, startup: Startup) -> Result<Self, RuntimeError> {
        Self::start_with_options(config, startup, true, None).await
    }

    async fn start_with_options(
        config: WorkspaceConfig,
        startup: Startup,
        watch_enabled: bool,
        _test_control: Option<StartupTestControl>,
    ) -> Result<Self, RuntimeError> {
        Self::start_with_policy_options(
            config,
            startup,
            JournalRetentionConfig::default(),
            watch_enabled,
            _test_control,
        )
        .await
    }

    #[cfg(test)]
    async fn test_start_with_retention_control(
        config: WorkspaceConfig,
        control: journal_retention_test_support::RetentionTestControl,
    ) -> Result<Self, RuntimeError> {
        Self::start_with_options(
            config,
            Startup::Open,
            false,
            Some(StartupTestControl {
                fail_watch: false,
                disconnect_ready: false,
                pause: None,
                sweep_interval: None,
                retention: Some(control),
            }),
        )
        .await
    }

    async fn start_with_policy_options(
        config: WorkspaceConfig,
        startup: Startup,
        journal_policy: JournalRetentionConfig,
        watch_enabled: bool,
        #[allow(unused_mut)] mut _test_control: Option<StartupTestControl>,
    ) -> Result<Self, RuntimeError> {
        let retention_seconds = journal_policy
            .retention_seconds()
            .map_err(|_| RuntimeError::Start)?;
        #[cfg(test)]
        let retention_test = _test_control
            .as_mut()
            .and_then(|control| control.retention.take());
        #[cfg(not(test))]
        let retention_test = ();
        let scan_on_start = matches!(&startup, Startup::Open);
        #[cfg(test)]
        let watch_scan_retry_interval = _test_control
            .as_ref()
            .and_then(|control| control.sweep_interval)
            .unwrap_or(std::time::Duration::from_secs(30));
        #[cfg(not(test))]
        let watch_scan_retry_interval = std::time::Duration::from_secs(30);
        let (watcher_stop, watcher_stopped) = watch::channel(false);
        let uploads = Arc::new(UploadState::new(config.uploads.max_receiving));
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (watch_folder_requests, watch_folder_receiver) = mpsc::channel(256);
        let (started, response) = oneshot::channel();
        let (stop, stopped) = oneshot::channel();
        let (notices, _) = broadcast::channel(32);
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let worker_notices = notices.clone();
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
                runtime.block_on(run_worker(
                    config,
                    startup,
                    journal_policy,
                    retention_seconds,
                    started,
                    receiver,
                    stopped,
                    worker_stopping,
                    worker_notices,
                    retention_test,
                ))
            })
            .map_err(|_| RuntimeError::Start)?;
        let mut runtime = Self {
            handle: WorkspaceHandle {
                commands,
                backup_permit: Arc::new(Semaphore::new(1)),
                stopping,
                uploads,
                notices,
                event_permits: Arc::new(events::EventPermits::default()),
                observation_permits: Arc::new(Semaphore::new(2)),
                observation_worker_failed: Arc::new(AtomicBool::new(false)),
                known_file_scan_incomplete: Arc::new(AtomicBool::new(scan_on_start)),
                known_file_watch_degraded: Arc::new(AtomicBool::new(false)),
                watch_scan_registration_attempts: Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                watch_scan_retry_interval,
                known_file_watch_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                known_file_scan_retry: Arc::new(tokio::sync::Notify::new()),
                diagnostic_scan_retry: Arc::new(tokio::sync::Notify::new()),
                watch_folder_requests,
                watch_folder_recovery: Arc::new(tokio::sync::Notify::new()),
                watch_folder_recovery_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                watch_folder_recovery_completed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                watch_enabled,
                unsupported_items: Arc::new(std::sync::Mutex::new(unsupported::State::default())),
                scan_lock: Arc::new(tokio::sync::Mutex::new(())),
                #[cfg(test)]
                known_file_scan_events: Arc::new(std::sync::Mutex::new(Vec::new())),
                #[cfg(test)]
                known_file_watch_max_batch: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                #[cfg(test)]
                known_file_watch_scan_waiting: Arc::new(AtomicBool::new(false)),
                #[cfg(test)]
                known_file_watch_scan_panic: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                #[cfg(test)]
                known_file_watch_job_fault: Arc::new(std::sync::atomic::AtomicU8::new(0)),
                #[cfg(test)]
                watch_folder_failure: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                watch_scan_retry_override: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                stale_watch_retry_cleanup_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                deleted_folder_registration_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                deleted_folder_registration_observation: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                successful_folder_registration_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                watch_self_event: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_watch_max_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                #[cfg(test)]
                known_file_watch_lookup_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_watch_suppressed_name: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_watch_test_overflow: Arc::new(tokio::sync::Notify::new()),
                #[cfg(test)]
                known_file_watch_test_read_error: Arc::new(tokio::sync::Notify::new()),
                #[cfg(test)]
                known_file_watch_test_read_error_ack: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_watch_injected_read_errors: Arc::new(
                    std::sync::atomic::AtomicUsize::new(0),
                ),
                #[cfg(test)]
                known_file_watch_observed_read_errors: Arc::new(
                    std::sync::atomic::AtomicUsize::new(0),
                ),
                #[cfg(test)]
                known_file_watch_read_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_scan_finish_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                diagnostic_scan_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                diagnostic_scan_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                #[cfg(test)]
                known_file_watch_recovered_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_watch_folder_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_missing_confirm_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_watch_recovery_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_file_watch_folder_jobs_done: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                #[cfg(test)]
                confirm_missing_fault: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                known_new_file_scan_max_buffer: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                #[cfg(test)]
                known_new_file_scan_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                external_delete_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                external_folder_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                external_folder_trace: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                external_delete_commit_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                external_delete_transaction_pause: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                watch_name_signal: Arc::new(std::sync::Mutex::new(None)),
                #[cfg(test)]
                test_save_enqueued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            startup_scan: None,
            watcher: None,
            watcher_stop,
            stop: Some(stop),
            worker: Some(worker),
        };
        match response.await {
            Ok(Ok(())) => {
                if scan_on_start {
                    #[cfg(test)]
                    let fail_watch = _test_control
                        .as_ref()
                        .is_some_and(|control| control.fail_watch);
                    #[cfg(not(test))]
                    let fail_watch = false;
                    #[cfg(test)]
                    let disconnect_ready = _test_control
                        .as_ref()
                        .is_some_and(|control| control.disconnect_ready);
                    #[cfg(not(test))]
                    let disconnect_ready = false;
                    #[cfg(test)]
                    let test_sweep_interval = _test_control
                        .as_ref()
                        .and_then(|control| control.sweep_interval);
                    #[cfg(not(test))]
                    let test_sweep_interval = None;
                    if fail_watch {
                        runtime.handle.mark_known_file_watch_degraded();
                    }
                    let watch_registered = if watch_enabled {
                        if fail_watch {
                            None
                        } else {
                            let (watch_ready, registered) = oneshot::channel();
                            let watch_handle = runtime.handle.clone();
                            runtime.watcher = if disconnect_ready {
                                Some(tokio::spawn(async move {
                                    drop(watch_ready);
                                }))
                            } else {
                                Some(tokio::spawn(watcher::run(
                                    watch_handle,
                                    watch_ready,
                                    watcher_stopped,
                                    test_sweep_interval,
                                    watch_folder_receiver,
                                )))
                            };
                            Some(registered)
                        }
                    } else {
                        None
                    };
                    let handle = runtime.handle.clone();
                    runtime.startup_scan = Some(tokio::spawn(async move {
                        let watch_folders = if watch_enabled {
                            match watch_registered {
                                Some(registered) => registered.await.ok().flatten().map(Some),
                                None => None,
                            }
                        } else {
                            Some(None)
                        };
                        #[cfg(test)]
                        if let Some((entered, release)) =
                            _test_control.and_then(|control| control.pause)
                        {
                            let _ = entered.send(());
                            let _ = release.await;
                        }
                        match watch_folders {
                            Some(folders) => handle.scan_known_files(folders, false).await,
                            None => {
                                handle
                                    .known_file_scan_incomplete
                                    .store(true, Ordering::Release);
                                if !handle.stopping.load(Ordering::Acquire) {
                                    handle.mark_known_file_watch_degraded();
                                }
                                handle.log_known_file_scan_event("known_file_scan_started");
                                handle.log_known_file_scan_event("known_file_scan_incomplete");
                            }
                        }
                    }));
                }
                Ok(runtime)
            }
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
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        if let Some(scan) = self.startup_scan.take() {
            scan.abort();
        }
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
