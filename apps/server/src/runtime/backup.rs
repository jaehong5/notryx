use super::{Command, RuntimeError, WorkspaceHandle};
use crate::Workspace;
use rustix::fs::{self as rfs, Mode, OFlags, ResolveFlags};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const SNAPSHOT_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) notes: File,
    pub(crate) state: File,
    pub(crate) workspace_id: String,
    pub(crate) ready_uploads: Vec<(String, File)>,
    pub(crate) observation: String,
}

pub(super) struct Control {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    #[cfg(test)]
    interrupt_during_query: bool,
}

impl Control {
    fn check(&self) -> Result<(), RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            Err(RuntimeError::Closed)
        } else if self.cancelled.load(Ordering::Acquire) {
            Err(RuntimeError::Backup("backup_cancelled"))
        } else if Instant::now() >= self.deadline {
            Err(RuntimeError::Backup("backup_snapshot_timeout"))
        } else {
            Ok(())
        }
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl WorkspaceHandle {
    pub(crate) async fn backup_snapshot(
        &self,
        target_state: File,
    ) -> Result<Snapshot, RuntimeError> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(Arc::clone(&cancelled));
        let control = Control {
            deadline: Instant::now() + SNAPSHOT_LIMIT,
            cancelled,
            stopping: Arc::clone(&self.stopping),
            #[cfg(test)]
            interrupt_during_query: false,
        };
        let observation = format!(
            "committed_observed_state_only;scan_incomplete={};watch_degraded={};unobserved_and_excluded_files_not_included",
            self.known_file_scan_incomplete.load(Ordering::Acquire),
            self.known_file_watch_degraded.load(Ordering::Acquire),
        );
        let (reply, response) = oneshot::channel();
        self.enqueue(Command::BackupSnapshot {
            target_state,
            control,
            observation,
            reply,
        })?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }
}

pub(super) async fn capture(
    workspace: &mut Workspace,
    target_state: File,
    control: Control,
    observation: String,
) -> Result<Snapshot, RuntimeError> {
    control.check()?;
    if !workspace.ready() {
        return Err(RuntimeError::Workspace(crate::Error::RecoveryRequired));
    }
    workspace
        .verify_mutation_root()
        .map_err(RuntimeError::Workspace)?;
    workspace
        .files
        .validate_state_database()
        .map_err(crate::Error::from)
        .map_err(RuntimeError::Workspace)?;
    let (notes, state) = workspace
        .files
        .backup_root_handles()
        .map_err(crate::Error::from)
        .map_err(RuntimeError::Workspace)?;
    let metadata = target_state.metadata().map_err(io_error)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(RuntimeError::Backup("backup_invalid"));
    }
    let target = File::from(
        rfs::openat(
            &target_state,
            "state.db",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| io_error(error.into()))?,
    );
    let source_path = format!("/proc/self/fd/{}/state.db", state.as_raw_fd());
    let target_path = format!("/proc/self/fd/{}/state.db", target_state.as_raw_fd());
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(source_path)
            .read_only(true)
            .busy_timeout(Duration::from_millis(100)),
    )
    .await
    .map_err(db_error)?;
    let cancelled = Arc::clone(&control.cancelled);
    let stopping = Arc::clone(&control.stopping);
    let deadline = control.deadline;
    #[cfg(test)]
    let interrupt = control.interrupt_during_query;
    #[cfg(test)]
    let operations = if interrupt { 1 } else { 1000 };
    #[cfg(not(test))]
    let operations = 1000;
    db.lock_handle()
        .await
        .map_err(db_error)?
        .set_progress_handler(operations, move || {
            #[cfg(test)]
            if interrupt {
                cancelled.store(true, Ordering::Release);
            }
            !cancelled.load(Ordering::Acquire)
                && !stopping.load(Ordering::Acquire)
                && Instant::now() < deadline
        });
    // Dropping a SQLx future does not stop its SQLite worker. Await interruption before releasing admission.
    let copied = sqlx::query("VACUUM INTO ?")
        .bind(target_path)
        .execute(&mut db)
        .await;
    let closed = db.close().await;
    control.check()?;
    copied.map_err(db_error)?;
    closed.map_err(db_error)?;
    target.sync_all().map_err(io_error)?;
    target_state.sync_all().map_err(io_error)?;

    let mut ready_uploads = Vec::new();
    for upload in workspace
        .state
        .uploads()
        .await
        .map_err(RuntimeError::Workspace)?
    {
        if upload.status != "ready" {
            continue;
        }
        control.check()?;
        let name = format!("upload-{}.ready", upload.id);
        let file = File::from(
            rfs::openat2(
                &state,
                format!("staging/{name}"),
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH
                    | ResolveFlags::NO_SYMLINKS
                    | ResolveFlags::NO_MAGICLINKS
                    | ResolveFlags::NO_XDEV,
            )
            .map_err(|error| io_error(error.into()))?,
        );
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.nlink() != 1 || Some(metadata.len()) != upload.size {
            return Err(RuntimeError::Backup("backup_invalid"));
        }
        // Cleanup may unlink a ready upload once admission resumes; this descriptor keeps its bytes alive.
        ready_uploads.push((name, file));
    }
    control.check()?;
    workspace
        .verify_mutation_root()
        .map_err(RuntimeError::Workspace)?;
    Ok(Snapshot {
        notes,
        state,
        workspace_id: workspace.workspace_id().to_string(),
        ready_uploads,
        observation,
    })
}

fn db_error(error: sqlx::Error) -> RuntimeError {
    RuntimeError::Workspace(crate::state::db_error(error))
}

fn io_error(error: std::io::Error) -> RuntimeError {
    if error.raw_os_error().is_some_and(|code| {
        code == rustix::io::Errno::NOSPC.raw_os_error()
            || code == rustix::io::Errno::DQUOT.raw_os_error()
    }) {
        RuntimeError::Workspace(crate::Error::NoSpace)
    } else {
        RuntimeError::Workspace(crate::Error::Storage)
    }
}

#[cfg(test)]
#[path = "backup_tests.rs"]
mod tests;
