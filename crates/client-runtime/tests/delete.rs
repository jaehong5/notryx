use notryx_client_runtime::{DeleteMutation, Error, OperationLookup, Session};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, OperationReceipt, WorkspaceRole};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;

async fn grant(config: &Config, principal_id: String) -> (String, String) {
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
    (workspace_id.to_string(), pairing_secret)
}

#[tokio::test]
async fn real_delete_uses_whole_node_etag_retries_and_retirement() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("example.md"), b"synthetic original\n").unwrap();
    let document = workspace.adopt_root_document("example.md").await.unwrap();
    workspace.close().await.unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state,
            uploads: Default::default(),
        },
    };
    let server = RunningServer::start(config.clone()).await.unwrap();
    let AdminResponse::Ok {
        result: AdminResult::Status {
            owner_principal_id, ..
        },
    } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
        .await
        .unwrap()
    else {
        panic!("status")
    };
    let origin = format!("http://{}", server.local_addr());
    let (workspace_id, secret) = grant(&config, owner_principal_id).await;
    let editor = Session::pair(&origin, workspace_id.parse().unwrap(), &secret)
        .await
        .unwrap();
    let node = editor
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == document.node_id)
        .unwrap();
    let stale = DeleteMutation {
        binding: editor.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        node_id: document.node_id,
        expected_node_etag: "\"stale\"".into(),
    };
    assert!(matches!(
        editor.delete_operation(&stale).await.unwrap(),
        OperationLookup::Missing
    ));
    assert!(matches!(
        editor.delete(&stale).await.unwrap(),
        OperationReceipt::NodePreconditionFailed { .. }
    ));
    assert_eq!(
        std::fs::read(notes.join("example.md")).unwrap(),
        b"synthetic original\n"
    );
    let request = DeleteMutation {
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 2,
        expected_node_etag: node.node_etag.clone(),
        ..stale.clone()
    };
    assert!(matches!(
        editor.delete_operation(&request).await.unwrap(),
        OperationLookup::Missing
    ));
    let receipt = editor.delete(&request).await.unwrap();
    assert!(
        matches!(&receipt, OperationReceipt::Deleted { node, .. } if node.node_id == document.node_id && node.deleted)
    );
    assert!(!notes.join("example.md").exists());
    assert_eq!(editor.delete(&request).await.unwrap(), receipt);
    assert_eq!(
        editor.delete_operation(&request).await.unwrap(),
        OperationLookup::Receipt(Box::new(receipt))
    );
    let wrong_retry = DeleteMutation {
        expected_node_etag: "\"changed\"".into(),
        ..request.clone()
    };
    assert_eq!(
        editor.delete(&wrong_retry).await,
        Err(Error::IdempotencyMismatch)
    );
    editor.acknowledge(2).await.unwrap();
    assert!(matches!(
        editor.delete_operation(&request).await.unwrap(),
        OperationLookup::Retired
    ));
    assert_eq!(editor.delete(&request).await, Err(Error::OperationRetired));
    let absent = DeleteMutation {
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 3,
        ..request.clone()
    };
    assert!(matches!(
        editor.abandon_delete(&absent).await.unwrap(),
        OperationReceipt::Abandoned { .. }
    ));
    assert!(matches!(
        editor.delete_operation(&absent).await.unwrap(),
        OperationLookup::Receipt(_)
    ));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_reader_is_denied_and_file_is_preserved() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("example.md"), b"preserved\n").unwrap();
    let document = workspace.adopt_root_document("example.md").await.unwrap();
    workspace.close().await.unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state,
            uploads: Default::default(),
        },
    };
    let server = RunningServer::start(config.clone()).await.unwrap();
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
    let (workspace_id, secret) = grant(&config, principal_id).await;
    let reader = Session::pair(
        &format!("http://{}", server.local_addr()),
        workspace_id.parse().unwrap(),
        &secret,
    )
    .await
    .unwrap();
    let node = reader
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == document.node_id)
        .unwrap();
    let request = DeleteMutation {
        binding: reader.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        node_id: node.node_id,
        expected_node_etag: node.node_etag,
    };
    assert_eq!(reader.delete(&request).await, Err(Error::Forbidden));
    assert_eq!(
        std::fs::read(notes.join("example.md")).unwrap(),
        b"preserved\n"
    );
    assert!(matches!(
        reader.delete_operation(&request).await.unwrap(),
        OperationLookup::Missing
    ));
    server.shutdown().await.unwrap();
}

