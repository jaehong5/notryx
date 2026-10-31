use crate::runtime::WorkspaceHandle;
use notryx_protocol::{AdminRequest, AdminResponse};
use rustix::fs::{FlockOperation, Mode, OFlags, flock};
use std::fs::File;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

const MAX_FRAME_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONNECTIONS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminError {
    Io,
    Locked,
    UnsafePath,
    Peer,
    Protocol,
    Timeout,
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "admin_{self:?}")
    }
}

impl std::error::Error for AdminError {}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketGuard {
    fn capture(path: &Path) -> Result<Self, AdminError> {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| AdminError::Io)?;
        Ok(Self {
            path: path.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        // The path lock serializes server lifetimes; inode checks also preserve unrelated replacements.
        if std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn validate_parent(path: &Path) -> Result<(), AdminError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(AdminError::UnsafePath);
    }
    let parent = path.parent().ok_or(AdminError::UnsafePath)?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|_| AdminError::UnsafePath)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || std::fs::canonicalize(parent).map_err(|_| AdminError::UnsafePath)? != parent
    {
        return Err(AdminError::UnsafePath);
    }
    Ok(())
}

fn validate_socket(path: &Path) -> Result<std::fs::Metadata, AdminError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| AdminError::UnsafePath)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(AdminError::UnsafePath);
    }
    Ok(metadata)
}

fn lock_socket_path(path: &Path) -> Result<File, AdminError> {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    // Never unlink this file: every server lifetime must contend on the same inode.
    let file = File::from(
        rustix::fs::open(
            &lock_path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| AdminError::UnsafePath)?,
    );
    let metadata = file.metadata().map_err(|_| AdminError::Io)?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(AdminError::UnsafePath);
    }
    flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error == rustix::io::Errno::WOULDBLOCK {
            AdminError::Locked
        } else {
            AdminError::Io
        }
    })?;
    let current = std::fs::symlink_metadata(&lock_path).map_err(|_| AdminError::UnsafePath)?;
    if !current.is_file()
        || current.dev() != metadata.dev()
        || current.ino() != metadata.ino()
        || current.nlink() != 1
    {
        return Err(AdminError::UnsafePath);
    }
    Ok(file)
}

fn reclaim_stale_socket(path: &Path, original: &std::fs::Metadata) -> Result<(), AdminError> {
    // The probe yields; only the socket observed before it may be reclaimed.
    drop(SocketGuard {
        path: path.into(),
        device: original.dev(),
        inode: original.ino(),
    });
    Ok(())
}

fn validate_peer(stream: &UnixStream) -> Result<(), AdminError> {
    if stream.peer_cred().map_err(|_| AdminError::Peer)?.uid()
        != rustix::process::geteuid().as_raw()
    {
        return Err(AdminError::Peer);
    }
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, AdminError> {
    let mut frame = Vec::new();
    BufReader::new(stream)
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut frame)
        .await
        .map_err(|_| AdminError::Io)?;
    if frame.len() > MAX_FRAME_BYTES || frame.last() != Some(&b'\n') {
        return Err(AdminError::Protocol);
    }
    Ok(frame)
}

async fn write_frame(
    stream: &mut UnixStream,
    message: &impl serde::Serialize,
) -> Result<(), AdminError> {
    let mut frame = serde_json::to_vec(message).map_err(|_| AdminError::Protocol)?;
    frame.push(b'\n');
    if frame.len() > MAX_FRAME_BYTES {
        return Err(AdminError::Protocol);
    }
    stream.write_all(&frame).await.map_err(|_| AdminError::Io)
}

async fn connection(mut stream: UnixStream, handle: WorkspaceHandle) -> Result<(), AdminError> {
    validate_peer(&stream)?;
    let frame = tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| AdminError::Timeout)??;
    let response = match serde_json::from_slice::<AdminRequest>(&frame) {
        Ok(request) => {
            match tokio::time::timeout(command_timeout(&request), handle.admin(request)).await {
                Ok(Ok(result)) => AdminResponse::Ok { result },
                Ok(Err(error)) => AdminResponse::Error {
                    code: error.code().into(),
                },
                Err(_) => return Err(AdminError::Timeout),
            }
        }
        Err(_) => AdminResponse::Error {
            code: "invalid_request".into(),
        },
    };
    tokio::time::timeout(REQUEST_TIMEOUT, write_frame(&mut stream, &response))
        .await
        .map_err(|_| AdminError::Timeout)?
}

fn command_timeout(request: &AdminRequest) -> Duration {
    match request {
        AdminRequest::OnlineBackup { .. } => Duration::from_secs(30 * 60 + 30),
        AdminRequest::RecoveryResolve { .. } => RECOVERY_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    }
}

pub async fn request(path: &Path, request: &AdminRequest) -> Result<AdminResponse, AdminError> {
    validate_parent(path)?;
    validate_socket(path)?;
    let mut stream = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|_| AdminError::Io)?;
        validate_peer(&stream)?;
        write_frame(&mut stream, request).await?;
        Ok::<_, AdminError>(stream)
    })
    .await
    .map_err(|_| AdminError::Timeout)??;
    let response = tokio::time::timeout(command_timeout(request), read_frame(&mut stream))
        .await
        .map_err(|_| AdminError::Timeout)??;
    serde_json::from_slice(&response).map_err(|_| AdminError::Protocol)
}

pub struct AdminServer {
    // Fields drop in declaration order, so socket cleanup must precede unlocking.
    _guard: SocketGuard,
    _lock: File,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), AdminError>>>,
}

impl AdminServer {
    pub async fn start(path: &Path, handle: WorkspaceHandle) -> Result<Self, AdminError> {
        validate_parent(path)?;
        let lock = lock_socket_path(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(_) => {
                let original = validate_socket(path)?;
                let probe =
                    tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(path)).await;
                match probe {
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                        reclaim_stale_socket(path, &original)?;
                    }
                    _ => return Err(AdminError::UnsafePath),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(AdminError::Io),
        }
        let listener = UnixListener::bind(path).map_err(|_| AdminError::Io)?;
        let guard = SocketGuard::capture(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| AdminError::Io)?;
        validate_socket(path)?;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(serve(listener, handle, stopped));
        Ok(Self {
            _guard: guard,
            _lock: lock,
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub async fn shutdown(mut self) -> Result<(), AdminError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.as_mut() {
            task.await.map_err(|_| AdminError::Io)??;
        }
        self.task.take();
        Ok(())
    }
}

async fn serve(
    listener: UnixListener,
    handle: WorkspaceHandle,
    mut stopped: oneshot::Receiver<()>,
) -> Result<(), AdminError> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => break,
            _ = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                match accepted {
                    Ok((stream, _)) => {
                        let handle = handle.clone();
                        connections.spawn(async move { let _ = connection(stream, handle).await; });
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        }
    }
    drop(listener);
    if tokio::time::timeout(REQUEST_TIMEOUT, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    Ok(())
}

impl Drop for AdminServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_probe_does_not_delete_a_replacement_listener() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let path = directory.path().join("admin.sock");
        let stale = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let original = validate_socket(&path).unwrap();
        drop(stale);
        assert_eq!(
            UnixStream::connect(&path).await.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionRefused
        );
        // An unrelated same-UID process may replace the path without taking the server lock.
        std::fs::rename(&path, directory.path().join("old.sock")).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        reclaim_stale_socket(&path, &original).unwrap();
        assert!(
            UnixStream::connect(&path).await.is_ok(),
            "a replacement listener must remain reachable"
        );
        drop(replacement);
    }
}
