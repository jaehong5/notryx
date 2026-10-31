use notryx_server::{Workspace, config::Config, http::RunningServer};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Output};

async fn cli(arguments: Vec<PathBuf>) -> Output {
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .args(arguments)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn succeeded(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn online_backup_restores_committed_bytes_without_stopping_server() {
    let directory = tempfile::Builder::new()
        .prefix("notryx-online-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let root = directory.path();
    let notes = root.join("notes");
    let state = root.join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let config_path = root.join("server.toml");
    fs::write(&config_path, format!(
        "[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n",
        root.join("admin.sock").display(), notes.display(), state.display()
    )).unwrap();
    let config = Config::load(&config_path).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let original = b"\xef\xbb\xbf# Synthetic\r\n\r\nPreserve bytes.\r\n";
    fs::write(notes.join("Original.md"), original).unwrap();
    let document = workspace.adopt_root_document("Original.md").await.unwrap();
    let epoch = workspace.epoch();
    let id = workspace.workspace_id();
    workspace.close().await.unwrap();
    fs::create_dir(notes.join(".obsidian")).unwrap();
    fs::write(notes.join(".obsidian/excluded.json"), b"{}\n").unwrap();
    let server = RunningServer::start(config).await.unwrap();
    let backup = root.join("backup");
    succeeded(
        cli(vec![
            "backup-online".into(),
            "--config".into(),
            config_path.clone(),
            backup.clone(),
        ])
        .await,
    );
    succeeded(
        cli(vec![
            "healthcheck".into(),
            "--address".into(),
            server.local_addr().to_string().into(),
        ])
        .await,
    );
    assert_eq!(
        fs::read(backup.join("notes/Original.md")).unwrap(),
        original
    );
    assert!(!backup.join("notes/.obsidian").exists());
    succeeded(cli(vec!["backup-verify".into(), backup.clone()]).await);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    assert!(
        manifest["observation"]
            .as_str()
            .unwrap()
            .contains("committed")
    );
    assert_eq!(manifest["workspace_id"], id.to_string());
    assert_eq!(manifest["epoch"], epoch.to_string());
    let retry = cli(vec![
        "backup-online".into(),
        "--config".into(),
        config_path,
        backup.clone(),
    ])
    .await;
    assert!(!retry.status.success());
    assert!(String::from_utf8_lossy(&retry.stderr).contains("backup_target_exists"));
    succeeded(
        cli(vec![
            "healthcheck".into(),
            "--address".into(),
            server.local_addr().to_string().into(),
        ])
        .await,
    );
    succeeded(cli(vec!["backup-verify".into(), backup.clone()]).await);
    let restored = root.join("restored");
    succeeded(cli(vec!["restore".into(), backup.clone(), restored.clone()]).await);
    assert_eq!(
        fs::read(restored.join("notes/Original.md")).unwrap(),
        original
    );
    let mut copy = Workspace::open(&restored.join("notes"), &restored.join("state"))
        .await
        .unwrap();
    assert_eq!(copy.workspace_id(), id);
    assert_ne!(copy.epoch(), epoch);
    let (restored_document, bytes) = copy.read(document.node_id).await.unwrap();
    assert_eq!(restored_document.content_rev, document.content_rev);
    assert_eq!(bytes, original);
    copy.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(backup.join("state/state.db"))
            .read_only(true),
    )
    .await
    .unwrap();
    let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(manifest["head_seq"], head);
    db.close().await.unwrap();
    server.shutdown().await.unwrap();
    assert_eq!(fs::read(notes.join("Original.md")).unwrap(), original);
    assert_eq!(
        fs::read(notes.join(".obsidian/excluded.json")).unwrap(),
        b"{}\n"
    );
}
