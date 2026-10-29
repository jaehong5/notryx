pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{PairResponse, ServerInfo};
use notryx_server::{Error, Workspace, config::Config, http::RunningServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::net::SocketAddr;
use support::{HttpResponse, initialized, pairing_request, post_json, request};

const RETENTION_SECONDS: u64 = 30 * 24 * 60 * 60;
const START: i64 = 1_800_000_000;
const BUDGET: u64 = 512 * 1024 * 1024;
const CONTENT: &[u8] = b"\xef\xbb\xbf# synthetic\ncontent\n";

async fn retain_journal(
    workspace: &mut Workspace,
    retention_seconds: u64,
    budget_bytes: u64,
    now_seconds: i64,
) -> Result<(), Error> {
    workspace
        .retain_journal(retention_seconds, budget_bytes, now_seconds)
        .await
}

async fn server(config: &Config) -> RunningServer {
    RunningServer::start(config.clone()).await.unwrap()
}

async fn pair(config: &Config, server: &RunningServer) -> PairResponse {
    let pairing = pairing_request(config).await.0;
    let response = post_json(server.local_addr(), &serde_json::to_vec(&pairing).unwrap()).await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).unwrap()
}

async fn get(address: SocketAddr, device: &PairResponse, path: &str) -> HttpResponse {
    request(
        address,
        "GET",
        path,
        &[("Authorization", &format!("Bearer {}", device.device_token))],
        &[],
    )
    .await
}

fn body(response: &HttpResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap()
}

async fn info(address: SocketAddr, device: &PairResponse) -> ServerInfo {
    let response = get(address, device, "/v1/server-info").await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).unwrap()
}

