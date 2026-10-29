pub mod support;

use notryx_server::{
    SaveContent, Workspace, config::Config, http::RunningServer, runtime::WorkspaceRuntime,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{os::unix::fs::PermissionsExt, path::Path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const DEFAULT_BUDGET: u64 = 512 * 1024 * 1024;

async fn request(path: &Path, frame: &Value) -> Value {
    let mut connection = UnixStream::connect(path).await.unwrap();
    assert_eq!(
        connection.peer_cred().unwrap().uid(),
        rustix::process::geteuid().as_raw()
    );
    let mut body = serde_json::to_vec(frame).unwrap();
    body.push(b'\n');
    connection.write_all(&body).await.unwrap();
    let mut line = Vec::new();
    BufReader::new(connection)
        .read_until(b'\n', &mut line)
        .await
        .unwrap();
    serde_json::from_slice(&line).unwrap()
}

async fn measured_size(config: &Config) -> u64 {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.workspace.state_root.join("state.db"))
            .read_only(true),
    )
    .await
    .unwrap();
    let mut tx = db.begin().await.unwrap();
    let changes: Vec<(String, Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT seq,operation_id,committed_at FROM change_sets")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let node_changes: Vec<(String, String, String)> =
        sqlx::query_as("SELECT seq,node_id,post_state FROM node_changes")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let baseline: Vec<(String, String, String)> =
        sqlx::query_as("SELECT node_id,seq,post_state FROM journal_baseline")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    let changes_size: usize = changes
        .iter()
        .map(|(seq, operation_id, timestamp)| {
            seq.len()
                + operation_id.as_ref().map_or(0, String::len)
                + usize::from(timestamp.is_some()) * 8
        })
        .sum();
    let changes_nodes_size: usize = node_changes
        .iter()
        .map(|(seq, node, state)| seq.len() + node.len() + state.len())
        .sum();
    let baseline_size: usize = baseline
        .iter()
        .map(|(node, seq, state)| node.len() + seq.len() + state.len())
        .sum();
    (changes_size + changes_nodes_size + baseline_size)
        .try_into()
        .unwrap()
}

fn decimal(value: &Value, name: &str) -> u64 {
    let text = value[name].as_str().unwrap();
    let number: u64 = text.parse().unwrap();
    assert_eq!(text, number.to_string());
    number
}

fn diagnostic(value: Value) -> Value {
    assert_eq!(value["status"], "ok");
    assert_eq!(value["result"]["kind"], "journal_status");
    value["result"].clone()
}

#[tokio::test]
async fn journal_status_reports_fresh_logical_size_and_preserves_existing_admin_status() {
    let (directory, config) = support::initialized().await;
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let content = b"\xef\xbb\xbf# synthetic\n\xed\x95\x9c\xea\xb8\x80\n";
    std::fs::write(
        config.workspace.notes_root.join("Synthetic.md"),
        b"initial\n",
    )
    .unwrap();
    let document = workspace.adopt_root_document("Synthetic.md").await.unwrap();
    let stream_id = workspace.register_stream().await.unwrap();
    let receipt = workspace
        .save(SaveContent {
            epoch: workspace.epoch(),
            stream_id,
            operation_seq: 1,
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_content_rev: document.content_rev,
            content_hash: Sha256::digest(content)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: content.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, notryx_server::Outcome::Saved(_)));
    workspace.close().await.unwrap();
    let expected_before = measured_size(&config).await;
    assert!(expected_before > 0);
    let server = RunningServer::start(config.clone()).await.unwrap();
    let socket = &config.server.admin_socket;
    assert_eq!(
        std::fs::symlink_metadata(socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let status = request(socket, &json!({"command":"status"})).await;
    assert_eq!(status["status"], "ok");
    let status_result = status["result"].as_object().unwrap();
    assert_eq!(status_result.len(), 4);
    assert_eq!(status["result"]["kind"], "status");
    assert!(status_result.contains_key("workspace_id"));
    assert!(status_result.contains_key("workspace_epoch"));
    assert!(status_result.contains_key("owner_principal_id"));

    let result = diagnostic(request(socket, &json!({"command":"journal_status"})).await);
    assert_eq!(
        result["workspace_epoch"],
        status["result"]["workspace_epoch"]
    );
    assert_eq!(decimal(&result, "head_seq"), 2);
    assert_eq!(decimal(&result, "journal_floor_seq"), 0);
    assert_eq!(decimal(&result, "budget_bytes"), DEFAULT_BUDGET);
    assert_eq!(
        decimal(&result, "logical_bytes"),
        measured_size(&config).await
    );
    assert_eq!(result["budget_exceeded"], false);
    assert_eq!(result["accounting"], "sqlite_blob_lengths_v1");
    let invalid = request(
        socket,
        &json!({"command":"journal_status","extra":"synthetic-secret"}),
    )
    .await;
    assert_eq!(invalid["status"], "error");
    assert_eq!(invalid["code"], "invalid_request");
    assert!(
        !serde_json::to_string(&invalid)
            .unwrap()
            .contains("synthetic-secret")
    );
    let public = support::request(server.local_addr(), "GET", "/v1/journal-status", &[], &[]).await;
    assert_ne!(public.status, 200);
    let config_path = directory.path().join("synthetic-config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\nadmin_socket = \"{}\"\n[workspace]\nnotes_root = \"{}\"\nstate_root = \"{}\"\n",
            socket.display(),
            config.workspace.notes_root.display(),
            config.workspace.state_root.display()
        ),
    )
    .unwrap();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .args(["admin", "--config"])
            .arg(config_path)
            .arg("journal-status")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success(), "journal-status CLI must succeed");
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cli["kind"], "journal_status");
    assert_eq!(decimal(&cli, "logical_bytes"), measured_size(&config).await);
    server.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Synthetic.md")).unwrap(),
        content
    );
}

#[tokio::test]
async fn journal_status_reports_baseline_over_budget_without_removing_it() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    let socket = directory.path().join("admin.sock");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let text = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nadmin_socket = \"{}\"\n[workspace]\nnotes_root = \"{}\"\nstate_root = \"{}\"\n[sync]\njournal_budget_bytes = 1\n",
        socket.display(),
        notes.display(),
        state.display()
    );
    let config = Config::from_toml(&text).expect("positive journal budget must be configurable");
    WorkspaceRuntime::initialize(config.workspace.clone())
        .await
        .unwrap();
    let size_before = measured_size(&config).await;
    assert!(size_before > 1);
    let server = RunningServer::start(config.clone()).await.unwrap();
    let result = diagnostic(request(&socket, &json!({"command":"journal_status"})).await);
    assert_eq!(decimal(&result, "head_seq"), 0);
    assert_eq!(decimal(&result, "journal_floor_seq"), 0);
    assert_eq!(decimal(&result, "budget_bytes"), 1);
    assert_eq!(decimal(&result, "logical_bytes"), size_before);
    assert_eq!(result["budget_exceeded"], true);
    let ready = support::request(server.local_addr(), "GET", "/health/ready", &[], &[]).await;
    assert_eq!(ready.status, 200);
    server.shutdown().await.unwrap();
    let size_after = measured_size(&config).await;
    assert_eq!(size_after, size_before);
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    let after_restart = diagnostic(request(&socket, &json!({"command":"journal_status"})).await);
    assert_eq!(decimal(&after_restart, "logical_bytes"), size_before);
    assert_eq!(after_restart["budget_exceeded"], true);
    assert_eq!(decimal(&after_restart, "journal_floor_seq"), 0);
    restarted.shutdown().await.unwrap();
}
