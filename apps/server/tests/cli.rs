use notryx_server::config::Config;
use notryx_server::runtime::WorkspaceRuntime;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> (tempfile::TempDir, PathBuf, Config) {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let path = directory.path().join("server.toml");
    let text = format!(
        "[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n",
        directory.path().join("admin.sock").display(),
        notes.display(),
        state.display()
    );
    std::fs::write(&path, &text).unwrap();
    (directory, path, Config::from_toml(&text).unwrap())
}

async fn cli(path: &Path, command: &str, arguments: &[&str]) -> std::process::Output {
    let path = path.to_owned();
    let command = command.to_owned();
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .arg(command)
            .arg("--config")
            .arg(path)
            .args(arguments)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn json(output: std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn help_lists_offline_and_local_administration_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_notryx-server"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in [
        "migrate",
        "adopt",
        "admin",
        "create-principal",
        "set-role",
        "transfer-owner",
        "issue-pairing",
        "revoke-device",
    ] {
        assert!(help.contains(command), "missing command: {command}");
    }
}

#[test]
fn invalid_admin_inputs_are_rejected_before_reading_configuration_without_echoing_them() {
    for arguments in [
        vec![
            "admin",
            "--config",
            "/absent-notryx-config",
            "set-role",
            "invalid-private-id",
            "reader",
        ],
        vec![
            "admin",
            "--config",
            "/absent-notryx-config",
            "create-principal",
            "invalid-private-role",
        ],
        vec![
            "admin",
            "--config",
            "/absent-notryx-config",
            "revoke-device",
            "00000000-0000-7000-8000-000000000001",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("invalid_request"));
        assert!(!error.contains("invalid-private"));
        assert!(!error.contains("absent-notryx-config"));
    }
}

#[tokio::test]
async fn offline_adoption_and_migration_require_exclusive_access_and_preserve_bytes() {
    let (_directory, path, config) = fixture();
    assert!(cli(&path, "init", &[]).await.status.success());
    let original = b"\xef\xbb\xbf---\r\nsynthetic: true\r\n---\r\n# Kept bytes\r\n";
    std::fs::write(config.workspace.notes_root.join("Document.md"), original).unwrap();
    let runtime = WorkspaceRuntime::start(config.workspace.clone())
        .await
        .unwrap();
    for (command, arguments) in [("migrate", vec![]), ("adopt", vec!["Document.md"])] {
        let output = cli(&path, command, &arguments).await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("locked"));
    }
    runtime.shutdown().await.unwrap();
    assert!(cli(&path, "migrate", &[]).await.status.success());
    let adopted = json(cli(&path, "adopt", &["Document.md"]).await);
    let node_id = adopted["node_id"].as_str().unwrap().parse().unwrap();
    let mut workspace =
        notryx_server::Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
            .await
            .unwrap();
    assert_eq!(workspace.read(node_id).await.unwrap().1, original);
    workspace.close().await.unwrap();
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        original
    );
}

#[tokio::test]
async fn local_admin_cli_manages_roles_and_emits_pairing_only_for_explicit_issue() {
    use notryx_protocol::{AdminRequest, PairRequest};
    let (_directory, path, config) = fixture();
    WorkspaceRuntime::initialize(config.workspace.clone())
        .await
        .unwrap();
    let runtime = WorkspaceRuntime::start(config.workspace.clone())
        .await
        .unwrap();
    let handle = runtime.handle();
    let server =
        notryx_server::admin::AdminServer::start(&config.server.admin_socket, handle.clone())
            .await
            .unwrap();
    let status = json(cli(&path, "admin", &["status"]).await);
    let old_owner = status["owner_principal_id"].as_str().unwrap();
    let principal = json(cli(&path, "admin", &["create-principal", "editor"]).await);
    let principal_id = principal["principal_id"].as_str().unwrap();
    assert_eq!(
        json(cli(&path, "admin", &["set-role", principal_id, "reader"]).await)["kind"],
        "done"
    );
    let denied = cli(&path, "admin", &["set-role", old_owner, "reader"]).await;
    assert!(!denied.status.success());
    assert!(denied.stdout.is_empty());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("forbidden"));
    assert_eq!(
        json(cli(&path, "admin", &["transfer-owner", principal_id, "editor"]).await)["kind"],
        "done"
    );
    let status = json(cli(&path, "admin", &["status"]).await);
    assert_eq!(status["owner_principal_id"], principal_id);
    assert!(status.get("pairing_secret").is_none());
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let grant = json(cli(&path, "admin", &["issue-pairing", principal_id]).await);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires: u64 = grant["expires_at"].as_str().unwrap().parse().unwrap();
    assert!((before + 300..=now + 300).contains(&expires));
    let request = PairRequest {
        protocol: "mdw-sync/1.2".into(),
        workspace_id: grant["workspace_id"].as_str().unwrap().parse().unwrap(),
        pairing_secret: grant["pairing_secret"].as_str().unwrap().to_owned(),
    };
    let paired = handle.pair(request.clone()).await.unwrap();
    assert!(matches!(
        handle.pair(request).await,
        Err(notryx_server::runtime::RuntimeError::Auth(
            notryx_server::auth::AuthError::Unauthorized
        ))
    ));
    assert_eq!(
        json(cli(&path, "admin", &["revoke-device", &paired.device_id]).await)["kind"],
        "done"
    );
    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
    assert!(matches!(
        handle.admin(AdminRequest::Status {}).await,
        Err(notryx_server::runtime::RuntimeError::Closed)
    ));
}
