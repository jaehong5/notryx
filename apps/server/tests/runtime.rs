use notryx_server::Workspace;
use notryx_server::config::{Config, ConfigError, ServerConfig, WorkspaceConfig};
use notryx_server::http::RunningServer;
use notryx_server::runtime::{RuntimeError, WorkspaceRuntime};
use notryx_server::{Error, SaveContent};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

async fn initialized() -> (tempfile::TempDir, Config) {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    Workspace::initialize(&notes, &state)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes,
            state_root: state,
            uploads: Default::default(),
        },
    };
    (directory, config)
}

async fn get(address: SocketAddr, path: &'static str) -> String {
    tokio::task::spawn_blocking(move || {
        let mut stream =
            std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    })
    .await
    .unwrap()
}

#[test]
fn configuration_defaults_to_loopback_and_rejects_unknown_fields() {
    let minimal = "[workspace]\nnotes_root='/tmp/notes'\nstate_root='/tmp/state'\n";
    let config = Config::from_toml(minimal).unwrap();
    assert_eq!(
        config.server.listen,
        "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
    );
    assert!(matches!(
        Config::from_toml(&format!("unexpected=true\n{minimal}")),
        Err(ConfigError::Invalid)
    ));
    assert!(matches!(
        Config::from_toml(&format!("{minimal}state_rooot='/tmp/typo'\n")),
        Err(ConfigError::Invalid)
    ));
    assert!(matches!(
        Config::from_toml(&format!("[server]\nlitsen='0.0.0.0:8080'\n{minimal}")),
        Err(ConfigError::Invalid)
    ));
}

#[tokio::test]
async fn serving_workspace_keeps_health_public_protects_api_and_releases_locks() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone())
        .await
        .expect("start must open the initialized workspace and bind HTTP");
    let live = get(server.local_addr(), "/health/live").await;
    let ready = get(server.local_addr(), "/health/ready").await;
    let inventory = get(server.local_addr(), "/v1/nodes").await;
    let private = get(server.local_addr(), "/v1/server-info").await;
    assert!(live.starts_with("HTTP/1.1 200"));
    assert!(live.ends_with("live\n"));
    assert!(ready.starts_with("HTTP/1.1 200"));
    assert!(ready.ends_with("ready\n"));
    assert!(inventory.starts_with("HTTP/1.1 401"));
    assert!(private.starts_with("HTTP/1.1 401"));
    assert!(!live.contains(config.workspace.notes_root.to_str().unwrap()));
    server.shutdown().await.unwrap();
    Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_or_corrupt_stores_fail_startup_without_initializing_or_overwriting_files() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: directory.path().join("missing-notes"),
            state_root: directory.path().join("missing-state"),
            uploads: Default::default(),
        },
    };
    assert!(RunningServer::start(config.clone()).await.is_err());
    assert!(!config.workspace.notes_root.exists());
    assert!(!config.workspace.state_root.exists());
    let (_directory, config) = initialized().await;
    let database = config.workspace.state_root.join("state.db");
    let marker = std::fs::read(config.workspace.notes_root.join(".mdw-workspace.json")).unwrap();
    std::fs::write(&database, b"damaged store: preserve original evidence").unwrap();
    let original = std::fs::read(&database).unwrap();
    assert!(RunningServer::start(config.clone()).await.is_err());
    assert_eq!(std::fs::read(database).unwrap(), original);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join(".mdw-workspace.json")).unwrap(),
        marker
    );
}

