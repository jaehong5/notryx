pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::PairResponse;
use notryx_server::{config::Config, http::RunningServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use support::{HttpResponse, initialized, pairing_request, post_json, request};

async fn setup() -> (
    tempfile::TempDir,
    Config,
    RunningServer,
    PairResponse,
    String,
) {
    let (directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let pairing = pairing_request(&config).await.0;
    let paired = post_json(server.local_addr(), &serde_json::to_vec(&pairing).unwrap()).await;
    assert_eq!(paired.status, 200);
    let device: PairResponse = serde_json::from_slice(&paired.body).unwrap();
    let info = get(server.local_addr(), &device, "/v1/server-info").await;
    let root = body(&info)["root_id"].as_str().unwrap().to_owned();
    (directory, config, server, device, root)
}

fn body(response: &HttpResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap()
}

fn id() -> String {
    uuid::Uuid::now_v7().to_string()
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

async fn create(
    address: SocketAddr,
    device: &PairResponse,
    node: &str,
    parent: &str,
) -> HttpResponse {
    let value = json!({"node_id":node,"parent_id":parent,"name":"created.md","kind":"file"});
    request(
        address,
        "POST",
        "/v1/nodes",
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "1"),
            ("Idempotency-Key", &id()),
        ],
        &serde_json::to_vec(&value).unwrap(),
    )
    .await
}

async fn put(
    address: SocketAddr,
    device: &PairResponse,
    node: &str,
    etag: &str,
    bytes: &[u8],
) -> HttpResponse {
    request(
        address,
        "PUT",
        &format!("/v1/nodes/{node}/content"),
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "2"),
            ("Idempotency-Key", &id()),
            ("If-Match", etag),
            (
                "Content-Digest",
                &format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(bytes))),
            ),
        ],
        bytes,
    )
    .await
}

#[tokio::test]
async fn changes_returns_historical_states_and_keeps_requested_through_fixed() {
    let (_directory, _config, server, device, root) = setup().await;
    let address = server.local_addr();
    let node = id();
    let created = create(address, &device, &node, &root).await;
    assert_eq!(created.status, 200);
    let created = body(&created)["node"].clone();
    let saved = put(
        address,
        &device,
        &node,
        created["content_etag"].as_str().unwrap(),
        b"later\n",
    )
    .await;
    assert_eq!(saved.status, 200);
    let latest = body(&get(address, &device, &format!("/v1/nodes/{node}")).await);
    assert_ne!(created["content_rev"], latest["content_rev"]);

    let first = get(address, &device, "/v1/changes?after=0&through=1").await;
    assert_eq!(first.status, 200);
    assert_eq!(first.header("cache-control"), Some("no-store"));
    let page = body(&first);
    assert_eq!(page["workspace_epoch"], device.workspace_epoch.to_string());
    assert_eq!(page["from_exclusive"], "0");
    assert_eq!(page["through_seq"], "1");
    assert_eq!(page["covered_through"], "1");
    assert_eq!(page["has_more"], false);
    let nodes = page["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2, "node and parent are one complete ChangeSet");
    assert!(nodes.contains(&created));
    assert!(
        nodes
            .iter()
            .any(|node| node["node_id"] == root && node["last_change_seq"] == "1")
    );
    assert!(nodes.iter().all(|node| node["last_change_seq"] == "1"));

    let page = body(&get(address, &device, "/v1/changes?after=0").await);
    assert_eq!(page["through_seq"], "2");
    assert_eq!(page["covered_through"], "2");
    assert_eq!(page["has_more"], false);
    let versions: Vec<_> = page["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["node_id"] == node)
        .collect();
    assert_eq!(versions, vec![&created, &latest]);
    let empty = get(address, &device, "/v1/changes?after=2&through=2").await;
    assert_eq!(empty.status, 200);
    let empty = body(&empty);
    assert_eq!(empty["covered_through"], "2");
    assert_eq!(empty["has_more"], false);
    assert_eq!(empty["nodes"], json!([]));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn changes_requires_authentication_and_rejects_invalid_cursors() {
    let (_directory, _config, server, device, _root) = setup().await;
    let address = server.local_addr();
    let denied = request(address, "GET", "/v1/changes?after=0", &[], &[]).await;
    assert_eq!(denied.status, 401);
    for query in [
        "after=",
        "after=-1",
        "after=18446744073709551616",
        "after=0&through=-1",
        "after=1&through=0",
        "after=0&through=1",
        "after=0&after=0",
        "after=0&extra=1",
    ] {
        let response = get(address, &device, &format!("/v1/changes?{query}")).await;
        assert_eq!(response.status, 400, "invalid query {query}");
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn changes_allows_editors_and_readers_and_denies_revoked_or_expired_devices() {
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceRole};
    use notryx_server::admin;
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

    let (_directory, config, server, device, _root) = setup().await;
    let address = server.local_addr();
    let principal = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Editor,
        },
    )
    .await
    .unwrap();
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = principal
    else {
        panic!("synthetic principal")
    };
    let grant = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing { principal_id },
    )
    .await
    .unwrap();
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = grant
    else {
        panic!("synthetic pairing")
    };
    let paired = post_json(address, &serde_json::to_vec(&json!({"protocol":notryx_protocol::CONTRACT,"workspace_id":workspace_id,"pairing_secret":pairing_secret})).unwrap()).await;
    assert_eq!(paired.status, 200);
    let reader: PairResponse = serde_json::from_slice(&paired.body).unwrap();
    assert_eq!(
        get(address, &reader, "/v1/changes?after=0").await.status,
        200
    );
    let changed = admin::request(
        &config.server.admin_socket,
        &AdminRequest::SetRole {
            principal_id: reader.principal_id.clone(),
            role: WorkspaceRole::Reader,
        },
    )
    .await
    .unwrap();
    assert!(matches!(changed, AdminResponse::Ok { .. }));
    assert_eq!(
        get(address, &reader, "/v1/changes?after=0").await.status,
        200
    );
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: reader.device_id.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        get(address, &reader, "/v1/changes?after=0").await.status,
        401
    );
    server.shutdown().await.unwrap();

    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE devices SET issued_at=1,idle_expires_at=2,absolute_expires_at=3 WHERE device_id=?",
    )
    .bind(&device.device_id)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let restarted = RunningServer::start(config).await.unwrap();
    assert_eq!(
        get(restarted.local_addr(), &device, "/v1/changes?after=0")
            .await
            .status,
        401
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_workspace_marker_is_unavailable_not_document_not_found() {
    let (directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let info_before = body(&get(address, &device, "/v1/server-info").await);
    let note = config.workspace.notes_root.join("External.md");
    std::fs::write(&note, b"external bytes remain\n").unwrap();
    let marker = config.workspace.notes_root.join(".mdw-workspace.json");
    let marker_saved = directory.path().join("saved-workspace-marker.json");
    std::fs::rename(&marker, &marker_saved).unwrap();

    let first = create(address, &device, &id(), &root).await;
    assert_eq!(first.status, 503);
    assert_eq!(body(&first)["code"], "storage_unavailable");
    let second = create(address, &device, &id(), &root).await;
    assert_eq!(second.status, 503);
    assert_eq!(body(&second)["code"], "recovery_pending");
    let info_after = body(&get(address, &device, "/v1/server-info").await);
    assert_eq!(info_after["head_seq"], info_before["head_seq"]);
    assert_eq!(info_after["ready"], false);
    assert_eq!(std::fs::read(&note).unwrap(), b"external bytes remain\n");
    assert!(marker_saved.is_file());
    assert!(!config.workspace.notes_root.join("created.md").exists());
    server.shutdown().await.unwrap();
}
