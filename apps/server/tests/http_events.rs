mod support;

use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, HeadHint, PairResponse};
use notryx_server::{admin, config::Config, http::RunningServer};
use std::net::SocketAddr;
use std::time::Duration;
use support::{initialized, pairing_request, post_json, request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn setup() -> (tempfile::TempDir, Config, RunningServer, PairResponse) {
    let (directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let pairing = pairing_request(&config).await.0;
    let response = post_json(server.local_addr(), &serde_json::to_vec(&pairing).unwrap()).await;
    assert_eq!(response.status, 200);
    let device = serde_json::from_slice(&response.body).unwrap();
    (directory, config, server, device)
}

async fn events(address: SocketAddr, token: &str) -> (TcpStream, Vec<u8>) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(format!("GET /v1/events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut buf = [0; 4096];
            let read = stream.read(&mut buf).await.unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buf[..read]);
        }
    })
    .await
    .unwrap();
    (stream, bytes)
}

async fn read_until(stream: &mut TcpStream, bytes: &mut Vec<u8>, needle: &[u8], timeout: Duration) {
    tokio::time::timeout(timeout, async {
        while !bytes.windows(needle.len()).any(|window| window == needle) {
            let mut buf = [0; 4096];
            let read = stream.read(&mut buf).await.unwrap();
            assert!(read > 0, "stream ended before event");
            bytes.extend_from_slice(&buf[..read]);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn events_send_head_hints_and_changes_recovers_missed_hints() {
    let (_directory, _config, server, device) = setup().await;
    let address = server.local_addr();
    let (mut stream, mut wire) = events(address, &device.device_token).await;
    read_until(
        &mut stream,
        &mut wire,
        b"\"head_seq\":\"0\"",
        Duration::from_secs(5),
    )
    .await;
    let response = String::from_utf8_lossy(&wire);
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream")
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("cache-control: no-store")
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("x-accel-buffering: no")
    );
    assert!(response.contains("event: head"));
    assert!(response.contains("id: 0"));
    let payload = response
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let hint: HeadHint = serde_json::from_str(payload).unwrap();
    assert_eq!(hint.workspace_epoch, device.workspace_epoch);
    assert_eq!(hint.head_seq.get(), 0);

    let info = request(
        address,
        "GET",
        "/v1/server-info",
        &[("Authorization", &format!("Bearer {}", device.device_token))],
        &[],
    )
    .await;
    let root = serde_json::from_slice::<serde_json::Value>(&info.body).unwrap()["root_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = serde_json::to_vec(&serde_json::json!({"node_id":uuid::Uuid::now_v7().to_string(), "parent_id":root,"name":"synthetic.md","kind":"file"})).unwrap();
    let created = request(
        address,
        "POST",
        "/v1/nodes",
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "1"),
            ("Idempotency-Key", &uuid::Uuid::now_v7().to_string()),
        ],
        &body,
    )
    .await;
    assert_eq!(created.status, 200);
    read_until(
        &mut stream,
        &mut wire,
        b"\"head_seq\":\"1\"",
        Duration::from_secs(5),
    )
    .await;
    drop(stream);
    let changes = request(
        address,
        "GET",
        "/v1/changes?after=0",
        &[("Authorization", &format!("Bearer {}", device.device_token))],
        &[],
    )
    .await;
    let page: serde_json::Value = serde_json::from_slice(&changes.body).unwrap();
    assert_eq!(page["covered_through"], "1");
    assert_eq!(page["nodes"].as_array().unwrap().len(), 2);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_rejects_missing_auth_and_closes_on_device_revocation() {
    let (_directory, config, server, device) = setup().await;
    let address = server.local_addr();
    assert_eq!(
        request(address, "GET", "/v1/events", &[], &[]).await.status,
        401
    );
    let (mut stream, mut wire) = events(address, &device.device_token).await;
    read_until(
        &mut stream,
        &mut wire,
        b"\"head_seq\":\"0\"",
        Duration::from_secs(5),
    )
    .await;
    let result = admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: device.device_id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        AdminResponse::Ok {
            result: AdminResult::Done {}
        }
    ));
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        request(
            address,
            "GET",
            "/v1/events",
            &[("Authorization", &format!("Bearer {}", device.device_token))],
            &[]
        )
        .await
        .status,
        401
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_enforces_device_and_principal_limits_and_keeps_regular_requests_available() {
    let (_directory, config, server, device) = setup().await;
    let address = server.local_addr();
    let mut streams = Vec::new();
    let auth = format!("Bearer {}", device.device_token);
    let (mut first, mut wire) = events(address, &device.device_token).await;
    read_until(
        &mut first,
        &mut wire,
        b"\"head_seq\":\"0\"",
        Duration::from_secs(5),
    )
    .await;
    streams.push(first);
    assert_eq!(
        request(
            address,
            "GET",
            "/v1/events",
            &[("Authorization", &auth)],
            &[]
        )
        .await
        .status,
        429
    );
    let mut extra = None;
    for index in 0..5 {
        let pair = pairing_request(&config).await.0;
        let response = post_json(address, &serde_json::to_vec(&pair).unwrap()).await;
        let paired: PairResponse = serde_json::from_slice(&response.body).unwrap();
        if index == 4 {
            extra = Some(paired);
            break;
        }
        let (mut stream, mut wire) = events(address, &paired.device_token).await;
        read_until(
            &mut stream,
            &mut wire,
            b"\"head_seq\":\"0\"",
            Duration::from_secs(5),
        )
        .await;
        streams.push(stream);
    }
    let extra = extra.unwrap();
    assert_eq!(
        request(
            address,
            "GET",
            "/v1/events",
            &[("Authorization", &format!("Bearer {}", extra.device_token))],
            &[]
        )
        .await
        .status,
        429
    );
    assert_eq!(
        request(
            address,
            "GET",
            "/v1/server-info",
            &[("Authorization", &auth)],
            &[]
        )
        .await
        .status,
        200
    );
    drop(streams);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_sends_periodic_heartbeat() {
    let (_directory, _config, server, device) = setup().await;
    let (mut stream, mut wire) = events(server.local_addr(), &device.device_token).await;
    read_until(
        &mut stream,
        &mut wire,
        b": heartbeat\n\n",
        Duration::from_secs(20),
    )
    .await;
    drop(stream);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_closes_expired_device_on_heartbeat() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

    let (_directory, config, server, device) = setup().await;
    let (mut stream, mut wire) = events(server.local_addr(), &device.device_token).await;
    read_until(
        &mut stream,
        &mut wire,
        b"\"head_seq\":\"0\"",
        Duration::from_secs(5),
    )
    .await;
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
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut Vec::new()))
        .await
        .unwrap()
        .unwrap();
    server.shutdown().await.unwrap();
}
