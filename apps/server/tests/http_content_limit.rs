pub mod support;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use notryx_server::{http::RunningServer, runtime::WorkspaceRuntime};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::time::Duration;
use support::{initialized, request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn start_read(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: &str,
) -> (TcpStream, u16) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\n\r\n").as_bytes()).await.unwrap();
    let mut wire = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !wire.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut bytes = [0; 1024];
            let n = stream.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            wire.extend_from_slice(&bytes[..n]);
        }
    })
    .await
    .unwrap();
    let status = std::str::from_utf8(&wire[..wire.iter().position(|&b| b == b'\r').unwrap()])
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (stream, status)
}

#[tokio::test]
async fn content_reads_are_bounded_and_release_slots_on_disconnect() {
    let (_directory, config) = initialized().await;
    std::fs::write(
        config.workspace.notes_root.join("synthetic.md"),
        vec![b'x'; 5 * 1024 * 1024],
    )
    .unwrap();
    let node = WorkspaceRuntime::adopt(config.workspace.clone(), "synthetic.md".into())
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap();
    let workspace: String =
        sqlx::query_scalar("SELECT workspace_id FROM workspaces WHERE singleton=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    let mut tx = db.begin().await.unwrap();
    let secret = [42_u8; 32];
    let secret_hash = Sha256::digest(secret);
    let mut tokens = Vec::new();
    for _ in 0..4 {
        let principal = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO principals VALUES(?,?,'reader')")
            .bind(principal.to_string())
            .bind(&workspace)
            .execute(&mut *tx)
            .await
            .unwrap();
        for _ in 0..5 {
            let device = uuid::Uuid::new_v4();
            let stream = uuid::Uuid::new_v4();
            let token_id = uuid::Uuid::new_v4();
            sqlx::query("INSERT INTO operation_streams(stream_id,last_seq) VALUES(?,'0')")
                .bind(stream.to_string())
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("INSERT INTO devices(device_id,principal_id,stream_id,token_id,token_hash,issued_at,idle_expires_at,absolute_expires_at) VALUES(?,?,?,?,?,1,4102444800,4102444800)")
                .bind(device.to_string()).bind(principal.to_string()).bind(stream.to_string()).bind(token_id.to_string()).bind(secret_hash.as_slice()).execute(&mut *tx).await.unwrap();
            tokens.push(format!(
                "device.{token_id}.{}",
                URL_SAFE_NO_PAD.encode(secret)
            ));
        }
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let mut event_streams = Vec::new();
    for token in &tokens {
        let (stream, status) = start_read(server.local_addr(), "GET", "/v1/events", token).await;
        assert_eq!(status, 200);
        event_streams.push(stream);
    }
    let token = &tokens[0];
    let path = format!("/v1/nodes/{node}/content");
    let mut streams = Vec::new();
    for _ in 0..12 {
        let (stream, status) = start_read(server.local_addr(), "GET", &path, token).await;
        assert_eq!(status, 200);
        streams.push(stream);
    }
    let auth = format!("Bearer {token}");
    assert_eq!(
        request(
            server.local_addr(),
            "GET",
            &path,
            &[("Authorization", &auth)],
            &[]
        )
        .await
        .status,
        429
    );
    let (head_stream, status) = start_read(server.local_addr(), "HEAD", &path, token).await;
    assert_eq!(status, 429);
    drop(head_stream);
    assert_eq!(
        request(
            server.local_addr(),
            "GET",
            "/v1/server-info",
            &[("Authorization", &auth)],
            &[]
        )
        .await
        .status,
        200
    );
    drop(streams.pop());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (stream, status) = start_read(server.local_addr(), "GET", &path, token).await;
            if status == 200 {
                drop(stream);
                break;
            }
            assert_eq!(status, 429);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    drop(streams);
    drop(event_streams);
    let full = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = request(
                server.local_addr(),
                "GET",
                &path,
                &[("Authorization", &auth)],
                &[],
            )
            .await;
            if response.status == 200 {
                break response;
            }
            assert_eq!(response.status, 429);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(full.status, 200);
    assert_eq!(full.header("content-length"), Some("5242880"));
    assert_eq!(full.body, vec![b'x'; 5 * 1024 * 1024]);
    server.shutdown().await.unwrap();
}
