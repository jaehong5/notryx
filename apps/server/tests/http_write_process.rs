pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::PairResponse;
use notryx_server::{Workspace, config::Config};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use support::{initialized, pairing_request, post_json, request};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

struct ServerProcess {
    child: Option<Child>,
    output: Option<std::thread::JoinHandle<()>>,
}

impl ServerProcess {
    fn start(config_path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .arg("serve")
            .arg("--config")
            .arg(config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let output = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line == "server_started" {
                    let _ = sender.send(());
                }
            }
        });
        let server = Self {
            child: Some(child),
            output: Some(output),
        };
        assert!(
            receiver.recv_timeout(Duration::from_secs(20)).is_ok(),
            "isolated server must report startup within twenty seconds"
        );
        server
    }

    fn kill(&mut self) -> ExitStatus {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        let status = child.wait().unwrap();
        self.output.take().unwrap().join().unwrap();
        status
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
    }
}

fn process_config(directory: &Path, config: &mut Config) -> std::path::PathBuf {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.server.listen = reservation.local_addr().unwrap();
    let path = directory.join("server.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\nlisten='{}'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n",
            config.server.listen,
            config.server.admin_socket.display(),
            config.workspace.notes_root.display(),
            config.workspace.state_root.display()
        ),
    )
    .unwrap();
    path
}

fn write_headers(
    paired: &PairResponse,
    operation: &str,
    sequence: &str,
    etag: &str,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {}", paired.device_token)),
        ("X-Workspace-Epoch", paired.workspace_epoch.to_string()),
        ("X-Device-Stream", paired.device_stream_id.clone()),
        ("X-Operation-Seq", sequence.into()),
        ("Idempotency-Key", operation.into()),
        ("If-Match", etag.into()),
        (
            "Content-Digest",
            format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(body))),
        ),
        ("Content-Type", "application/octet-stream".into()),
    ]
}

async fn send_without_reading_response(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> TcpStream {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut connection = TcpStream::connect(address).await.unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        connection.write_all(head.as_bytes()).await.unwrap();
        connection.write_all(body).await.unwrap();
        connection
    })
    .await
    .expect("isolated request must be sent within five seconds")
}

#[tokio::test]
async fn unread_move_survives_sigkill_without_moving_the_node_twice() {
    let (directory, mut config) = initialized().await;
    let bytes = b"\xef\xbb\xbfsource\r\n\xff";
    std::fs::write(config.workspace.notes_root.join("Original.md"), bytes).unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace.adopt_root_document("Original.md").await.unwrap();
    workspace.close().await.unwrap();
    let config_path = process_config(directory.path(), &mut config);
    let mut server = ServerProcess::start(&config_path);
    let address = config.server.listen;
    let (pair, _, _) = pairing_request(&config).await;
    let response = post_json(address, &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let paired: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let authorization = format!("Bearer {}", paired.device_token);
    let auth = [("Authorization", authorization.as_str())];
    let path = format!("/v1/nodes/{}/metadata", document.node_id);
    let before = request(address, "GET", &path, &auth, b"").await;
    assert_eq!(before.status, 200);
    let operation_id = uuid::Uuid::now_v7().to_string();
    let operation_path = format!("/v1/operations/{operation_id}");
    let epoch = paired.workspace_epoch.to_string();
    let headers = [
        ("Authorization", authorization.as_str()),
        ("X-Workspace-Epoch", epoch.as_str()),
        ("X-Device-Stream", paired.device_stream_id.as_str()),
        ("X-Operation-Seq", "1"),
        ("Idempotency-Key", operation_id.as_str()),
        ("If-Match", before.header("etag").unwrap()),
        ("Content-Type", "application/json"),
    ];
    let body = serde_json::to_vec(&serde_json::json!({
        "expected_parent_id": document.parent_id,
        "parent_id": document.parent_id,
        "name": "Moved.md"
    }))
    .unwrap();
    let unread = send_without_reading_response(address, "PATCH", &path, &headers, &body).await;
    let receipt = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = request(address, "GET", &operation_path, &auth, b"").await;
            if result.status == 200 {
                let value: Value = serde_json::from_slice(&result.body).unwrap();
                if value["status"] == "moved" {
                    break value;
                }
                assert_eq!(value["status"], "pending");
            } else {
                assert_eq!(result.status, 404);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("move must commit within twenty seconds");
    drop(unread);
    assert_eq!(server.kill().signal(), Some(9));
    let mut restarted = ServerProcess::start(&config_path);
    let replay = request(address, "PATCH", &path, &headers, &body).await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap(),
        receipt
    );
    let found = request(address, "GET", &operation_path, &auth, b"").await;
    assert_eq!(found.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&found.body).unwrap(),
        receipt
    );
    assert!(!config.workspace.notes_root.join("Original.md").exists());
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Moved.md")).unwrap(),
        bytes
    );
    let info = request(address, "GET", "/v1/server-info", &auth, b"").await;
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(info["head_seq"], receipt["commit_seq"]);
    assert_eq!(restarted.kill().signal(), Some(9));
}

