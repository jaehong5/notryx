pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, PairResponse, WorkspaceRole};
use notryx_server::{admin, config::Config, http::RunningServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::net::SocketAddr;
use std::time::Duration;
use support::{HttpResponse, initialized, pairing_request, post_json, request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn setup() -> (
    tempfile::TempDir,
    Config,
    RunningServer,
    PairResponse,
    String,
) {
    let (directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let pair = pairing_request(&config).await.0;
    let response = post_json(server.local_addr(), &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let device: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let response = get(server.local_addr(), &device, "/v1/server-info").await;
    let root = value(&response)["root_id"].as_str().unwrap().to_owned();
    (directory, config, server, device, root)
}

fn value(response: &HttpResponse) -> Value {
    assert_eq!(response.header("cache-control"), Some("no-store"));
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

fn digest(bytes: &[u8]) -> String {
    format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(bytes)))
}

async fn upload(address: SocketAddr, device: &PairResponse, bytes: &[u8]) -> HttpResponse {
    support::request_with_timeout(
        address,
        "POST",
        "/v1/uploads",
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/octet-stream"),
            ("Content-Digest", &digest(bytes)),
        ],
        bytes,
        Duration::from_secs(60),
    )
    .await
}

async fn create(
    address: SocketAddr,
    device: &PairResponse,
    body: &Value,
    operation: &str,
    sequence: u64,
) -> HttpResponse {
    support::request_with_timeout(
        address,
        "POST",
        "/v1/nodes",
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", &sequence.to_string()),
            ("Idempotency-Key", operation),
        ],
        &serde_json::to_vec(body).unwrap(),
        Duration::from_secs(60),
    )
    .await
}

#[tokio::test]
async fn upload_preserves_binary_bytes_and_attach_is_single_use_with_receipt_retry() {
    let (_directory, config, server, device, root) = setup().await;
    let bytes: Vec<u8> = (0..131_073).map(|offset| (offset % 256) as u8).collect();
    let response = upload(server.local_addr(), &device, &bytes).await;
    assert_eq!(response.status, 201);
    let staged = value(&response);
    assert_eq!(staged["size"], bytes.len().to_string());
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(staged["content_hash"], format!("sha256:{hash}"));
    assert_eq!(
        uuid::Uuid::parse_str(staged["upload_id"].as_str().unwrap())
            .unwrap()
            .get_version_num(),
        4
    );
    let info = value(&get(server.local_addr(), &device, "/v1/server-info").await);
    assert_eq!(info["head_seq"], "0");
    assert_eq!(info["max_document_bytes"], 5 * 1024 * 1024);
    assert_eq!(info["max_attachment_bytes"], 25 * 1024 * 1024);
    let node = uuid::Uuid::now_v7().to_string();
    let operation = uuid::Uuid::now_v7().to_string();
    let body = json!({"node_id":node,"parent_id":root,"name":"binary.dat","kind":"file","upload_id":staged["upload_id"]});
    let created = create(server.local_addr(), &device, &body, &operation, 1).await;
    assert_eq!(created.status, 200);
    let content = get(
        server.local_addr(),
        &device,
        &format!("/v1/nodes/{node}/content"),
    )
    .await;
    assert_eq!(content.status, 200);
    assert_eq!(content.body, bytes);
    let mut db = database(&config).await;
    sqlx::query("UPDATE uploads SET started_at=0,expires_at=1 WHERE upload_id=?")
        .bind(staged["upload_id"].as_str().unwrap())
        .execute(&mut db)
        .await
        .unwrap();
    drop(db);
    server.shutdown().await.unwrap();
    let server = RunningServer::start(config).await.unwrap();
    let retry = create(server.local_addr(), &device, &body, &operation, 1).await;
    assert_eq!(retry.status, 200);
    assert_eq!(value(&retry), value(&created));
    let mut second = body;
    second["node_id"] = json!(uuid::Uuid::now_v7().to_string());
    second["name"] = json!("second.dat");
    let response = create(
        server.local_addr(),
        &device,
        &second,
        &uuid::Uuid::now_v7().to_string(),
        2,
    )
    .await;
    assert_eq!(response.status, 410);
    assert_eq!(value(&response)["code"], "upload_unavailable");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn upload_rejects_missing_auth_bad_digest_and_nonidentity_encoding() {
    let (_directory, _config, server, device, _root) = setup().await;
    let bytes = b"synthetic binary\0\xff";
    let response = request(
        server.local_addr(),
        "POST",
        "/v1/uploads",
        &[("Content-Digest", &digest(bytes))],
        bytes,
    )
    .await;
    assert_eq!(response.status, 401);
    for (extra, code) in [
        (vec![], "invalid_request"),
        (
            vec![("Content-Digest", "sha-256=:AAAA:")],
            "invalid_request",
        ),
        (
            vec![(
                "Content-Digest",
                "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:",
            )],
            "digest_mismatch",
        ),
        (
            vec![
                (
                    "Content-Digest",
                    "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:",
                ),
                ("Content-Encoding", "gzip"),
            ],
            "invalid_request",
        ),
    ] {
        let auth = format!("Bearer {}", device.device_token);
        let mut headers = vec![("Authorization", auth.as_str())];
        headers.extend(extra);
        let response = request(server.local_addr(), "POST", "/v1/uploads", &headers, bytes).await;
        assert_eq!(response.status, 400);
        assert_eq!(value(&response)["code"], code);
    }
    let response = upload(server.local_addr(), &device, b"").await;
    assert_eq!(response.status, 201);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn upload_requires_current_write_role_before_receiving() {
    let (_directory, config, server, _owner, _root) = setup().await;
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Reader,
        },
    )
    .await
    .unwrap()
    else {
        panic!("reader")
    };
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing { principal_id },
    )
    .await
    .unwrap()
    else {
        panic!("pairing")
    };
    let response = post_json(server.local_addr(), &serde_json::to_vec(&json!({"protocol":"mdw-sync/1.2","workspace_id":workspace_id,"pairing_secret":pairing_secret})).unwrap()).await;
    let reader: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let response = upload(server.local_addr(), &reader, b"synthetic").await;
    assert_eq!(response.status, 403);
    assert_eq!(value(&response)["code"], "forbidden");
    server.shutdown().await.unwrap();
}

