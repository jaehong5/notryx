pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::PairResponse;
use notryx_server::{Workspace, http::RunningServer};
use sha2::{Digest, Sha256};
use std::time::Duration;
use support::{initialized, pairing_request, post_json, request};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpSocket, TcpStream};

#[tokio::test]
async fn stalled_downloads_release_connections_without_the_readers_closing_them() {
    let (_directory, config) = initialized().await;
    let path = config.workspace.notes_root.join("Attachment.bin");
    std::fs::write(&path, vec![b'x'; 25 * 1024 * 1024]).unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace
        .adopt_root_document("Attachment.bin")
        .await
        .unwrap();
    workspace.close().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let pair = pairing_request(&config).await.0;
    let response = post_json(server.local_addr(), &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let device: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let authorization = format!("Bearer {}", device.device_token);
    let mut downloads = Vec::new();
    for _ in 0..16 {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(1024).unwrap();
        let mut stream = socket.connect(server.local_addr()).await.unwrap();
        let head = format!(
            "GET /v1/nodes/{}/content HTTP/1.1\r\nHost: localhost\r\nAuthorization: {authorization}\r\nConnection: close\r\n\r\n",
            document.node_id
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        downloads.push(stream);
    }
    let saturated = tokio::time::timeout(
        Duration::from_secs(1),
        request(server.local_addr(), "GET", "/health/ready", &[], &[]),
    )
    .await
    .is_err();
    for stream in &mut downloads {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut head = Vec::new();
            let mut bytes = [0; 1024];
            while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut bytes).await.unwrap();
                assert!(count > 0 && head.len() < 16 * 1024);
                head.extend_from_slice(&bytes[..count]);
            }
            assert!(head.starts_with(b"HTTP/1.1 200"));
        })
        .await
        .expect("authenticated download must start");
    }
    tokio::time::sleep(Duration::from_secs(35)).await;
    let recovered = tokio::time::timeout(Duration::from_secs(3), async {
        let health = request(server.local_addr(), "GET", "/health/ready", &[], &[]).await;
        let info = request(
            server.local_addr(),
            "GET",
            "/v1/server-info",
            &[("Authorization", &authorization)],
            &[],
        )
        .await;
        (health.status, info.status)
    })
    .await;
    drop(downloads);
    server.shutdown().await.unwrap();
    assert!(
        saturated,
        "the readers must occupy every connection slot first"
    );
    assert_eq!(
        recovered.ok(),
        Some((200, 200)),
        "stalled responses must release slots without waiting for the clients to close"
    );
    assert_eq!(std::fs::metadata(path).unwrap().len(), 25 * 1024 * 1024);
}

async fn read_response(stream: &mut TcpStream) -> (u16, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut length = None;
        let mut header_bytes = line.len();
        loop {
            line.clear();
            assert!(stream.read_line(&mut line).await.unwrap() > 0);
            header_bytes += line.len();
            assert!(header_bytes <= 16 * 1024);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let length = length.expect("fixed responses must declare their body size");
        assert!(length <= 16 * 1024);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        (status, body)
    })
    .await
    .expect("HTTP response must complete")
}

#[tokio::test]
async fn progressing_upload_keeps_its_connection_past_the_write_stall_interval() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let pair = pairing_request(&config).await.0;
    let response = post_json(server.local_addr(), &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let device: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let payload = vec![b'p'; 36 * 8192];
    let digest = STANDARD.encode(Sha256::digest(&payload));
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let head = format!(
        "POST /v1/uploads HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nContent-Digest: sha-256=:{digest}:\r\n\r\n",
        device.device_token,
        payload.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    for (index, chunk) in payload.chunks(8192).enumerate() {
        if index > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        stream.write_all(chunk).await.unwrap();
    }
    let (status, body) = read_response(&mut stream).await;
    assert_eq!(status, 201);
    let upload: notryx_protocol::UploadResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(upload.size, payload.len().to_string());
    stream
        .write_all(b"GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (status, body) = read_response(&mut stream).await;
    assert_eq!(status, 200);
    assert_eq!(body, b"live\n");
    drop(stream);
    server.shutdown().await.unwrap();
}
