pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::PairResponse;
use notryx_server::http::RunningServer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use support::{initialized, pairing_request, post_json, request};

#[tokio::test]
async fn oversized_raw_post_state_preserves_http_error_and_server_readiness() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let address = server.local_addr();
    let pairing = pairing_request(&config).await.0;
    let response = post_json(address, &serde_json::to_vec(&pairing).unwrap()).await;
    assert_eq!(response.status, 200);
    let device: PairResponse = serde_json::from_slice(&response.body).unwrap();
    let authorization = format!("Bearer {}", device.device_token);
    let auth = [("Authorization", authorization.as_str())];
    let info = request(address, "GET", "/v1/server-info", &auth, &[]).await;
    assert_eq!(info.status, 200);
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(info["ready"], true);
    let root = info["root_id"].as_str().unwrap();
    let node = uuid::Uuid::now_v7().to_string();
    let create_id = uuid::Uuid::now_v7().to_string();
    let epoch = device.workspace_epoch.to_string();
    let created = request(
        address,
        "POST",
        "/v1/nodes",
        &[
            ("Authorization", &authorization),
            ("Content-Type", "application/json"),
            ("X-Workspace-Epoch", &epoch),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "1"),
            ("Idempotency-Key", &create_id),
        ],
        &serde_json::to_vec(
            &json!({"node_id":node,"parent_id":root,"name":"synthetic.md","kind":"file"}),
        )
        .unwrap(),
    )
    .await;
    assert_eq!(created.status, 200);
    let created: Value = serde_json::from_slice(&created.body).unwrap();
    let save_id = uuid::Uuid::now_v7().to_string();
    let contents = b"synthetic content\n";
    let digest = format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(contents)));
    let saved = request(
        address,
        "PUT",
        &format!("/v1/nodes/{node}/content"),
        &[
            ("Authorization", &authorization),
            ("X-Workspace-Epoch", &epoch),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "2"),
            ("Idempotency-Key", &save_id),
            (
                "If-Match",
                created["node"]["content_etag"].as_str().unwrap(),
            ),
            ("Content-Digest", &digest),
        ],
        contents,
    )
    .await;
    assert_eq!(saved.status, 200);
    let receipt_before = request(
        address,
        "GET",
        &format!("/v1/operations/{save_id}"),
        &auth,
        &[],
    )
    .await;
    assert_eq!(receipt_before.status, 200);
    let receipt_before: Value = serde_json::from_slice(&receipt_before.body).unwrap();

    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap();
    let result = sqlx::query("UPDATE node_changes SET post_state=json_set(post_state,'$.content_rev',?) WHERE seq='2' AND node_id=?")
        .bind("x".repeat(1024 * 1024 + 1))
        .bind(&node)
        .execute(&mut db).await.unwrap();
    assert_eq!(result.rows_affected(), 1);
    let post_before: String =
        sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq='2' AND node_id=?")
            .bind(&node)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(post_before.len() > 1024 * 1024);
    let receipts_before: Vec<String> = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    assert_eq!(receipts_before.len(), 2);

    let denied = request(address, "GET", "/v1/changes?after=1", &[], &[]).await;
    assert_eq!(denied.status, 401);
    let oversized = request(address, "GET", "/v1/changes?after=1", &auth, &[]).await;
    assert_eq!(oversized.status, 503);
    let error: Value = serde_json::from_slice(&oversized.body).unwrap();
    assert_eq!(error["code"], "storage_unavailable");
    let info_after = request(address, "GET", "/v1/server-info", &auth, &[]).await;
    assert_eq!(info_after.status, 200);
    let info_after: Value = serde_json::from_slice(&info_after.body).unwrap();
    assert_eq!(info_after["ready"], true);
    assert_eq!(info_after["head_seq"], "2");
    let content_after = request(
        address,
        "GET",
        &format!("/v1/nodes/{node}/content"),
        &auth,
        &[],
    )
    .await;
    assert_eq!(content_after.status, 200);
    assert_eq!(content_after.body, contents);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("synthetic.md")).unwrap(),
        contents
    );
    let receipt_after = request(
        address,
        "GET",
        &format!("/v1/operations/{save_id}"),
        &auth,
        &[],
    )
    .await;
    assert_eq!(receipt_after.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&receipt_after.body).unwrap(),
        receipt_before
    );
    let post_after: String =
        sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq='2' AND node_id=?")
            .bind(&node)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(post_after, post_before);
    let receipts_after: Vec<String> = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    assert_eq!(receipts_after, receipts_before);
    db.close().await.unwrap();
    server.shutdown().await.unwrap();
}
