use super::*;
use crate::config::WorkspaceConfig;
use crate::runtime::{WorkspaceRuntime, now};
use notryx_protocol::{CONTRACT, PairRequest};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn config(root: &Path) -> WorkspaceConfig {
    let config = WorkspaceConfig {
        notes_root: root.join("notes"),
        state_root: root.join("state"),
        uploads: Default::default(),
    };
    fs::create_dir(&config.notes_root).unwrap();
    fs::create_dir(&config.state_root).unwrap();
    config
}

fn target(root: &Path, name: &str) -> File {
    let path = root.join(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    File::open(path).unwrap()
}

fn control() -> Control {
    Control {
        deadline: Instant::now() + SNAPSHOT_LIMIT,
        cancelled: Arc::new(AtomicBool::new(false)),
        stopping: Arc::new(AtomicBool::new(false)),
        interrupt_during_query: false,
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
        .await
        .unwrap()
}

#[tokio::test]
async fn snapshot_keeps_head_and_ready_upload_bytes_after_live_changes() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    fs::write(config.notes_root.join("Before.md"), b"before\r\n").unwrap();
    workspace.adopt_root_document("Before.md").await.unwrap();
    let principal = workspace.auth_status().await.unwrap().owner_principal_id;
    let time = now().unwrap();
    let grant = workspace.issue_pairing(principal, time).await.unwrap();
    let device = workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            time,
        )
        .await
        .unwrap();
    let bytes = b"\0snapshot upload\xff\r\n";
    let mut upload = workspace
        .begin_upload(
            &device.device_token,
            digest(bytes),
            Some(bytes.len() as u64),
            time,
        )
        .await
        .unwrap();
    upload.writer.write_chunk(bytes).unwrap();
    let sealed = upload.writer.seal().unwrap();
    let uploaded = workspace
        .finish_upload(&device.device_token, upload.upload_id, sealed, time)
        .await
        .unwrap();
    let before_head = workspace.state.head_seq().await.unwrap().get().to_string();
    let snapshot = capture(
        &mut workspace,
        target(directory.path(), "snapshot"),
        control(),
        "limited".into(),
    )
    .await
    .unwrap();

    fs::write(config.notes_root.join("After.md"), b"after\n").unwrap();
    workspace.adopt_root_document("After.md").await.unwrap();
    workspace
        .cleanup_uploads(uploaded.expires_at + 1)
        .await
        .unwrap();
    assert!(
        !config
            .state_root
            .join(format!("staging/upload-{}.ready", uploaded.upload_id))
            .exists()
    );
    assert_ne!(
        workspace.state.head_seq().await.unwrap().get().to_string(),
        before_head
    );

    let mut db = connect(&directory.path().join("snapshot/state.db")).await;
    let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(head, before_head);
    let after_count: i64 = sqlx::query_scalar("SELECT count(*) FROM nodes WHERE name='After.md'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(after_count, 0);
    assert_eq!(snapshot.workspace_id, workspace.workspace_id().to_string());
    assert_eq!(snapshot.observation, "limited");
    assert_eq!(snapshot.ready_uploads.len(), 1);
    let (name, mut source) = snapshot.ready_uploads.into_iter().next().unwrap();
    assert_eq!(name, format!("upload-{}.ready", uploaded.upload_id));
    let mut actual = Vec::new();
    source.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, bytes);
    db.close().await.unwrap();
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn failed_snapshot_does_not_replace_target_or_stop_actor() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let workspace = Workspace::initialize(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    let root = workspace.state.root_id;
    workspace.close().await.unwrap();
    let runtime = WorkspaceRuntime::test_start_without_known_file_watch(config.clone())
        .await
        .unwrap();
    let destination = target(directory.path(), "snapshot");
    fs::write(
        directory.path().join("snapshot/state.db"),
        b"preserve existing",
    )
    .unwrap();
    assert!(runtime.handle().backup_snapshot(destination).await.is_err());
    assert_eq!(
        fs::read(directory.path().join("snapshot/state.db")).unwrap(),
        b"preserve existing"
    );
    fs::write(
        config.notes_root.join("After.md"),
        b"after failed snapshot\n",
    )
    .unwrap();
    runtime
        .handle()
        .reconcile_new_file(root, "After.md".into())
        .await
        .unwrap();
    assert!(runtime.handle().health().await.unwrap().ready);
    runtime
        .handle()
        .backup_snapshot(target(directory.path(), "retry"))
        .await
        .unwrap();
    let mut db = connect(&directory.path().join("retry/state.db")).await;
    let after_count: i64 = sqlx::query_scalar("SELECT count(*) FROM nodes WHERE name='After.md'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(after_count, 1);
    db.close().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupted_vacuum_leaves_live_connection_usable() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    let mut interrupted = control();
    interrupted.interrupt_during_query = true;
    assert_eq!(
        capture(
            &mut workspace,
            target(directory.path(), "interrupted"),
            interrupted,
            "limited".into()
        )
        .await
        .unwrap_err(),
        RuntimeError::Backup("backup_cancelled")
    );
    fs::write(config.notes_root.join("After.md"), b"after interruption\n").unwrap();
    workspace.adopt_root_document("After.md").await.unwrap();
    capture(
        &mut workspace,
        target(directory.path(), "retry"),
        control(),
        "limited".into(),
    )
    .await
    .unwrap();
    assert!(workspace.ready());
    workspace.close().await.unwrap();
}