#[tokio::test]
async fn quarantined_recovery_keeps_liveness_but_readiness_is_unavailable() {
    let (_directory, config) = initialized().await;
    let path = config.workspace.notes_root.join("Document.md");
    std::fs::write(&path, b"before").unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    let stream = workspace.register_stream().await.unwrap();
    let mut fault = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_finalize BEFORE UPDATE OF content_rev ON nodes BEGIN SELECT RAISE(ABORT, 'injected failure'); END;")
        .execute(&mut fault).await.unwrap();
    let proposed = b"proposed".to_vec();
    let request = SaveContent {
        epoch: workspace.epoch(),
        stream_id: stream,
        operation_seq: 1,
        operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
        node_id: document.node_id,
        expected_content_rev: document.content_rev,
        content_hash: Sha256::digest(&proposed)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        bytes: proposed,
    };
    assert_eq!(workspace.save(request).await, Err(Error::Storage));
    sqlx::query("DROP TRIGGER fail_finalize")
        .execute(&mut fault)
        .await
        .unwrap();
    fault.close().await.unwrap();
    workspace.close().await.unwrap();
    let private = b"synthetic private content must never enter health responses";
    std::fs::write(&path, private).unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let live = get(server.local_addr(), "/health/live").await;
    let ready = get(server.local_addr(), "/health/ready").await;
    assert!(live.starts_with("HTTP/1.1 200"));
    assert!(ready.starts_with("HTTP/1.1 503"));
    assert!(ready.ends_with("not_ready\n"));
    assert!(!ready.contains("synthetic private"));
    assert!(!ready.contains(config.workspace.state_root.to_str().unwrap()));
    assert_eq!(std::fs::read(&path).unwrap(), private);
    server.shutdown().await.unwrap();
    let reopened = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    assert!(!reopened.ready());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_handle_clones_and_releases_both_workspace_locks() {
    let (_directory, config) = initialized().await;
    let runtime = WorkspaceRuntime::start(config.workspace.clone())
        .await
        .unwrap();
    let handle = runtime.handle();
    assert!(handle.health().await.unwrap().ready);
    assert!(matches!(
        WorkspaceRuntime::start(config.workspace.clone()).await,
        Err(RuntimeError::Workspace(Error::Locked))
    ));
    runtime.shutdown().await.unwrap();
    assert_eq!(handle.health().await, Err(RuntimeError::Closed));
    Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn owner_drop_and_listener_failure_do_not_leave_workspace_locks_behind() {
    let (_directory, mut config) = initialized().await;
    let runtime = WorkspaceRuntime::start(config.workspace.clone())
        .await
        .unwrap();
    let handle = runtime.handle();
    drop(runtime);
    assert_eq!(handle.health().await, Err(RuntimeError::Closed));
    let reopened = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match Workspace::open(&config.workspace.notes_root, &config.workspace.state_root).await
            {
                Ok(workspace) => break workspace,
                Err(Error::Locked) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(error) => panic!("unexpected open failure: {error}"),
            }
        }
    })
    .await
    .unwrap();
    reopened.close().await.unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.server.listen = occupied.local_addr().unwrap();
    assert!(matches!(
        RunningServer::start(config.clone()).await,
        Err(notryx_server::http::ServerError::Listen)
    ));
    Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[test]
fn cli_separates_initialization_from_serve_and_handles_termination_signals() {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    for signal in [rustix::process::Signal::TERM, rustix::process::Signal::INT] {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&state).unwrap();
        let config = directory.path().join("server.toml");
        std::fs::write(&config, format!("[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n", directory.path().join("admin.sock").display(), notes.display(), state.display())).unwrap();
        let binary = env!("CARGO_BIN_EXE_notryx-server");
        let before_init = Command::new(binary)
            .args(["serve", "--config"])
            .arg(&config)
            .output()
            .unwrap();
        assert!(!before_init.status.success());
        assert!(!notes.join(".mdw-workspace.json").exists());
        assert!(!state.join("state.db").exists());
        let initialized = Command::new(binary)
            .args(["init", "--config"])
            .arg(&config)
            .output()
            .unwrap();
        assert!(
            initialized.status.success(),
            "explicit init failed: {}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        let mut child = Command::new(binary)
            .args(["serve", "--config"])
            .arg(&config)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if line == "server_started" {
                    let _ = sender.send(());
                }
            }
        });
        let started = receiver.recv_timeout(Duration::from_secs(15));
        if started.is_err() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            reader.join().unwrap();
            panic!(
                "serve did not start: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let sent = rustix::process::kill_process(rustix::process::Pid::from_child(&child), signal);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        reader.join().unwrap();
        assert!(sent.is_ok(), "failed to send {signal:?}: {sent:?}");
        assert!(
            status.is_some_and(|status| status.success()),
            "graceful termination failed for {signal:?}"
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                Workspace::open(&notes, &state)
                    .await
                    .unwrap()
                    .close()
                    .await
                    .unwrap();
            });
    }
}

#[tokio::test]
async fn incomplete_http_headers_do_not_hold_workspace_locks_during_shutdown() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let connection = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    connection.writable().await.unwrap();
    connection
        .try_write(b"GET /health/ready HTTP/1.1\r\nHost: ")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let outcome = tokio::time::timeout(Duration::from_secs(7), server.shutdown())
        .await
        .expect("HTTP shutdown must have a bounded drain");
    assert!(matches!(
        outcome,
        Ok(()) | Err(notryx_server::http::ServerError::ShutdownTimeout)
    ));
    let peer_closed =
        match tokio::time::timeout(Duration::from_secs(1), connection.readable()).await {
            Ok(Ok(())) => matches!(connection.try_read(&mut [0_u8; 1]), Ok(0)),
            _ => false,
        };
    println!("incomplete-header shutdown: {outcome:?}; peer_closed={peer_closed}");
    assert!(peer_closed, "shutdown must close accepted connections");
    Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    drop(connection);
}