const NODE: &str = "019a1000-0000-7000-8000-000000000002";
const PARENT: &str = "019a1000-0000-7000-8000-000000000001";
const OP: &str = "019a1000-0000-7000-8000-000000000003";
fn deleted(epoch: &str) -> Value {
    json!({"status":"deleted", "operation_id":OP, "operation_seq":"1", "workspace_epoch":epoch,
        "node":{"node_id":NODE,"parent_id":PARENT,"name":"example.md","kind":"file","deleted":true,"content_rev":"content","content_etag":"\"content\"","meta_rev":"meta","meta_etag":"\"meta\"","node_etag":"\"node-2\"","content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"11"},"commit_seq":"11"})
}
async fn mock_session<F>(reply: F) -> (Session, tokio::task::JoinHandle<String>)
where
    F: FnOnce(&str) -> (String, String, String),
{
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let workspace = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let pair = json!({"protocol":"mdw-sync/1.2","workspace_id":workspace,"workspace_epoch":epoch,"principal_id":Uuid::new_v4().to_string(),"device_id":Uuid::new_v4().to_string(),"device_stream_id":Uuid::new_v4().to_string(),"device_token":"synthetic-token","role":"editor","idle_expires_at":"1","absolute_expires_at":"2"}).to_string();
    let answer = reply(&epoch);
    let server = tokio::spawn(async move {
        let mut captured = String::new();
        for (index, (status, media, body)) in
            [("200 OK".into(), "application/json".into(), pair), answer]
                .into_iter()
                .enumerate()
        {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            if index == 1 {
                captured = String::from_utf8(request).unwrap();
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {media}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
        captured
    });
    let session = Session::pair(
        &format!("http://{address}"),
        workspace.parse().unwrap(),
        "synthetic-pairing",
    )
    .await
    .unwrap();
    (session, server)
}
fn mutation(session: &Session) -> DeleteMutation {
    DeleteMutation {
        binding: session.binding().clone(),
        operation_id: OP.parse().unwrap(),
        operation_seq: 1,
        node_id: NODE.parse().unwrap(),
        expected_node_etag: "\"node-before\"".into(),
    }
}
#[tokio::test]
async fn untrusted_delete_receipts_are_rejected_and_request_is_fixed() {
    for case in [
        "operation_id",
        "operation_seq",
        "workspace_epoch",
        "node_id",
        "not_deleted",
        "meta_etag",
        "node_etag",
        "content_etag",
        "hash",
        "commit_seq",
    ] {
        let (session, server) = mock_session(|epoch| {
            let mut v = deleted(epoch);
            match case {
                "operation_id" => v["operation_id"] = json!("019a1000-0000-7000-8000-000000000004"),
                "operation_seq" => v["operation_seq"] = json!("2"),
                "workspace_epoch" => v["workspace_epoch"] = json!(Uuid::new_v4().to_string()),
                "node_id" => v["node"]["node_id"] = json!("019a1000-0000-7000-8000-000000000004"),
                "not_deleted" => v["node"]["deleted"] = json!(false),
                "meta_etag" => v["node"]["meta_etag"] = json!("\"wrong\""),
                "node_etag" => v["node"]["node_etag"] = json!("W/\"weak\""),
                "content_etag" => v["node"]["content_etag"] = json!("\"wrong\""),
                "hash" => v["node"]["content_hash"] = json!("sha256:broken"),
                "commit_seq" => v["commit_seq"] = json!("12"),
                _ => unreachable!(),
            }
            ("200 OK".into(), "application/json".into(), v.to_string())
        })
        .await;
        assert!(
            matches!(
                session.delete(&mutation(&session)).await,
                Err(Error::Binding | Error::Contract)
            ),
            "{case}"
        );
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with(&format!("delete /v1/nodes/{NODE} ")));
        assert!(request.contains("authorization: bearer synthetic-token"));
        assert!(request.contains("if-match: \"node-before\""));
        assert!(request.contains("x-operation-seq: 1"));
        assert!(request.contains(&format!("idempotency-key: {OP}")));
    }
}
#[tokio::test]
async fn delete_rejects_wrong_status_and_distinguishes_retired() {
    let (session, server) = mock_session(|epoch| {
        let operation = json!({"status":"node_precondition_failed", "operation_id":OP, "operation_seq":"1", "workspace_epoch":epoch, "current_node_etag":"\"current\""});
        ("409 Conflict".into(), "application/json".into(), json!({"code":"node_revision_mismatch","message":"synthetic","request_id":Uuid::now_v7().to_string(),"operation":operation}).to_string())
    }).await;
    assert_eq!(
        session.delete(&mutation(&session)).await,
        Err(Error::Contract)
    );
    server.await.unwrap();
    let (session, server) = mock_session(|_| ("410 Gone".into(), "application/json".into(), json!({"code":"operation_retired","message":"synthetic","request_id":Uuid::now_v7().to_string()}).to_string())).await;
    assert!(matches!(
        session.delete_operation(&mutation(&session)).await.unwrap(),
        OperationLookup::Retired
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn delete_rejects_precondition_failure_with_unchanged_etag() {
    let (session, server) = mock_session(|epoch| {
        let operation = json!({"status":"node_precondition_failed", "operation_id":OP, "operation_seq":"1", "workspace_epoch":epoch, "current_node_etag":"\"node-before\""});
        ("412 Precondition Failed".into(), "application/json".into(), json!({"code":"node_revision_mismatch","message":"synthetic","request_id":Uuid::now_v7().to_string(),"operation":operation}).to_string())
    }).await;
    assert_eq!(
        session.delete(&mutation(&session)).await,
        Err(Error::Contract)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn lost_delete_response_is_recovered_by_lookup_using_same_identity() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("example.md"), b"committed before loss\n").unwrap();
    let document = workspace.adopt_root_document("example.md").await.unwrap();
    let node = notryx_server::NodeSnapshot::from(&document)
        .representation()
        .unwrap();
    workspace.close().await.unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state,
            uploads: Default::default(),
        },
    };
    let server = RunningServer::start(config.clone()).await.unwrap();
    let AdminResponse::Ok {
        result: AdminResult::Status {
            owner_principal_id, ..
        },
    } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
        .await
        .unwrap()
    else {
        panic!("status")
    };
    let (workspace_id, secret) = grant(&config, owner_principal_id).await;
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", proxy.local_addr().unwrap());
    let target = server.local_addr();
    let proxy_task = tokio::spawn(async move {
        let mut paths = Vec::new();
        for _ in 0..3 {
            let (mut downstream, _) = proxy.accept().await.unwrap();
            let mut request = Vec::new();
            let end;
            loop {
                let mut buffer = [0; 4096];
                let n = downstream.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(pos) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..pos]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= pos + 4 + length {
                        end = pos;
                        break;
                    }
                }
            }
            let line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap()
                .to_owned();
            let drop_response = line.starts_with("DELETE ");
            paths.push(line);
            let mut upstream = TcpStream::connect(target).await.unwrap();
            upstream.write_all(&request[..end]).await.unwrap();
            upstream
                .write_all(b"\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            upstream.write_all(&request[end + 4..]).await.unwrap();
            let mut response = Vec::new();
            upstream.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
            if !drop_response {
                downstream.write_all(&response).await.unwrap();
            }
        }
        paths
    });
    let session = Session::pair(&origin, workspace_id.parse().unwrap(), &secret)
        .await
        .unwrap();
    let request = DeleteMutation {
        binding: session.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        node_id: document.node_id,
        expected_node_etag: node.node_etag,
    };
    assert_eq!(session.delete(&request).await, Err(Error::Network));
    assert!(!notes.join("example.md").exists());
    assert!(
        matches!(session.delete_operation(&request).await.unwrap(), OperationLookup::Receipt(receipt) if matches!(*receipt, OperationReceipt::Deleted { operation_id, operation_seq, .. } if operation_id == request.operation_id && operation_seq.get() == request.operation_seq))
    );
    let paths = proxy_task.await.unwrap();
    assert!(paths[1].starts_with("DELETE "));
    assert!(paths[2].starts_with(&format!("GET /v1/operations/{} ", request.operation_id)));
    server.shutdown().await.unwrap();
}