async fn receiving(
    address: SocketAddr,
    device: &PairResponse,
    bytes: &[u8],
    initial: usize,
) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let headers = format!(
        "POST /v1/uploads HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nContent-Digest: {}\r\n\r\n",
        device.device_token,
        bytes.len(),
        digest(bytes)
    );
    stream.write_all(headers.as_bytes()).await.unwrap();
    stream.write_all(&bytes[..initial]).await.unwrap();
    stream
}

async fn database(config: &Config) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap()
}

async fn wait_receiving(config: &Config, expected: i64) {
    let mut database = database(config).await;
    for _ in 0..100 {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM uploads WHERE status='receiving'")
                .fetch_one(&mut database)
                .await
                .unwrap();
        if count == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upload reservation count did not converge");
}

async fn wire_status(mut stream: TcpStream) -> u16 {
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    std::str::from_utf8(&response)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn cancelled_and_stalled_uploads_release_slots_and_shutdown_joins_writers() {
    let (_directory, config, server, device, _root) = setup().await;
    let bytes = vec![42; 512 * 1024];
    let one = receiving(server.local_addr(), &device, &bytes, 1).await;
    let two = receiving(server.local_addr(), &device, &bytes, 1).await;
    wait_receiving(&config, 2).await;
    let response = upload(server.local_addr(), &device, b"third").await;
    assert_eq!(response.status, 429);
    drop(one);
    drop(two);
    wait_receiving(&config, 0).await;
    let mut admitted = false;
    for _ in 0..40 {
        let response = upload(server.local_addr(), &device, b"after cancellation").await;
        if response.status == 201 {
            admitted = true;
            break;
        }
        assert_eq!(response.status, 429);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(admitted);
    let stalled = receiving(server.local_addr(), &device, &bytes, 16 * 1024).await;
    let started = tokio::time::Instant::now();
    assert_eq!(wire_status(stalled).await, 408);
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(14));
    let active = receiving(server.local_addr(), &device, &bytes, 1).await;
    wait_receiving(&config, 1).await;
    tokio::time::timeout(Duration::from_secs(5), server.shutdown())
        .await
        .unwrap()
        .unwrap();
    drop(active);
    let reopened = RunningServer::start(config).await.unwrap();
    assert_eq!(
        upload(reopened.local_addr(), &device, b"after shutdown")
            .await
            .status,
        201
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn attachment_limit_is_25_mib_and_raw_put_can_replace_large_file() {
    let (_directory, _config, server, device, root) = setup().await;
    let bytes = vec![0xc1; 25 * 1024 * 1024];
    let response = upload(server.local_addr(), &device, &bytes).await;
    assert_eq!(response.status, 201);
    let uploaded = value(&response);
    assert_eq!(uploaded["size"], "26214400");
    let node = uuid::Uuid::now_v7().to_string();
    let body = json!({"node_id":node,"parent_id":root,"name":"large.bin","kind":"file","upload_id":uploaded["upload_id"]});
    let response = create(
        server.local_addr(),
        &device,
        &body,
        &uuid::Uuid::now_v7().to_string(),
        1,
    )
    .await;
    assert_eq!(response.status, 200);
    let etag = value(&response)["node"]["content_etag"]
        .as_str()
        .unwrap()
        .to_owned();
    let replacement = vec![0xa2; 6 * 1024 * 1024];
    let response = support::request_with_timeout(
        server.local_addr(),
        "PUT",
        &format!("/v1/nodes/{node}/content"),
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", "2"),
            ("Idempotency-Key", &uuid::Uuid::now_v7().to_string()),
            ("If-Match", &etag),
            ("Content-Digest", &digest(&replacement)),
        ],
        &replacement,
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(response.status, 200);
    let content = get(
        server.local_addr(),
        &device,
        &format!("/v1/nodes/{node}/content"),
    )
    .await;
    assert_eq!(content.body, replacement);
    let mut oversized = bytes;
    oversized.push(0);
    let response = upload(server.local_addr(), &device, &oversized).await;
    assert_eq!(response.status, 413);
    server.shutdown().await.unwrap();
}

async fn editor(config: &Config, address: SocketAddr) -> PairResponse {
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Editor,
        },
    )
    .await
    .unwrap()
    else {
        panic!("editor")
    };
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing { principal_id },
    )
    .await
    .unwrap()
    else {
        panic!("pairing")
    };
    let response = post_json(address, &serde_json::to_vec(&json!({"protocol":"mdw-sync/1.2","workspace_id":workspace_id,"pairing_secret":pairing_secret})).unwrap()).await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).ok().unwrap()
}