async fn await_receipt(
    address: SocketAddr,
    operation: &str,
    auth: &[(&str, &str)],
    status: &str,
) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let response = request(
                address,
                "GET",
                &format!("/v1/operations/{operation}"),
                auth,
                b"",
            )
            .await;
            if response.status == 200 {
                let value: Value = serde_json::from_slice(&response.body).unwrap();
                if value["status"] == status {
                    break value;
                }
                assert_eq!(value["status"], "pending");
            } else {
                assert_eq!(response.status, 404);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("isolated operation must commit within twenty seconds")
}

async fn assert_head(address: SocketAddr, auth: &[(&str, &str)], expected: &Value) {
    let info = request(address, "GET", "/v1/server-info", auth, b"").await;
    assert_eq!(info.status, 200);
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(&info["head_seq"], expected);
}

#[tokio::test]
async fn unread_delete_and_restore_survive_sigkill_and_replay_after_another_deletion() {
    let (directory, mut config) = initialized().await;
    let bytes = b"\xef\xbb\xbfretained\r\n\xff";
    std::fs::write(config.workspace.notes_root.join("Original.md"), bytes).unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace.adopt_root_document("Original.md").await.unwrap();
    workspace.close().await.unwrap();
    let config_path = process_config(directory.path(), &mut config);
    let mut server = ServerProcess::start(&config_path);
    let address = config.server.listen;
    let (pair, _, _) = pairing_request(&config).await;
    let response = post_json(address, &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let paired: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let authorization = format!("Bearer {}", paired.device_token);
    let epoch = paired.workspace_epoch.to_string();
    let auth = [("Authorization", authorization.as_str())];
    let path = format!("/v1/nodes/{}", document.node_id);
    let before = request(address, "GET", &path, &auth, b"").await;
    assert_eq!(before.status, 200);
    let deletion = uuid::Uuid::now_v7().to_string();
    let delete_headers = [
        ("Authorization", authorization.as_str()),
        ("X-Workspace-Epoch", epoch.as_str()),
        ("X-Device-Stream", paired.device_stream_id.as_str()),
        ("X-Operation-Seq", "1"),
        ("Idempotency-Key", deletion.as_str()),
        ("If-Match", before.header("etag").unwrap()),
    ];
    let unread =
        send_without_reading_response(address, "DELETE", &path, &delete_headers, b"").await;
    let deleted = await_receipt(address, &deletion, &auth, "deleted").await;
    drop(unread);
    assert_eq!(server.kill().signal(), Some(9));
    let mut server = ServerProcess::start(&config_path);
    let persisted = request(
        address,
        "GET",
        &format!("/v1/operations/{deletion}"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(persisted.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&persisted.body).unwrap(),
        deleted
    );
    let replay = request(address, "DELETE", &path, &delete_headers, b"").await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap(),
        deleted
    );
    assert_head(address, &auth, &deleted["commit_seq"]).await;
    assert!(!config.workspace.notes_root.join("Original.md").exists());
    let restoration = uuid::Uuid::now_v7().to_string();
    let restore_path = format!("{path}/restore");
    let restore_headers = [
        ("Authorization", authorization.as_str()),
        ("X-Workspace-Epoch", epoch.as_str()),
        ("X-Device-Stream", paired.device_stream_id.as_str()),
        ("X-Operation-Seq", "2"),
        ("Idempotency-Key", restoration.as_str()),
        ("Content-Type", "application/json"),
    ];
    let body = serde_json::to_vec(&serde_json::json!({
        "expected_meta_rev": deleted["node"]["meta_rev"],
        "parent_id": document.parent_id,
        "name": "Restored.md"
    }))
    .unwrap();
    let unread =
        send_without_reading_response(address, "POST", &restore_path, &restore_headers, &body)
            .await;
    let restored = await_receipt(address, &restoration, &auth, "restored").await;
    drop(unread);
    assert_eq!(server.kill().signal(), Some(9));
    let mut server = ServerProcess::start(&config_path);
    let persisted = request(
        address,
        "GET",
        &format!("/v1/operations/{restoration}"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(persisted.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&persisted.body).unwrap(),
        restored
    );
    let replay = request(address, "POST", &restore_path, &restore_headers, &body).await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap(),
        restored
    );
    assert_head(address, &auth, &restored["commit_seq"]).await;
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Restored.md")).unwrap(),
        bytes
    );
    let next = uuid::Uuid::now_v7().to_string();
    let later_headers = [
        ("Authorization", authorization.as_str()),
        ("X-Workspace-Epoch", epoch.as_str()),
        ("X-Device-Stream", paired.device_stream_id.as_str()),
        ("X-Operation-Seq", "3"),
        ("Idempotency-Key", next.as_str()),
        ("If-Match", restored["node"]["node_etag"].as_str().unwrap()),
    ];
    let later = request(address, "DELETE", &path, &later_headers, b"").await;
    assert_eq!(later.status, 200);
    let later: Value = serde_json::from_slice(&later.body).unwrap();
    let replay = request(address, "POST", &restore_path, &restore_headers, &body).await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap(),
        restored
    );
    assert_head(address, &auth, &later["commit_seq"]).await;
    assert!(!config.workspace.notes_root.join("Restored.md").exists());
    let tombstone = request(address, "GET", &path, &auth, b"").await;
    assert_eq!(tombstone.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&tombstone.body).unwrap()["deleted"],
        true
    );
    assert_eq!(server.kill().signal(), Some(9));
}