async fn connect(config: &Config) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.workspace.state_root.join("state.db"))
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn receipt_and_stream(config: &Config) -> (Vec<String>, Vec<String>) {
    let mut db = connect(config).await;
    let receipts = sqlx::query_scalar::<_, String>("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    let streams = sqlx::query_scalar::<_, String>("SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    (receipts, streams)
}

async fn run_retention(config: &Config, now: i64) -> ServerInfo {
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    retain_journal(&mut workspace, RETENTION_SECONDS, BUDGET, now)
        .await
        .unwrap();
    let info = workspace.server_info().await.unwrap();
    workspace.close().await.unwrap();
    info
}

#[tokio::test]
async fn journal_retention_http_preserves_complete_changes_and_restarts_at_floor() {
    let (_directory, config) = initialized().await;
    let first_server = server(&config).await;
    let device = pair(&config, &first_server).await;
    let initial = info(first_server.local_addr(), &device).await;
    assert_eq!(initial.head_seq.get(), 0);
    assert_eq!(initial.journal_floor_seq.get(), 0);
    let node = uuid::Uuid::now_v7().to_string();
    let create_id = uuid::Uuid::now_v7().to_string();
    let create_body = json!({"node_id":node,"parent_id":initial.root_id.to_string(),"name":"synthetic.md","kind":"file"});
    let created = request(
        first_server.local_addr(),
        "POST",
        "/v1/nodes",
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "1"),
            ("Idempotency-Key", &create_id),
        ],
        &serde_json::to_vec(&create_body).unwrap(),
    )
    .await;
    assert_eq!(created.status, 200);
    let created = body(&created)["node"].clone();
    let first_page = get(
        first_server.local_addr(),
        &device,
        "/v1/changes?after=0&through=1",
    )
    .await;
    assert_eq!(first_page.status, 200);
    let first_nodes = body(&first_page)["nodes"].as_array().unwrap().clone();
    assert_eq!(first_nodes.len(), 2);
    assert!(first_nodes.iter().any(|value| value["node_id"] == node));
    assert!(
        first_nodes
            .iter()
            .any(|value| value["node_id"] == initial.root_id.to_string())
    );
    assert!(
        first_nodes
            .iter()
            .all(|value| value["last_change_seq"] == "1")
    );
    first_server.shutdown().await.unwrap();

    let mut db = connect(&config).await;
    let committed_at: Option<i64> =
        sqlx::query_scalar("SELECT committed_at FROM change_sets WHERE seq='1'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(committed_at, None);
    db.close().await.unwrap();
    let observed = run_retention(&config, START).await;
    assert_eq!(
        observed.journal_floor_seq.get(),
        0,
        "first observation must not expire a newly observed commit"
    );
    assert_eq!(observed.workspace_epoch, initial.workspace_epoch);
    let mut db = connect(&config).await;
    let observed_time: Option<i64> =
        sqlx::query_scalar("SELECT committed_at FROM change_sets WHERE seq='1'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(observed_time.is_some_and(|time| time > START));
    db.close().await.unwrap();

    let second_server = server(&config).await;
    let save_id = uuid::Uuid::now_v7().to_string();
    let saved = request(
        second_server.local_addr(),
        "PUT",
        &format!("/v1/nodes/{node}/content"),
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "2"),
            ("Idempotency-Key", &save_id),
            ("If-Match", created["content_etag"].as_str().unwrap()),
            (
                "Content-Digest",
                &format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(CONTENT))),
            ),
        ],
        CONTENT,
    )
    .await;
    assert_eq!(saved.status, 200);
    let saved_state = body(
        &get(
            second_server.local_addr(),
            &device,
            &format!("/v1/nodes/{node}"),
        )
        .await,
    );
    let epoch = info(second_server.local_addr(), &device)
        .await
        .workspace_epoch;
    assert_eq!(epoch, initial.workspace_epoch);
    let receipt_before = get(
        second_server.local_addr(),
        &device,
        &format!("/v1/operations/{save_id}"),
    )
    .await;
    assert_eq!(receipt_before.status, 200);
    let receipt_before = body(&receipt_before);
    second_server.shutdown().await.unwrap();
    let preserved = receipt_and_stream(&config).await;
    assert_eq!(preserved.0.len(), 2);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("synthetic.md")).unwrap(),
        CONTENT
    );

    let before_expiry = run_retention(&config, START + RETENTION_SECONDS as i64 - 1).await;
    assert_eq!(before_expiry.journal_floor_seq.get(), 0);
    assert_eq!(before_expiry.head_seq.get(), 2);
    assert_eq!(before_expiry.workspace_epoch, epoch);
    assert_eq!(receipt_and_stream(&config).await, preserved);
    let at_early_boundary = run_retention(&config, START + RETENTION_SECONDS as i64).await;
    assert_eq!(at_early_boundary.journal_floor_seq.get(), 0);
    assert_eq!(at_early_boundary.head_seq.get(), 2);

    let retained = run_retention(&config, START + RETENTION_SECONDS as i64 + 2).await;
    assert_eq!(retained.journal_floor_seq.get(), 1);
    assert_eq!(retained.head_seq.get(), 2);
    assert_eq!(retained.workspace_epoch, epoch);
    let mut db = connect(&config).await;
    let deleted_changes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='1'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    let remaining_changes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='2'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    let baseline: Vec<(String, String)> =
        sqlx::query_as("SELECT node_id,seq FROM journal_baseline ORDER BY node_id")
            .fetch_all(&mut db)
            .await
            .unwrap();
    assert_eq!(deleted_changes, 0);
    assert_eq!(remaining_changes, 1);
    assert_eq!(baseline.len(), 2);
    assert!(baseline.contains(&(node.clone(), "1".to_string())));
    assert!(baseline.contains(&(initial.root_id.to_string(), "1".to_string())));
    db.close().await.unwrap();
    assert_eq!(receipt_and_stream(&config).await, preserved);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("synthetic.md")).unwrap(),
        CONTENT
    );

    let third_server = server(&config).await;
    let after_restart = info(third_server.local_addr(), &device).await;
    assert_eq!(after_restart.workspace_epoch, epoch);
    assert_eq!(after_restart.head_seq.get(), 2);
    assert_eq!(after_restart.journal_floor_seq.get(), 1);
    let denied = request(
        third_server.local_addr(),
        "GET",
        "/v1/changes?after=0",
        &[],
        &[],
    )
    .await;
    assert_eq!(denied.status, 401);
    let expired = get(
        third_server.local_addr(),
        &device,
        "/v1/changes?after=0&through=2",
    )
    .await;
    assert_eq!(expired.status, 410);
    assert_eq!(expired.header("cache-control"), Some("no-store"));
    assert_eq!(body(&expired)["code"], "resync_required");
    let page = get(
        third_server.local_addr(),
        &device,
        "/v1/changes?after=1&through=2",
    )
    .await;
    assert_eq!(page.status, 200);
    let page = body(&page);
    assert_eq!(page["from_exclusive"], "1");
    assert_eq!(page["covered_through"], "2");
    assert_eq!(page["through_seq"], "2");
    assert_eq!(page["has_more"], false);
    assert_eq!(page["nodes"], json!([saved_state]));
    let empty = get(
        third_server.local_addr(),
        &device,
        "/v1/changes?after=1&through=1",
    )
    .await;
    assert_eq!(empty.status, 200);
    assert_eq!(body(&empty)["nodes"], json!([]));
    let receipt_after = get(
        third_server.local_addr(),
        &device,
        &format!("/v1/operations/{save_id}"),
    )
    .await;
    assert_eq!(receipt_after.status, 200);
    assert_eq!(body(&receipt_after), receipt_before);
    let file_after = get(
        third_server.local_addr(),
        &device,
        &format!("/v1/nodes/{node}/content"),
    )
    .await;
    assert_eq!(file_after.status, 200);
    assert_eq!(file_after.body, CONTENT);
    third_server.shutdown().await.unwrap();

    let retained_again = run_retention(&config, START + 2 * RETENTION_SECONDS as i64 + 10).await;
    assert_eq!(retained_again.journal_floor_seq.get(), 2);
    assert_eq!(retained_again.head_seq.get(), 2);
    assert_eq!(retained_again.workspace_epoch, epoch);
    let fourth_server = server(&config).await;
    let last = info(fourth_server.local_addr(), &device).await;
    assert_eq!(last.journal_floor_seq.get(), 2);
    assert_eq!(last.workspace_epoch, epoch);
    assert_eq!(receipt_and_stream(&config).await, preserved);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("synthetic.md")).unwrap(),
        CONTENT
    );
    fourth_server.shutdown().await.unwrap();
}