#[tokio::test]
async fn upload_finish_rechecks_downgrade_revocation_and_expiry_during_body_transfer() {
    let (_directory, config, server, owner, _root) = setup().await;
    let bytes = b"synthetic partial upload";
    for mode in 0..3 {
        let device = editor(&config, server.local_addr()).await;
        let mut stream = receiving(server.local_addr(), &device, bytes, 1).await;
        wait_receiving(&config, 1).await;
        match mode {
            0 => {
                let result = admin::request(
                    &config.server.admin_socket,
                    &AdminRequest::SetRole {
                        principal_id: device.principal_id.clone(),
                        role: WorkspaceRole::Reader,
                    },
                )
                .await
                .unwrap();
                assert!(matches!(result, AdminResponse::Ok { .. }));
            }
            1 => {
                let result = admin::request(
                    &config.server.admin_socket,
                    &AdminRequest::RevokeDevice {
                        device_id: device.device_id.clone(),
                    },
                )
                .await
                .unwrap();
                assert!(matches!(result, AdminResponse::Ok { .. }));
            }
            _ => {
                let mut db = database(&config).await;
                sqlx::query("UPDATE devices SET issued_at=0,idle_expires_at=1,absolute_expires_at=1 WHERE device_id=?")
                    .bind(&device.device_id).execute(&mut db).await.unwrap();
            }
        }
        stream.write_all(&bytes[1..]).await.unwrap();
        assert_eq!(wire_status(stream).await, if mode == 0 { 403 } else { 401 });
        wait_receiving(&config, 0).await;
    }
    assert_eq!(
        upload(server.local_addr(), &owner, b"after rejected finishes")
            .await
            .status,
        201
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn upload_attachment_is_bound_to_device_and_expired_staging_is_unavailable() {
    let (_directory, config, server, owner, root) = setup().await;
    let pair = pairing_request(&config).await.0;
    let response = post_json(server.local_addr(), &serde_json::to_vec(&pair).unwrap()).await;
    let other: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let response = upload(server.local_addr(), &owner, b"private staging").await;
    assert_eq!(response.status, 201);
    let id = value(&response)["upload_id"].as_str().unwrap().to_owned();
    let body = json!({"node_id":uuid::Uuid::now_v7().to_string(),"parent_id":root,"name":"private.bin","kind":"file","upload_id":id});
    let response = create(
        server.local_addr(),
        &other,
        &body,
        &uuid::Uuid::now_v7().to_string(),
        1,
    )
    .await;
    assert_eq!(response.status, 403);
    let mut db = database(&config).await;
    sqlx::query("UPDATE uploads SET started_at=0,expires_at=1 WHERE upload_id=?")
        .bind(&id)
        .execute(&mut db)
        .await
        .unwrap();
    let response = create(
        server.local_addr(),
        &owner,
        &body,
        &uuid::Uuid::now_v7().to_string(),
        1,
    )
    .await;
    assert_eq!(response.status, 410);
    assert_eq!(value(&response)["code"], "upload_unavailable");
    assert!(!config.workspace.notes_root.join("private.bin").exists());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn upload_byte_and_count_quota_are_structured_507_errors() {
    let (_directory, mut config) = initialized().await;
    config.workspace.uploads.max_device_bytes = 4;
    config.workspace.uploads.max_workspace_bytes = 8;
    config.workspace.uploads.max_device_uploads = 1;
    config.workspace.uploads.max_workspace_uploads = 2;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let pair = pairing_request(&config).await.0;
    let response = post_json(server.local_addr(), &serde_json::to_vec(&pair).unwrap()).await;
    let device: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let response = upload(server.local_addr(), &device, b"12345").await;
    assert_eq!(response.status, 507);
    assert_eq!(value(&response)["code"], "upload_quota");
    assert_eq!(upload(server.local_addr(), &device, b"").await.status, 201);
    let response = upload(server.local_addr(), &device, b"").await;
    assert_eq!(response.status, 507);
    assert_eq!(value(&response)["code"], "upload_quota");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn upload_attachment_cannot_bypass_excluded_names_or_symlinked_parent() {
    let (directory, config, server, device, root) = setup().await;
    let payload = b"synthetic attachment\0\xff\r\n";
    let response = upload(server.local_addr(), &device, payload).await;
    assert_eq!(response.status, 201);
    let upload_id = value(&response)["upload_id"].as_str().unwrap().to_owned();
    for name in [
        "CON.txt",
        ".mdw-hidden",
        ".MdW-Hidden",
        ".GIT",
        ".Obſidian",
        "NoDe_MoDuLeS",
    ] {
        let operation = uuid::Uuid::now_v7().to_string();
        let body = json!({"node_id":uuid::Uuid::now_v7().to_string(),"parent_id":root,
            "name":name,"kind":"file","upload_id":upload_id});
        let response = create(server.local_addr(), &device, &body, &operation, 1).await;
        assert_eq!(response.status, 400);
        assert_eq!(value(&response)["code"], "invalid_request");
        assert_eq!(
            get(
                server.local_addr(),
                &device,
                &format!("/v1/operations/{operation}")
            )
            .await
            .status,
            404
        );
        assert!(!config.workspace.notes_root.join(name).exists());
    }
    let mut db = database(&config).await;
    let sequence: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(&device.device_stream_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(sequence, "0");
    let parent = uuid::Uuid::now_v7().to_string();
    let folder = json!({"node_id":parent,"parent_id":root,"name":"Parent","kind":"folder"});
    assert_eq!(
        create(
            server.local_addr(),
            &device,
            &folder,
            &uuid::Uuid::now_v7().to_string(),
            1
        )
        .await
        .status,
        200
    );

    let parent_path = config.workspace.notes_root.join("Parent");
    let retained_parent = directory.path().join("retained-parent");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let sentinel = b"outside sentinel must remain unchanged";
    std::fs::write(outside.join("sentinel.bin"), sentinel).unwrap();
    std::fs::rename(&parent_path, &retained_parent).unwrap();
    std::os::unix::fs::symlink(&outside, &parent_path).unwrap();

    let node = uuid::Uuid::now_v7().to_string();
    let operation = uuid::Uuid::now_v7().to_string();
    let body = json!({"node_id":node,"parent_id":parent,"name":"Escaped.bin","kind":"file","upload_id":upload_id});
    let response = create(server.local_addr(), &device, &body, &operation, 2).await;
    assert_eq!(response.status, 503);
    assert_eq!(value(&response)["code"], "storage_unavailable");
    assert_eq!(
        std::fs::read(outside.join("sentinel.bin")).unwrap(),
        sentinel
    );
    assert!(!outside.join("Escaped.bin").exists());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&retained_parent).unwrap().count(), 0);
    assert_eq!(
        get(server.local_addr(), &device, &format!("/v1/nodes/{node}"))
            .await
            .status,
        404
    );
    assert_eq!(
        get(
            server.local_addr(),
            &device,
            &format!("/v1/operations/{operation}")
        )
        .await
        .status,
        404
    );
    let sequence: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(&device.device_stream_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(sequence, "1");
    let staging: (String, Option<String>) =
        sqlx::query_as("SELECT status,pending_operation_id FROM uploads WHERE upload_id=?")
            .bind(&upload_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(staging, ("ready".into(), None));

    std::fs::remove_file(&parent_path).unwrap();
    std::fs::rename(&retained_parent, &parent_path).unwrap();
    let accepted = json!({"node_id":node,"parent_id":root,"name":"Accepted.bin","kind":"file","upload_id":upload_id});
    let response = create(
        server.local_addr(),
        &device,
        &accepted,
        &uuid::Uuid::now_v7().to_string(),
        2,
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(value(&response)["status"], "created");
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Accepted.bin")).unwrap(),
        payload
    );
    assert_eq!(
        get(
            server.local_addr(),
            &device,
            &format!("/v1/nodes/{node}/content")
        )
        .await
        .body,
        payload
    );
    assert_eq!(
        std::fs::read(outside.join("sentinel.bin")).unwrap(),
        sentinel
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_create_response_survives_restart_without_reusing_upload_or_sequence() {
    let (_directory, config, server, device, root) = setup().await;
    let response = upload(server.local_addr(), &device, b"lost response bytes").await;
    assert_eq!(response.status, 201);
    let staged = value(&response);
    let node = uuid::Uuid::now_v7().to_string();
    let operation = uuid::Uuid::now_v7().to_string();
    let body = json!({"node_id":node,"parent_id":root,"name":"lost.bin","kind":"file","upload_id":staged["upload_id"]});
    let encoded = serde_json::to_vec(&body).unwrap();
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let headers = format!(
        "POST /v1/nodes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: application/json\r\nAuthorization: Bearer {}\r\nX-Workspace-Epoch: {}\r\nX-Device-Stream: {}\r\nX-Operation-Seq: 1\r\nIdempotency-Key: {operation}\r\n\r\n",
        encoded.len(),
        device.device_token,
        device.workspace_epoch,
        device.device_stream_id
    );
    stream.write_all(headers.as_bytes()).await.unwrap();
    stream.write_all(&encoded).await.unwrap();
    let mut db = database(&config).await;
    let mut committed = false;
    for _ in 0..100 {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM operation_receipts WHERE operation_id=?")
                .bind(&operation)
                .fetch_one(&mut db)
                .await
                .unwrap();
        if count == 1 {
            committed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(committed);
    drop(stream);
    sqlx::query("UPDATE uploads SET started_at=0,expires_at=1")
        .execute(&mut db)
        .await
        .unwrap();
    drop(db);
    server.shutdown().await.unwrap();
    let server = RunningServer::start(config).await.unwrap();
    let response = create(server.local_addr(), &device, &body, &operation, 1).await;
    assert_eq!(response.status, 200);
    assert_eq!(value(&response)["status"], "created");
    assert_eq!(value(&response)["node"]["node_id"], node);
    assert_eq!(
        get(
            server.local_addr(),
            &device,
            &format!("/v1/nodes/{node}/content")
        )
        .await
        .body,
        b"lost response bytes"
    );
    assert_eq!(
        value(&get(server.local_addr(), &device, "/v1/server-info").await)["head_seq"],
        "1"
    );
    server.shutdown().await.unwrap();
}

fn put_headers(
    device: &PairResponse,
    etag: &str,
    bytes: &[u8],
    operation: &str,
) -> Vec<(String, String)> {
    vec![
        (
            "Authorization".into(),
            format!("Bearer {}", device.device_token),
        ),
        (
            "X-Workspace-Epoch".into(),
            device.workspace_epoch.to_string(),
        ),
        ("X-Device-Stream".into(), device.device_stream_id.clone()),
        ("X-Operation-Seq".into(), "2".into()),
        ("Idempotency-Key".into(), operation.into()),
        ("If-Match".into(), etag.into()),
        ("Content-Digest".into(), digest(bytes)),
    ]
}

async fn partial_put(
    address: SocketAddr,
    node: &str,
    device: &PairResponse,
    etag: &str,
    bytes: &[u8],
) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut head = format!(
        "PUT /v1/nodes/{node}/content HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\nExpect: 100-continue\r\n",
        bytes.len()
    );
    for (name, value) in put_headers(device, etag, bytes, &uuid::Uuid::now_v7().to_string()) {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut interim = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !interim.ends_with(b"\r\n\r\n") {
            interim.push(stream.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(interim.starts_with(b"HTTP/1.1 100 "));
    stream.write_all(&bytes[..1]).await.unwrap();
    stream
}

#[tokio::test]
async fn raw_put_bounds_complete_body_memory_and_releases_admission_on_cancel() {
    let (_directory, _config, server, device, root) = setup().await;
    let node = uuid::Uuid::now_v7().to_string();
    let response = create(
        server.local_addr(),
        &device,
        &json!({"node_id":node,"parent_id":root,"name":"bounded.bin","kind":"file"}),
        &uuid::Uuid::now_v7().to_string(),
        1,
    )
    .await;
    assert_eq!(response.status, 200);
    let etag = value(&response)["node"]["content_etag"]
        .as_str()
        .unwrap()
        .to_owned();
    let bytes = b"synthetic bounded PUT";
    let first = partial_put(server.local_addr(), &node, &device, &etag, bytes).await;
    let second = partial_put(server.local_addr(), &node, &device, &etag, bytes).await;
    let operation = uuid::Uuid::now_v7().to_string();
    let headers = put_headers(&device, &etag, bytes, &operation);
    let headers: Vec<_> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let response = request(
        server.local_addr(),
        "PUT",
        &format!("/v1/nodes/{node}/content"),
        &headers,
        bytes,
    )
    .await;
    assert_eq!(response.status, 429);
    drop(first);
    drop(second);
    let mut saved = false;
    for _ in 0..100 {
        let response = request(
            server.local_addr(),
            "PUT",
            &format!("/v1/nodes/{node}/content"),
            &headers,
            bytes,
        )
        .await;
        if response.status == 200 {
            saved = true;
            break;
        }
        assert_eq!(response.status, 429);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saved);
    assert_eq!(
        get(
            server.local_addr(),
            &device,
            &format!("/v1/nodes/{node}/content")
        )
        .await
        .body,
        bytes
    );
    server.shutdown().await.unwrap();
}

async fn chunked_upload(address: SocketAddr, device: &PairResponse, bytes: &[u8]) -> (u16, Value) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let head = format!(
        "POST /v1/uploads HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\nContent-Digest: {}\r\n\r\n",
        device.device_token,
        digest(bytes)
    );
    let (mut reader, mut writer) = stream.split();
    let mut response = Vec::new();
    let (written, read) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            async {
                writer.write_all(head.as_bytes()).await?;
                for chunk in bytes.chunks(32 * 1024) {
                    writer
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .await?;
                    writer.write_all(chunk).await?;
                    writer.write_all(b"\r\n").await?;
                }
                writer.write_all(b"0\r\n\r\n").await
            },
            reader.read_to_end(&mut response)
        )
    })
    .await
    .unwrap();
    read.unwrap();
    let boundary = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    let status = std::str::from_utf8(&response[..boundary])
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    assert!(written.is_ok() || status >= 400);
    (
        status,
        serde_json::from_slice(&response[boundary + 4..]).unwrap(),
    )
}

#[tokio::test]
async fn chunked_upload_streams_without_content_length_and_still_enforces_size() {
    let (_directory, _config, server, device, _root) = setup().await;
    let bytes = vec![0x9a; 65_537];
    let (status, body) = chunked_upload(server.local_addr(), &device, &bytes).await;
    assert_eq!(status, 201);
    assert_eq!(body["size"], bytes.len().to_string());
    let (status, body) = chunked_upload(
        server.local_addr(),
        &device,
        &vec![0x9b; 25 * 1024 * 1024 + 1],
    )
    .await;
    assert_eq!(status, 413);
    assert_eq!(body["code"], "upload_too_large");
    server.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "actual 120 second upload deadline over TCP"]
async fn progressing_upload_obeys_120_second_total_deadline() {
    let (_directory, config, server, device, _root) = setup().await;
    let bytes = vec![0x34; 1024 * 1024];
    let mut stream = receiving(server.local_addr(), &device, &bytes, 6 * 1024).await;
    wait_receiving(&config, 1).await;
    let started = tokio::time::Instant::now();
    let (mut reader, mut writer) = stream.split();
    let sending = async {
        loop {
            tokio::time::sleep(Duration::from_secs(4)).await;
            if writer.write_all(&bytes[..6 * 1024]).await.is_err() {
                return;
            }
        }
    };
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(130), async {
        tokio::select! {
            _ = sending => { reader.read_to_end(&mut response).await.unwrap(); }
            read = reader.read_to_end(&mut response) => { read.unwrap(); }
        }
    })
    .await
    .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 408 "));
    assert!(started.elapsed() >= Duration::from_secs(115));
    assert!(started.elapsed() < Duration::from_secs(126));
    wait_receiving(&config, 0).await;
    assert_eq!(
        upload(server.local_addr(), &device, b"after deadline")
            .await
            .status,
        201
    );
    server.shutdown().await.unwrap();
}