#[tokio::test]
async fn unread_create_survives_sigkill_and_replay_preserves_later_content() {
    let (directory, mut config) = initialized().await;
    let config_path = process_config(directory.path(), &mut config);
    let mut server = ServerProcess::start(&config_path);
    let address = config.server.listen;
    let (pair, _, _) = pairing_request(&config).await;
    let response = post_json(address, &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let paired: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let authorization = format!("Bearer {}", paired.device_token);
    let auth = [("Authorization", authorization.as_str())];
    let info = request(address, "GET", "/v1/server-info", &auth, b"").await;
    assert_eq!(info.status, 200);
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    let folder_id = uuid::Uuid::now_v7().to_string();
    let folder_operation = uuid::Uuid::now_v7().to_string();
    let create_headers = |operation: &str, sequence: &str| {
        vec![
            ("Authorization", authorization.clone()),
            ("X-Workspace-Epoch", paired.workspace_epoch.to_string()),
            ("X-Device-Stream", paired.device_stream_id.clone()),
            ("X-Operation-Seq", sequence.to_owned()),
            ("Idempotency-Key", operation.to_owned()),
            ("Content-Type", "application/json".to_owned()),
        ]
    };
    let folder_headers = create_headers(&folder_operation, "1");
    let folder_headers: Vec<_> = folder_headers
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    let folder_body = serde_json::to_vec(&serde_json::json!({
        "node_id": folder_id, "parent_id": info["root_id"], "name": "Folder", "kind": "folder"
    }))
    .unwrap();
    let folder = request(address, "POST", "/v1/nodes", &folder_headers, &folder_body).await;
    assert_eq!(folder.status, 200, "folder creation must be supported");
    let folder: Value = serde_json::from_slice(&folder.body).unwrap();
    assert_eq!(folder["status"], "created");

    let file_id = uuid::Uuid::now_v7().to_string();
    let operation_id = uuid::Uuid::now_v7().to_string();
    let operation_path = format!("/v1/operations/{operation_id}");
    let body = serde_json::to_vec(&serde_json::json!({
        "node_id": file_id, "parent_id": folder_id, "name": "Document.md", "kind": "file"
    }))
    .unwrap();
    let headers = create_headers(&operation_id, "2");
    let headers: Vec<_> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let unread = send_without_reading_response(address, "POST", "/v1/nodes", &headers, &body).await;
    let receipt = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = request(address, "GET", &operation_path, &auth, b"").await;
            if result.status == 200 {
                let value: Value = serde_json::from_slice(&result.body).unwrap();
                if value["status"] == "created" {
                    break value;
                }
                assert_eq!(value["status"], "pending");
            } else {
                assert_eq!(result.status, 404);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("creation must commit within twenty seconds");
    drop(unread);
    assert_eq!(server.kill().signal(), Some(9));

    let mut restarted = ServerProcess::start(&config_path);
    let found = request(address, "GET", &operation_path, &auth, b"").await;
    assert_eq!(found.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&found.body).unwrap(),
        receipt
    );
    let replay = request(address, "POST", "/v1/nodes", &headers, &body).await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap(),
        receipt
    );
    let info = request(address, "GET", "/v1/server-info", &auth, b"").await;
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(info["head_seq"], receipt["commit_seq"]);
    let path = format!("/v1/nodes/{file_id}/content");
    let empty = request(address, "GET", &path, &auth, b"").await;
    assert_eq!(empty.status, 200);
    assert!(empty.body.is_empty());
    let bytes = b"\xef\xbb\xbflater raw content\r\n\xff\0";
    let save_id = uuid::Uuid::now_v7().to_string();
    let save_headers = write_headers(&paired, &save_id, "3", empty.header("etag").unwrap(), bytes);
    let save_headers: Vec<_> = save_headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let saved = request(address, "PUT", &path, &save_headers, bytes).await;
    assert_eq!(saved.status, 200);
    let saved: Value = serde_json::from_slice(&saved.body).unwrap();
    let replay = request(address, "POST", "/v1/nodes", &headers, &body).await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap(),
        receipt
    );
    let ack_headers = [
        ("Authorization", authorization.as_str()),
        ("Content-Type", "application/json"),
    ];
    let ack_body = serde_json::to_vec(&serde_json::json!({
        "workspace_epoch": paired.workspace_epoch, "device_stream_id": paired.device_stream_id, "ack_through": "3"
    })).unwrap();
    let ack = request(
        address,
        "POST",
        "/v1/operation-stream/ack",
        &ack_headers,
        &ack_body,
    )
    .await;
    assert_eq!(ack.status, 200);
    let retired = request(address, "POST", "/v1/nodes", &headers, &body).await;
    assert_eq!(retired.status, 410);
    assert_eq!(restarted.kill().signal(), Some(9));

    let mut restarted = ServerProcess::start(&config_path);
    let current = request(address, "GET", &path, &auth, b"").await;
    assert_eq!(current.status, 200);
    assert_eq!(current.body, bytes);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Folder/Document.md")).unwrap(),
        bytes
    );
    let info = request(address, "GET", "/v1/server-info", &auth, b"").await;
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(info["head_seq"], saved["commit_seq"]);
    assert_eq!(restarted.kill().signal(), Some(9));
}

