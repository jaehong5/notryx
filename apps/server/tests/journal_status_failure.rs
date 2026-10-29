#[allow(
    dead_code,
    reason = "This test uses only initialization from the shared HTTP test helpers."
)]
mod support;

use notryx_server::{Outcome, SaveContent, Workspace, config::Config, http::RunningServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use uuid::Uuid;

const INITIAL: &[u8] = b"# synthetic initial\r\n";
const SAVED: &[u8] = b"\xef\xbb\xbf# synthetic retained\r\n";

struct Fixture {
    _directory: tempfile::TempDir,
    config: Config,
    server: RunningServer,
}

async fn connect_db(config: &Config) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.workspace.state_root.join("state.db"))
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

impl Fixture {
    async fn new() -> Self {
        let (directory, config) = support::initialized().await;
        fs::write(config.workspace.notes_root.join("Synthetic.md"), INITIAL).unwrap();
        let mut workspace =
            Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
                .await
                .unwrap();
        let document = workspace.adopt_root_document("Synthetic.md").await.unwrap();
        let stream = workspace.register_stream().await.unwrap();
        let receipt = workspace
            .save(SaveContent {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: Sha256::digest(SAVED)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                bytes: SAVED.to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(receipt.outcome, Outcome::Saved(_)));
        workspace.close().await.unwrap();
        let server = RunningServer::start(config.clone()).await.unwrap();
        Self {
            _directory: directory,
            config,
            server,
        }
    }
}

async fn journal_status(path: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(path).await.unwrap();
        let mut request = serde_json::to_vec(&json!({"command":"journal_status"})).unwrap();
        request.push(b'\n');
        stream.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        BufReader::new(stream)
            .read_until(b'\n', &mut response)
            .await
            .unwrap();
        assert!(response.len() <= 64 * 1024);
        assert_eq!(response.last(), Some(&b'\n'));
        serde_json::from_slice(&response).unwrap()
    })
    .await
    .expect("synthetic admin status timed out")
}

fn object_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

#[derive(Debug, PartialEq, Eq)]
struct Preserved {
    workspace: Vec<String>,
    baseline: Option<Vec<String>>,
    nodes: Vec<String>,
    changes: Vec<String>,
    node_changes: Vec<String>,
    receipts: Vec<String>,
    streams: Vec<String>,
    note: Vec<u8>,
    objects: BTreeMap<PathBuf, Vec<u8>>,
}

async fn preserved(config: &Config) -> Preserved {
    let mut db = connect_db(config).await;
    let mut tx = db.begin().await.unwrap();
    let workspace = sqlx::query_scalar("SELECT json_array(singleton,workspace_id,epoch,root_id,head_seq,journal_floor_seq,journal_baseline_count,journal_baseline_hash) FROM workspaces")
        .fetch_all(&mut *tx).await.unwrap();
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='journal_baseline'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let baseline = if exists == 1 {
        Some(
            sqlx::query_scalar(
                "SELECT json_array(node_id,seq,post_state) FROM journal_baseline ORDER BY node_id",
            )
            .fetch_all(&mut *tx)
            .await
            .unwrap(),
        )
    } else {
        assert_eq!(exists, 0);
        None
    };
    let nodes = sqlx::query_scalar("SELECT json_array(node_id,parent_id,name,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,deleted) FROM nodes ORDER BY node_id")
        .fetch_all(&mut *tx).await.unwrap();
    let changes =
        sqlx::query_scalar("SELECT json_array(seq,operation_id) FROM change_sets ORDER BY seq")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let node_changes = sqlx::query_scalar(
        "SELECT json_array(seq,node_id,post_state) FROM node_changes ORDER BY seq,node_id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let receipts = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut *tx).await.unwrap();
    let streams = sqlx::query_scalar("SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id")
        .fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    Preserved {
        workspace,
        baseline,
        nodes,
        changes,
        node_changes,
        receipts,
        streams,
        note: fs::read(config.workspace.notes_root.join("Synthetic.md")).unwrap(),
        objects: object_files(&config.workspace.state_root.join("objects")),
    }
}

fn assert_error(response: &Value) {
    assert_eq!(
        response["status"], "error",
        "corrupt journal status must fail"
    );
    let code = response["code"]
        .as_str()
        .expect("a diagnostic failure code is required");
    assert!(!code.is_empty());
    assert_ne!(
        code, "invalid_request",
        "the journal_status command itself must be supported"
    );
    assert!(response.get("result").is_none());
}

#[tokio::test]
async fn journal_status_failure_valid_state_has_an_authenticated_read_only_diagnostic() {
    let fixture = Fixture::new().await;
    let before = preserved(&fixture.config).await;
    assert_eq!(before.note, SAVED);
    assert!(!before.receipts.is_empty());
    assert!(!before.objects.is_empty());
    let response = journal_status(&fixture.config.server.admin_socket).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["result"]["kind"], "journal_status");
    assert_eq!(preserved(&fixture.config).await, before);
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn journal_status_failure_invalid_floor_is_not_reported_as_healthy() {
    let fixture = Fixture::new().await;
    let mut db = connect_db(&fixture.config).await;
    let head: String = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert!(head.parse::<u64>().unwrap() < u64::MAX);
    let changed = sqlx::query("UPDATE workspaces SET journal_floor_seq=? WHERE singleton=1")
        .bind(u64::MAX.to_string())
        .execute(&mut db)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(changed, 1);
    db.close().await.unwrap();
    let before = preserved(&fixture.config).await;
    assert_error(&journal_status(&fixture.config.server.admin_socket).await);
    assert_eq!(preserved(&fixture.config).await, before);
    let mut db = connect_db(&fixture.config).await;
    let floor: String =
        sqlx::query_scalar("SELECT journal_floor_seq FROM workspaces WHERE singleton=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(floor, u64::MAX.to_string());
    db.close().await.unwrap();
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn journal_status_failure_sql_measurement_error_is_not_reported_as_zero() {
    let fixture = Fixture::new().await;
    let mut db = connect_db(&fixture.config).await;
    sqlx::query("DROP TABLE journal_baseline")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let before = preserved(&fixture.config).await;
    assert!(before.baseline.is_none());
    assert_eq!(before.note, SAVED);
    assert_error(&journal_status(&fixture.config.server.admin_socket).await);
    assert_eq!(preserved(&fixture.config).await, before);
    let mut db = connect_db(&fixture.config).await;
    let baseline_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='journal_baseline'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        baseline_exists, 0,
        "diagnostics must not recreate or hide the damaged table"
    );
    db.close().await.unwrap();
    fixture.server.shutdown().await.unwrap();
}
