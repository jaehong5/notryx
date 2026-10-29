pub mod support;

use notryx_server::{SaveContent, Workspace, http::RunningServer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const BUDGET: u64 = 512 * 1024 * 1024;
const RETENTION: u64 = 30 * 24 * 60 * 60;
const CLOCK: i64 = 1_800_000_000;

async fn diagnostic(path: &std::path::Path) -> Value {
    let mut stream = UnixStream::connect(path).await.unwrap();
    stream
        .write_all(b"{\"command\":\"journal_status\"}\n")
        .await
        .unwrap();
    let mut line = Vec::new();
    BufReader::new(stream)
        .read_until(b'\n', &mut line)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&line).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["result"]["kind"], "journal_status");
    value["result"].clone()
}

#[tokio::test]
async fn journal_status_floor_reports_retained_history_and_stays_correct_after_restart() {
    let (_directory, config) = support::initialized().await;
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    std::fs::write(
        config.workspace.notes_root.join("Synthetic.md"),
        b"before\n",
    )
    .unwrap();
    let document = workspace.adopt_root_document("Synthetic.md").await.unwrap();
    let epoch = workspace.epoch().to_string();
    workspace
        .retain_journal(RETENTION, BUDGET, CLOCK)
        .await
        .unwrap();
    assert_eq!(
        workspace
            .server_info()
            .await
            .unwrap()
            .journal_floor_seq
            .get(),
        0
    );
    let stream_id = workspace.register_stream().await.unwrap();
    let content = b"\xef\xbb\xbf# synthetic after retention\n";
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
    workspace
        .retain_journal(RETENTION, BUDGET, CLOCK + RETENTION as i64 + 2)
        .await
        .unwrap();
    let info = workspace.server_info().await.unwrap();
    assert_eq!(info.head_seq.get(), 2);
    assert_eq!(info.journal_floor_seq.get(), 1);
    workspace.close().await.unwrap();

    let server = RunningServer::start(config.clone()).await.unwrap();
    let status = diagnostic(&config.server.admin_socket).await;
    assert_eq!(status["workspace_epoch"], epoch);
    assert_eq!(status["head_seq"], "2");
    assert_eq!(status["journal_floor_seq"], "1");
    assert_eq!(status["accounting"], "sqlite_blob_lengths_v1");
    assert_eq!(status["budget_exceeded"], false);
    assert!(
        status["logical_bytes"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    server.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Synthetic.md")).unwrap(),
        content
    );

    let server = RunningServer::start(config.clone()).await.unwrap();
    let after_restart = diagnostic(&config.server.admin_socket).await;
    assert_eq!(after_restart["workspace_epoch"], epoch);
    assert_eq!(after_restart["head_seq"], "2");
    assert_eq!(after_restart["journal_floor_seq"], "1");
    assert_eq!(after_restart["budget_exceeded"], false);
    server.shutdown().await.unwrap();
}
