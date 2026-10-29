use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{CONTRACT, PairRequest};
use notryx_server::{DeleteNode, NodeSnapshot, Outcome, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

struct ServerProcess(std::process::Child);
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(args: &[&Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_notryx-server"))
        .args(args)
        .output()
        .unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                result.insert(relative, Vec::new());
                visit(root, &path, result);
            } else if metadata.is_file() {
                result.insert(relative, fs::read(path).unwrap());
            } else {
                panic!("unexpected fixture entry");
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[tokio::test]
async fn cold_restore_preserves_history_and_revokes_old_credentials() {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let source = root.path().join("source");
    let notes = source.join("notes");
    let state = source.join("state");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let config = source.join("server.toml");
    fs::write(&config, format!("[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n", source.join("admin.sock").display(), notes.display(), state.display())).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let status = workspace.auth_status().await.unwrap();
    let grant = workspace
        .issue_pairing(status.owner_principal_id, now)
        .await
        .unwrap();
    let device = workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            now,
        )
        .await
        .unwrap();
    let unused = workspace
        .issue_pairing(status.owner_principal_id, now)
        .await
        .unwrap();
    let original = b"\xef\xbb\xbf# synthetic\r\nbody\r\n";
    fs::write(notes.join("live.md"), original).unwrap();
    let live = workspace.adopt_root_document("live.md").await.unwrap();
    fs::write(notes.join("deleted.md"), b"deleted content\n").unwrap();
    let deleted = workspace.adopt_root_document("deleted.md").await.unwrap();
    let before_delete = NodeSnapshot::from(&deleted);
    let operation_id = Uuid::now_v7().to_string().parse().unwrap();
    let receipt = workspace
        .delete_node(DeleteNode {
            epoch: workspace.epoch(),
            stream_id: device.device_stream_id.parse().unwrap(),
            operation_seq: 1,
            operation_id,
            node_id: deleted.node_id,
            expected_node_etag: before_delete.etag().unwrap(),
        })
        .await
        .unwrap();
    let Outcome::Deleted(tombstone) = &receipt.outcome else {
        panic!("expected tombstone")
    };
    let tombstone = tombstone.clone();
    let info = workspace.server_info().await.unwrap();
    workspace.close().await.unwrap();
    fs::create_dir(notes.join(".obsidian")).unwrap();
    fs::write(notes.join(".obsidian/settings.json"), b"excluded bytes\n").unwrap();
    let backup = root.path().join("backup");
    let created = cli(&[Path::new("backup"), Path::new("--config"), &config, &backup]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(cli(&[Path::new("backup-verify"), &backup]).status.success());
    let source_before = snapshot(&source);
    let backup_before = snapshot(&backup);

    let restored = root.path().join("restored");
    let result = cli(&[Path::new("restore"), &backup, &restored]);
    assert!(
        result.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&backup), backup_before);
    assert_eq!(
        fs::read(restored.join("notes/.obsidian/settings.json")).unwrap(),
        b"excluded bytes\n"
    );
    let mut reopened = Workspace::open(&restored.join("notes"), &restored.join("state"))
        .await
        .unwrap();
    let restored_info = reopened.server_info().await.unwrap();
    assert_eq!(restored_info.workspace_id, info.workspace_id);
    assert_eq!(restored_info.root_id, info.root_id);
    assert_eq!(restored_info.head_seq, info.head_seq);
    assert_ne!(restored_info.workspace_epoch, info.workspace_epoch);
    let (restored_live, bytes) = reopened.read(live.node_id).await.unwrap();
    assert_eq!(restored_live, live);
    assert_eq!(bytes, original);
    assert_eq!(
        reopened.node_state(tombstone.node_id).await.unwrap(),
        tombstone.representation().unwrap()
    );
    assert!(
        reopened
            .authenticate(&device.device_token, now)
            .await
            .is_err()
    );
    assert!(
        reopened
            .pair(
                PairRequest {
                    protocol: CONTRACT.into(),
                    workspace_id: unused.workspace_id,
                    pairing_secret: unused.pairing_secret
                },
                now
            )
            .await
            .is_err()
    );
    reopened.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(restored.join("state/state.db"))
            .read_only(true),
    )
    .await
    .unwrap();
    let stored: String =
        sqlx::query_scalar("SELECT outcome FROM operation_receipts WHERE operation_id=?")
            .bind(operation_id.to_string())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).unwrap(),
        serde_json::to_value(&receipt.outcome).unwrap()
    );
    db.close().await.unwrap();
    assert_eq!(
        fs::read(
            restored
                .join("state/objects/sha256")
                .join(deleted.content_hash)
        )
        .unwrap(),
        b"deleted content\n"
    );
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&backup), backup_before);
    let restored_config = restored.join("config/server.toml");
    let cfg = notryx_server::config::Config::load(&restored_config).unwrap();
    assert_eq!(cfg.workspace.notes_root, restored.join("notes"));
    assert_eq!(cfg.workspace.state_root, restored.join("state"));
    assert_eq!(
        fs::read(restored.join("config/original-server.toml")).unwrap(),
        fs::read(&config).unwrap()
    );
    let _server = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .arg("serve")
            .arg("--config")
            .arg(&restored_config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..200 {
        if cfg.server.admin_socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        cfg.server.admin_socket.exists(),
        "restored server did not start"
    );
    let original_lock = fs::File::open(notes.join(".mdw-server.lock")).unwrap();
    rustix::fs::flock(
        &original_lock,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    drop(original_lock);
    drop(_server);
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&backup), backup_before);
}

