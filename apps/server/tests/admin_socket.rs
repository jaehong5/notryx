use notryx_server::admin::AdminServer;
use notryx_server::config::WorkspaceConfig;
use notryx_server::runtime::WorkspaceRuntime;
use rustix::fs::{CWD, FlockOperation, Mode, flock, mkfifoat};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};

async fn fixture() -> (tempfile::TempDir, WorkspaceRuntime) {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let config = WorkspaceConfig {
        notes_root: directory.path().join("notes"),
        state_root: directory.path().join("state"),
    };
    std::fs::create_dir(&config.notes_root).unwrap();
    std::fs::create_dir(&config.state_root).unwrap();
    WorkspaceRuntime::initialize(config.clone()).await.unwrap();
    let runtime = WorkspaceRuntime::start(config).await.unwrap();
    (directory, runtime)
}

#[tokio::test]
async fn admin_socket_is_private_and_removed_after_shutdown() {
    let (directory, runtime) = fixture().await;
    let path = directory.path().join("admin.sock");
    let server = AdminServer::start(&path, runtime.handle()).await.unwrap();
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.mode() & 0o777, 0o600);
    let connection = tokio::net::UnixStream::connect(&path).await.unwrap();
    assert_eq!(
        connection.peer_cred().unwrap().uid(),
        rustix::process::geteuid().as_raw()
    );
    drop(connection);
    server.shutdown().await.unwrap();
    assert!(!path.exists());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn socket_lock_is_acquired_before_reclaiming_a_stale_socket() {
    let (directory, runtime) = fixture().await;
    let path = directory.path().join("admin.sock");
    let lock_path = directory.path().join("admin.sock.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    let stale = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let inode = std::fs::symlink_metadata(&path).unwrap().ino();
    drop(stale);

    assert!(
        AdminServer::start(&path, runtime.handle()).await.is_err(),
        "startup must reject an owned socket path before probing or reclaiming it"
    );
    assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), inode);
    drop(lock);
    let server = AdminServer::start(&path, runtime.handle()).await.unwrap();
    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn socket_lock_excludes_other_workspaces_until_shutdown_or_drop() {
    let (directory, first_runtime) = fixture().await;
    let (_other_directory, second_runtime) = fixture().await;
    let path = directory.path().join("admin.sock");
    let lock_path = directory.path().join("admin.sock.lock");
    let first = AdminServer::start(&path, first_runtime.handle())
        .await
        .unwrap();
    // A missing socket pathname must not let another workspace bypass its owner's lifetime.
    std::fs::remove_file(&path).unwrap();
    assert!(
        AdminServer::start(&path, second_runtime.handle())
            .await
            .is_err(),
        "a different workspace must not bind the same path while its owner holds the lock"
    );
    let metadata = std::fs::symlink_metadata(&lock_path).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(metadata.nlink(), 1);
    let inode = metadata.ino();

    drop(first);
    let second = AdminServer::start(&path, second_runtime.handle())
        .await
        .unwrap();
    second.shutdown().await.unwrap();
    assert!(!path.exists());
    assert_eq!(std::fs::symlink_metadata(&lock_path).unwrap().ino(), inode);
    let restarted = AdminServer::start(&path, first_runtime.handle())
        .await
        .unwrap();
    restarted.shutdown().await.unwrap();
    assert_eq!(std::fs::symlink_metadata(&lock_path).unwrap().ino(), inode);
    first_runtime.shutdown().await.unwrap();
    second_runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn socket_lock_rejects_unsafe_files_without_modifying_them() {
    let (directory, runtime) = fixture().await;
    let target = directory.path().join("keep.txt");
    std::fs::write(&target, b"must remain").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    for name in [
        "symlink",
        "hardlink",
        "directory",
        "fifo",
        "shared",
        "special",
    ] {
        let path = directory.path().join(format!("{name}.sock"));
        let lock_path = directory.path().join(format!("{name}.sock.lock"));
        match name {
            "symlink" => symlink(&target, &lock_path).unwrap(),
            "hardlink" => std::fs::hard_link(&target, &lock_path).unwrap(),
            "directory" => std::fs::create_dir(&lock_path).unwrap(),
            "fifo" => mkfifoat(CWD, &lock_path, Mode::RUSR | Mode::WUSR).unwrap(),
            "shared" | "special" => {
                std::fs::write(&lock_path, b"lock contents must remain").unwrap();
                let mode = if name == "shared" { 0o644 } else { 0o4600 };
                std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(mode))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = std::fs::symlink_metadata(&lock_path).unwrap();
        assert!(
            AdminServer::start(&path, runtime.handle()).await.is_err(),
            "unsafe {name} lockfile must be rejected"
        );
        assert!(!path.exists());
        let after = std::fs::symlink_metadata(&lock_path).unwrap();
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        if matches!(name, "shared" | "special") {
            assert_eq!(
                std::fs::read(&lock_path).unwrap(),
                b"lock contents must remain"
            );
        }
    }
    assert_eq!(std::fs::read(&target).unwrap(), b"must remain");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn socket_lock_is_released_after_startup_fails() {
    let (directory, runtime) = fixture().await;
    let path = directory.path().join("admin.sock");
    let lock_path = directory.path().join("admin.sock.lock");
    let active = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(AdminServer::start(&path, runtime.handle()).await.is_err());
    let lock = std::fs::File::open(&lock_path).unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    drop(lock);
    drop(active);
    let server = AdminServer::start(&path, runtime.handle()).await.unwrap();
    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_shutdown_closes_existing_connections_and_releases_the_socket_lock() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (directory, runtime) = fixture().await;
    let path = directory.path().join("admin.sock");
    let server = AdminServer::start(&path, runtime.handle()).await.unwrap();
    let mut waiting = tokio::net::UnixStream::connect(&path).await.unwrap();
    waiting.write_all(b"{\"command\":").await.unwrap();
    // A reply on the next connection confirms that the earlier connection was accepted.
    assert!(matches!(
        notryx_server::admin::request(&path, &notryx_protocol::AdminRequest::Status {})
            .await
            .unwrap(),
        notryx_protocol::AdminResponse::Ok { .. }
    ));
    let mut shutdown = Box::pin(server.shutdown());
    poll_fn(|context| {
        assert!(shutdown.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(shutdown);

    assert!(!path.exists());
    let lock = std::fs::File::open(directory.path().join("admin.sock.lock")).unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(std::time::Duration::from_secs(2), waiting.read(&mut byte))
        .await
        .expect(
            "cancelled shutdown must close accepted connections without waiting for drain timeout",
        );
    assert!(
        matches!(read, Ok(0))
            || read.is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
    );
    assert!(
        runtime
            .handle()
            .admin(notryx_protocol::AdminRequest::Status {})
            .await
            .is_ok()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn admin_socket_does_not_replace_files_links_or_an_active_socket() {
    let (directory, runtime) = fixture().await;
    let file = directory.path().join("keep.txt");
    let link = directory.path().join("link.sock");
    std::fs::write(&file, b"must remain").unwrap();
    symlink(&file, &link).unwrap();
    assert!(AdminServer::start(&file, runtime.handle()).await.is_err());
    assert!(AdminServer::start(&link, runtime.handle()).await.is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"must remain");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let socket = directory.path().join("active.sock");
    let original = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let inode = std::fs::symlink_metadata(&socket).unwrap().ino();
    assert!(AdminServer::start(&socket, runtime.handle()).await.is_err());
    assert_eq!(std::fs::symlink_metadata(&socket).unwrap().ino(), inode);
    drop(original);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn admin_socket_rejects_a_shared_parent_directory() {
    let (directory, runtime) = fixture().await;
    let parent = directory.path().join("shared");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        AdminServer::start(&parent.join("admin.sock"), runtime.handle())
            .await
            .is_err()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_socket_is_reclaimed_but_a_replaced_path_survives_shutdown() {
    let (directory, runtime) = fixture().await;
    let path = directory.path().join("stale.sock");
    let stale = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    let server = AdminServer::start(&path, runtime.handle()).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement must remain").unwrap();
    server.shutdown().await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"replacement must remain");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_admin_request_returns_only_a_static_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (directory, runtime) = fixture().await;
    let path = directory.path().join("admin.sock");
    let server = AdminServer::start(&path, runtime.handle()).await.unwrap();
    let mut connection = tokio::net::UnixStream::connect(&path).await.unwrap();
    connection
        .write_all(b"{\"command\":\"status\",\"unexpected\":\"synthetic-secret\"}\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        connection.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!String::from_utf8_lossy(&response).contains("synthetic-secret"));
    assert!(
        matches!(serde_json::from_slice::<notryx_protocol::AdminResponse>(&response).unwrap(), notryx_protocol::AdminResponse::Error { code } if code == "invalid_request")
    );
    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}