#[tokio::test]
async fn unread_write_response_survives_sigkill_and_replays_without_a_second_commit() {
    let (directory, mut config) = initialized().await;
    let source_path = config.workspace.notes_root.join("Document.md");
    std::fs::write(&source_path, b"original\r\n").unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    let config_path = process_config(directory.path(), &mut config);
    let mut server = ServerProcess::start(&config_path);
    let address = config.server.listen;
    let (pair, _, _) = pairing_request(&config).await;
    let response = post_json(address, &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let paired: PairResponse = serde_json::from_slice(&response.body)
        .unwrap_or_else(|_| panic!("pairing response must be valid"));
    let authorization = format!("Bearer {}", paired.device_token);
    let auth = [("Authorization", authorization.as_str())];
    let path = format!("/v1/nodes/{}/content", document.node_id);
    let before = request(address, "GET", &path, &auth, b"").await;
    assert_eq!(before.status, 200);

    let first_id = uuid::Uuid::now_v7().to_string();
    let first_body = b"\xef\xbb\xbffirst saved bytes\r\n";
    let first_headers = write_headers(
        &paired,
        &first_id,
        "1",
        before.header("etag").unwrap(),
        first_body,
    );
    let first_headers: Vec<_> = first_headers
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    let first = request(address, "PUT", &path, &first_headers, first_body).await;
    assert_eq!(first.status, 200, "authenticated PUT must commit");
    let first: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(first["status"], "committed");

    let operation_id = uuid::Uuid::now_v7().to_string();
    let operation_path = format!("/v1/operations/{operation_id}");
    let body = b"\xef\xbb\xbfsecond saved bytes\r\n\xff\0\r\n";
    let headers = write_headers(
        &paired,
        &operation_id,
        "2",
        first["content_etag"].as_str().unwrap(),
        body,
    );
    let headers: Vec<_> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    // A separate observer detects commit; the writing client never consumes its response.
    let unread = send_without_reading_response(address, "PUT", &path, &headers, body).await;
    let receipt = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = request(address, "GET", &operation_path, &auth, b"").await;
            if result.status == 200 {
                let value: Value = serde_json::from_slice(&result.body).unwrap();
                if value["status"] == "committed" {
                    break value;
                }
                assert_eq!(value["status"], "pending");
            } else {
                assert_eq!(result.status, 404);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sent operation must commit within twenty seconds");
    drop(unread);
    assert_eq!(server.kill().signal(), Some(9));

    let mut restarted = ServerProcess::start(&config_path);
    let found = request(address, "GET", &operation_path, &auth, b"").await;
    assert_eq!(found.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&found.body).unwrap(),
        receipt
    );
    let replay = request(address, "PUT", &path, &headers, body).await;
    assert_eq!(replay.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap(),
        receipt
    );
    let old = request(
        address,
        "GET",
        &format!("/v1/operations/{first_id}"),
        &auth,
        b"",
    )
    .await;
    assert_eq!(old.status, 200);
    assert_eq!(serde_json::from_slice::<Value>(&old.body).unwrap(), first);
    let content = request(address, "GET", &path, &auth, b"").await;
    assert_eq!(content.status, 200);
    assert_eq!(content.body, body);
    let info = request(address, "GET", "/v1/server-info", &auth, b"").await;
    assert_eq!(info.status, 200);
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(info["head_seq"], receipt["commit_seq"]);
    assert_eq!(std::fs::read(source_path).unwrap(), body);
    assert_eq!(restarted.kill().signal(), Some(9));
}

#[tokio::test]
async fn unread_ack_survives_sigkill_and_retired_operations_never_run_again() {
    let (directory, mut config) = initialized().await;
    let source = config.workspace.notes_root.join("Document.md");
    std::fs::write(&source, b"original\r\n").unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    let config_path = process_config(directory.path(), &mut config);
    let mut server = ServerProcess::start(&config_path);
    let address = config.server.listen;
    let (pair, _, _) = pairing_request(&config).await;
    let response = post_json(address, &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    let paired: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
    let authorization = format!("Bearer {}", paired.device_token);
    let auth = [("Authorization", authorization.as_str())];
    let json_headers = [
        ("Authorization", authorization.as_str()),
        ("Content-Type", "application/json"),
    ];
    let ack = |sequence: &str| {
        serde_json::to_vec(&serde_json::json!({
            "workspace_epoch": paired.workspace_epoch,
            "device_stream_id": paired.device_stream_id,
            "ack_through": sequence,
        }))
        .unwrap()
    };
    let probe = request(
        address,
        "POST",
        "/v1/operation-stream/ack",
        &json_headers,
        &ack("0"),
    )
    .await;
    assert_eq!(
        probe.status, 200,
        "ACK of the empty prefix must be supported"
    );
    let path = format!("/v1/nodes/{}/content", document.node_id);
    let first_id = uuid::Uuid::now_v7().to_string();
    let bytes = b"\xef\xbb\xbfretained source\r\n";
    let first_headers = write_headers(
        &paired,
        &first_id,
        "1",
        &format!("\"{}\"", document.content_rev.as_str()),
        bytes,
    );
    let first_headers: Vec<_> = first_headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let saved = request(address, "PUT", &path, &first_headers, bytes).await;
    assert_eq!(saved.status, 200);
    let saved: Value = serde_json::from_slice(&saved.body).unwrap();
    let operation_path = format!("/v1/operations/{first_id}");
    let unread = send_without_reading_response(
        address,
        "POST",
        "/v1/operation-stream/ack",
        &json_headers,
        &ack("1"),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let response = request(address, "GET", &operation_path, &auth, b"").await;
            if response.status == 410 {
                break;
            }
            assert_eq!(response.status, 200);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ACK must retire the result within twenty seconds");
    drop(unread);
    assert_eq!(server.kill().signal(), Some(9));

    let mut server = ServerProcess::start(&config_path);
    assert_eq!(
        request(address, "GET", &operation_path, &auth, b"")
            .await
            .status,
        410
    );
    let replay = request(address, "PUT", &path, &first_headers, bytes).await;
    assert_eq!(replay.status, 410);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap()["code"],
        "operation_retired"
    );
    let reused = write_headers(
        &paired,
        &first_id,
        "2",
        saved["content_etag"].as_str().unwrap(),
        bytes,
    );
    let reused: Vec<_> = reused
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let replay = request(address, "PUT", &path, &reused, bytes).await;
    assert_eq!(replay.status, 409);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.body).unwrap()["code"],
        "idempotency_mismatch"
    );

    let cancelled_id = uuid::Uuid::now_v7().to_string();
    let cancel = serde_json::to_vec(&serde_json::json!({
        "workspace_epoch": paired.workspace_epoch,
        "device_stream_id": paired.device_stream_id,
        "operation_id": cancelled_id,
        "operation_seq": "2",
    }))
    .unwrap();
    let cancelled = request(
        address,
        "POST",
        "/v1/operation-stream/abandon",
        &json_headers,
        &cancel,
    )
    .await;
    assert_eq!(cancelled.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&cancelled.body).unwrap()["status"],
        "abandoned"
    );
    let resurrect = write_headers(
        &paired,
        &cancelled_id,
        "2",
        saved["content_etag"].as_str().unwrap(),
        b"must not run",
    );
    let resurrect: Vec<_> = resurrect
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    assert_eq!(
        request(address, "PUT", &path, &resurrect, b"must not run")
            .await
            .status,
        409
    );
    let acknowledged = request(
        address,
        "POST",
        "/v1/operation-stream/ack",
        &json_headers,
        &ack("2"),
    )
    .await;
    assert_eq!(acknowledged.status, 200);
    assert_eq!(server.kill().signal(), Some(9));

    let mut server = ServerProcess::start(&config_path);
    let stale = request(
        address,
        "POST",
        "/v1/operation-stream/ack",
        &json_headers,
        &ack("1"),
    )
    .await;
    assert_eq!(stale.status, 200);
    let state: Value = serde_json::from_slice(&stale.body).unwrap();
    assert_eq!(state["last_seq"], "2");
    assert_eq!(state["retired_through"], "2");
    assert_eq!(
        request(
            address,
            "GET",
            &format!("/v1/operations/{cancelled_id}"),
            &auth,
            b""
        )
        .await
        .status,
        410
    );
    let info = request(address, "GET", "/v1/server-info", &auth, b"").await;
    assert_eq!(info.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&info.body).unwrap()["head_seq"],
        saved["commit_seq"]
    );
    assert_eq!(std::fs::read(source).unwrap(), bytes);
    assert_eq!(server.kill().signal(), Some(9));
}