#[tokio::test]
async fn cold_restore_rejects_existing_destination_and_damaged_backup() {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let source = root.path().join("source");
    let notes = source.join("notes");
    let state = source.join("state");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&notes).unwrap();
    fs::set_permissions(&notes, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(&state).unwrap();
    let config = source.join("server.toml");
    fs::write(&config, format!("[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n", source.join("admin.sock").display(), notes.display(), state.display())).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("document.md"), b"source bytes\n").unwrap();
    workspace.adopt_root_document("document.md").await.unwrap();
    workspace.close().await.unwrap();
    let backup = root.path().join("backup");
    assert!(
        cli(&[Path::new("backup"), Path::new("--config"), &config, &backup])
            .status
            .success()
    );
    let source_before = snapshot(&source);
    let backup_before = snapshot(&backup);
    let occupied = root.path().join("occupied");
    fs::create_dir(&occupied).unwrap();
    fs::write(occupied.join("sentinel"), b"leave untouched\n").unwrap();
    let result = cli(&[Path::new("restore"), &backup, &occupied]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("restore_target_exists"));
    assert_eq!(
        fs::read(occupied.join("sentinel")).unwrap(),
        b"leave untouched\n"
    );
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&backup), backup_before);

    for original_parent in [&notes, &state] {
        let nested = original_parent.join("restored");
        let result = cli(&[Path::new("restore"), &backup, &nested]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("restore_target_overlaps_source"));
        assert!(!nested.exists());
        assert_eq!(snapshot(&source), source_before);
        assert_eq!(snapshot(&backup), backup_before);
    }

    fs::write(backup.join("notes/document.md"), b"tampered\n").unwrap();
    let damaged_before = snapshot(&backup);
    let destination = root.path().join("restored");
    let result = cli(&[Path::new("restore"), &backup, &destination]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("backup_invalid"));
    assert!(!destination.exists());
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&backup), damaged_before);
}

#[tokio::test]
async fn cold_restore_rejects_missing_live_upload_staging() {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let source = root.path().join("source");
    let notes = source.join("notes");
    let state = source.join("state");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let config = source.join("server.toml");
    fs::write(&config, format!("[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n", source.join("admin.sock").display(), notes.display(), state.display())).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let status = workspace.auth_status().await.unwrap();
    let grant = workspace
        .issue_pairing(status.owner_principal_id, now)
        .await
        .unwrap();
    let device = workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            now,
        )
        .await
        .unwrap();
    workspace.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(state.join("state.db")),
    )
    .await
    .unwrap();
    let upload_id = Uuid::new_v4();
    let hash: String = Sha256::digest(b"ok")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    sqlx::query("INSERT INTO uploads(upload_id,device_id,status,expected_hash,declared_size,reserved_bytes,started_at,expires_at,content_hash,size) VALUES(?,?,'ready',?,2,2,?,?,?,2)")
        .bind(upload_id.to_string()).bind(device.device_id).bind(&hash)
        .bind(now).bind(now + 3600).bind(&hash).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    fs::write(
        state
            .join("staging")
            .join(format!("upload-{upload_id}.ready")),
        b"ok",
    )
    .unwrap();
    let backup = root.path().join("backup");
    let created = cli(&[Path::new("backup"), Path::new("--config"), &config, &backup]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let source_before = snapshot(&source);
    let staged_path = format!("state/staging/upload-{upload_id}.ready");
    fs::remove_file(backup.join(&staged_path)).unwrap();
    let manifest_path = backup.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let entries = manifest["entries"].as_array_mut().unwrap();
    let old_len = entries.len();
    entries.retain(|entry| entry["path"] != STANDARD.encode(staged_path.as_bytes()));
    assert_eq!(entries.len() + 1, old_len);
    let bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(&manifest_path, &bytes).unwrap();
    let marker: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    fs::write(backup.join("COMPLETE"), marker).unwrap();
    assert!(cli(&[Path::new("backup-verify"), &backup]).status.success());
    let backup_before = snapshot(&backup);
    let restored = root.path().join("restored");
    let result = cli(&[Path::new("restore"), &backup, &restored]);
    assert!(
        !result.status.success(),
        "missing active upload cannot be published"
    );
    assert!(!restored.exists());
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&backup), backup_before);
}

#[tokio::test]
async fn cold_restore_rejects_admin_socket_paths_longer_than_linux_limit() {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let source = root.path().join("source");
    let notes = source.join("notes");
    let state = source.join("state");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let config = source.join("server.toml");
    fs::write(&config, format!("[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n", source.join("admin.sock").display(), notes.display(), state.display())).unwrap();
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    workspace.close().await.unwrap();
    let backup = root.path().join("backup");
    let result = cli(&[Path::new("backup"), Path::new("--config"), &config, &backup]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let source_before = snapshot(&source);
    let backup_before = snapshot(&backup);
    let suffix = "/notryx-admin.sock".len();
    let name_len = 107usize
        .checked_sub(root.path().as_os_str().as_bytes().len() + 1 + suffix)
        .unwrap();
    assert!(name_len > 0 && name_len < 254);
    let exact = root.path().join("a".repeat(name_len));
    let result = cli(&[Path::new("restore"), &backup, &exact]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let restored_config = exact.join("config/server.toml");
    let config = notryx_server::config::Config::load(&restored_config).unwrap();
    assert_eq!(config.server.admin_socket.as_os_str().as_bytes().len(), 107);
    let server = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .arg("serve")
            .arg("--config")
            .arg(&restored_config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..200 {
        if config.server.admin_socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        config.server.admin_socket.exists(),
        "server must start at the socket path boundary"
    );
    drop(server);
    let over = root.path().join("b".repeat(name_len + 1));
    let result = cli(&[Path::new("restore"), &backup, &over]);
    assert!(
        !result.status.success(),
        "restore must reject an unusable socket path"
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("restore_admin_socket_path_too_long"));
    assert!(!over.exists());
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&backup), backup_before);
}
