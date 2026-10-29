use notryx_client_runtime::{CreateMutation, Error, MoveMutation, OperationLookup, Session};
use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, CreateNodeRequest, MoveNodeRequest, NodeKind,
    NodeState, OperationReceipt, WorkspaceRole,
};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
use uuid::Uuid;

async fn pair(config: &Config, origin: &str, principal_id: String) -> Session {
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
    Session::pair(origin, workspace_id, &pairing_secret)
        .await
        .unwrap()
}

struct Rig {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    server: RunningServer,
    editor: Session,
    reader: Session,
    source: NodeState,
    existing: NodeState,
}

async fn rig() -> Rig {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("source.md"), b"# synthetic source\n").unwrap();
    std::fs::write(notes.join("existing.md"), b"# synthetic existing\n").unwrap();
    let source = workspace
        .adopt_root_document("source.md")
        .await
        .unwrap()
        .node_id;
    let existing = workspace
        .adopt_root_document("existing.md")
        .await
        .unwrap()
        .node_id;
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
    let origin = format!("http://{}", server.local_addr());
    let editor = pair(&config, &origin, owner_principal_id).await;
    let reader = pair(&config, &origin, principal_id).await;
    let nodes = editor.inventory(None).await.unwrap().nodes;
    let source = nodes
        .iter()
        .find(|node| node.node_id == source)
        .unwrap()
        .clone();
    let existing = nodes
        .iter()
        .find(|node| node.node_id == existing)
        .unwrap()
        .clone();
    Rig {
        _directory: directory,
        notes,
        server,
        editor,
        reader,
        source,
        existing,
    }
}

fn movement(
    session: &Session,
    source: &NodeState,
    sequence: u64,
    parent: notryx_domain::NodeId,
    name: &str,
) -> MoveMutation {
    MoveMutation {
        binding: session.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: sequence,
        node_id: source.node_id,
        expected_meta_etag: source.meta_etag.clone(),
        request: MoveNodeRequest {
            expected_parent_id: source.parent_id.unwrap(),
            parent_id: parent,
            name: name.into(),
        },
    }
}

#[tokio::test]
async fn real_move_preserves_bytes_identity_and_recovers_receipt() {
    let rig = rig().await;
    let root = rig.source.parent_id.unwrap();
    let folder_id = Uuid::now_v7().to_string().parse().unwrap();
    let create = CreateMutation {
        binding: rig.editor.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        request: CreateNodeRequest {
            node_id: folder_id,
            parent_id: root,
            name: "destination".into(),
            kind: NodeKind::Folder,
            upload_id: None,
        },
    };
    assert!(matches!(
        rig.editor.create(&create).await.unwrap(),
        OperationReceipt::Created { .. }
    ));
    let request = movement(&rig.editor, &rig.source, 2, folder_id, "renamed.md");
    assert_eq!(
        rig.editor.move_operation(&request).await.unwrap(),
        OperationLookup::Missing
    );
    let receipt = rig.editor.move_node(&request).await.unwrap();
    assert!(matches!(&receipt, OperationReceipt::Moved { node, .. }
        if node.node_id == rig.source.node_id && node.parent_id == Some(folder_id)
        && node.name == "renamed.md" && node.content_rev == rig.source.content_rev
        && node.content_hash == rig.source.content_hash));
    assert!(!rig.notes.join("source.md").exists());
    assert_eq!(
        std::fs::read(rig.notes.join("destination/renamed.md")).unwrap(),
        b"# synthetic source\n"
    );
    assert_eq!(rig.editor.move_node(&request).await.unwrap(), receipt);
    assert_eq!(
        rig.editor.move_operation(&request).await.unwrap(),
        OperationLookup::Receipt(Box::new(receipt))
    );
    let mut changed = request.clone();
    changed.request.name = "different.md".into();
    assert_eq!(
        rig.editor.move_node(&changed).await,
        Err(Error::IdempotencyMismatch)
    );
    rig.editor.acknowledge(2).await.unwrap();
    assert_eq!(
        rig.editor.move_operation(&request).await.unwrap(),
        OperationLookup::Retired
    );
    assert_eq!(
        rig.editor.move_node(&request).await,
        Err(Error::OperationRetired)
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_move_reports_conflict_and_stale_metadata_without_overwriting() {
    let rig = rig().await;
    let parent = rig.source.parent_id.unwrap();
    let collision = movement(&rig.editor, &rig.source, 1, parent, "existing.md");
    assert!(matches!(
        rig.editor.move_node(&collision).await.unwrap(),
        OperationReceipt::DestinationExists { .. }
    ));
    let mut stale = movement(&rig.editor, &rig.source, 2, parent, "other.md");
    stale.expected_meta_etag = "\"stale\"".into();
    assert!(matches!(
        rig.editor.move_node(&stale).await.unwrap(),
        OperationReceipt::MetadataPreconditionFailed { .. }
    ));
    assert_eq!(
        std::fs::read(rig.notes.join("source.md")).unwrap(),
        b"# synthetic source\n"
    );
    assert_eq!(
        std::fs::read(rig.notes.join("existing.md")).unwrap(),
        b"# synthetic existing\n"
    );
    assert!(!rig.notes.join("other.md").exists());
    assert_ne!(rig.source.node_id, rig.existing.node_id);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_and_foreign_binding_cannot_move_or_admit_operation() {
    let rig = rig().await;
    let request = movement(
        &rig.reader,
        &rig.source,
        1,
        rig.source.parent_id.unwrap(),
        "denied.md",
    );
    assert_eq!(rig.reader.move_node(&request).await, Err(Error::Forbidden));
    assert_eq!(
        rig.reader.move_operation(&request).await.unwrap(),
        OperationLookup::Missing
    );
    assert_eq!(rig.editor.move_node(&request).await, Err(Error::Binding));
    assert_eq!(
        std::fs::read(rig.notes.join("source.md")).unwrap(),
        b"# synthetic source\n"
    );
    assert!(!rig.notes.join("denied.md").exists());
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn same_path_move_is_a_valid_noop_with_existing_meta_revision() {
    let rig = rig().await;
    let parent = rig.source.parent_id.unwrap();
    let request = movement(&rig.editor, &rig.source, 1, parent, "source.md");
    let receipt = rig.editor.move_node(&request).await.unwrap();
    assert!(matches!(&receipt, OperationReceipt::Moved { node, .. }
        if node.node_id == rig.source.node_id && node.meta_etag == rig.source.meta_etag
        && node.content_rev == rig.source.content_rev));
    assert_eq!(
        std::fs::read(rig.notes.join("source.md")).unwrap(),
        b"# synthetic source\n"
    );
    rig.server.shutdown().await.unwrap();
}

const NODE: &str = "019a1000-0000-7000-8000-000000000002";
const PARENT: &str = "019a1000-0000-7000-8000-000000000001";
const OP: &str = "019a1000-0000-7000-8000-000000000003";
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

const DESTINATION: &str = "019a1000-0000-7000-8000-000000000004";
fn mock_move(session: &Session) -> MoveMutation {
    MoveMutation {
        binding: session.binding().clone(),
        operation_id: OP.parse().unwrap(),
        operation_seq: 1,
        node_id: NODE.parse().unwrap(),
        expected_meta_etag: "\"meta-before\"".into(),
        request: MoveNodeRequest {
            expected_parent_id: PARENT.parse().unwrap(),
            parent_id: DESTINATION.parse().unwrap(),
            name: "moved.md".into(),
        },
    }
}
fn moved(epoch: &str) -> Value {
    json!({"status":"moved", "operation_id":OP, "operation_seq":"1", "workspace_epoch":epoch,
        "node":{"node_id":NODE,"parent_id":DESTINATION,"name":"moved.md","kind":"file","deleted":false,"content_rev":"content","content_etag":"\"content\"","meta_rev":"meta-after","meta_etag":"\"meta-after\"","node_etag":"\"node-2\"","content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"11"},"commit_seq":"11"})
}

#[tokio::test]
async fn changed_parent_with_unchanged_metadata_etag_is_rejected() {
    let (session, server) = mock_session(|epoch| {
        let mut value = moved(epoch);
        value["node"]["meta_rev"] = json!("meta-before");
        value["node"]["meta_etag"] = json!("\"meta-before\"");
        (
            "200 OK".into(),
            "application/json".into(),
            value.to_string(),
        )
    })
    .await;
    assert_eq!(
        session.move_node(&mock_move(&session)).await,
        Err(Error::Contract)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn malicious_move_receipts_are_rejected_and_request_is_fixed() {
    for case in [
        "operation_id",
        "operation_seq",
        "workspace_epoch",
        "node_id",
        "parent_id",
        "name",
        "deleted",
        "meta_etag",
        "node_etag",
        "content_etag",
        "hash",
        "commit_seq",
    ] {
        let (session, server) = mock_session(|epoch| {
            let mut value = moved(epoch);
            match case {
                "operation_id" => {
                    value["operation_id"] = json!("019a1000-0000-7000-8000-000000000005")
                }
                "operation_seq" => value["operation_seq"] = json!("2"),
                "workspace_epoch" => value["workspace_epoch"] = json!(Uuid::new_v4().to_string()),
                "node_id" => value["node"]["node_id"] = json!(DESTINATION),
                "parent_id" => value["node"]["parent_id"] = json!(PARENT),
                "name" => value["node"]["name"] = json!("wrong.md"),
                "deleted" => value["node"]["deleted"] = json!(true),
                "meta_etag" => value["node"]["meta_etag"] = json!("\"wrong\""),
                "node_etag" => value["node"]["node_etag"] = json!("W/\"weak\""),
                "content_etag" => value["node"]["content_etag"] = json!("\"wrong\""),
                "hash" => value["node"]["content_hash"] = json!("sha256:broken"),
                "commit_seq" => value["commit_seq"] = json!("12"),
                _ => unreachable!(),
            }
            (
                "200 OK".into(),
                "application/json".into(),
                value.to_string(),
            )
        })
        .await;
        assert!(
            matches!(
                session.move_node(&mock_move(&session)).await,
                Err(Error::Binding | Error::Contract)
            ),
            "{case}"
        );
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with(&format!("patch /v1/nodes/{NODE}/metadata ")));
        assert!(request.contains("if-match: \"meta-before\""));
        assert!(request.contains("x-operation-seq: 1"));
        assert!(request.contains(&format!("idempotency-key: {OP}")));
    }
}

#[tokio::test]
async fn malformed_precondition_error_is_not_accepted() {
    for case in ["same_etag", "wrong_code", "wrong_status"] {
        let (session, server) = mock_session(|epoch| {
            let current = if case == "same_etag" { "meta-before" } else { "meta-after" };
            let operation = json!({"status":"metadata_precondition_failed", "operation_id":OP, "operation_seq":"1", "workspace_epoch":epoch, "current_meta_rev":current, "current_meta_etag":format!("\"{current}\"")});
            let code = if case == "wrong_code" { "destination_exists" } else { "metadata_revision_mismatch" };
            let status = if case == "wrong_status" { "409 Conflict" } else { "412 Precondition Failed" };
            (status.into(), "application/json".into(), json!({"code":code,"message":"synthetic","request_id":Uuid::now_v7().to_string(),"operation":operation}).to_string())
        }).await;
        assert_eq!(
            session.move_node(&mock_move(&session)).await,
            Err(Error::Contract),
            "{case}"
        );
        server.await.unwrap();
    }
}

async fn grant_move(config: &Config, principal_id: String) -> (String, String) {
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
async fn abandon_missing_and_committed_move_preserves_original_receipt_and_bytes() {
    let rig = rig().await;
    let parent = rig.source.parent_id.unwrap();
    let request = movement(&rig.editor, &rig.source, 1, parent, "moved.md");
    let receipt = rig.editor.move_node(&request).await.unwrap();
    assert_eq!(rig.editor.abandon_move(&request).await.unwrap(), receipt);
    assert_eq!(
        std::fs::read(rig.notes.join("moved.md")).unwrap(),
        b"# synthetic source\n"
    );
    let absent = movement(&rig.editor, &rig.existing, 2, parent, "not-created.md");
    assert!(matches!(
        rig.editor.abandon_move(&absent).await.unwrap(),
        OperationReceipt::Abandoned { .. }
    ));
    assert!(
        matches!(rig.editor.move_operation(&absent).await.unwrap(), OperationLookup::Receipt(receipt) if matches!(*receipt, OperationReceipt::Abandoned { .. }))
    );
    assert!(matches!(
        rig.editor.move_node(&absent).await,
        Err(Error::IdempotencyMismatch)
    ));
    assert_eq!(
        std::fs::read(rig.notes.join("existing.md")).unwrap(),
        b"# synthetic existing\n"
    );
    assert!(!rig.notes.join("not-created.md").exists());
    rig.server.shutdown().await.unwrap();
}
#[tokio::test]
async fn lost_move_response_is_recovered_by_lookup_using_same_identity() {
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
    let (workspace_id, secret) = grant_move(&config, owner_principal_id).await;
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
            let drop_response = line.starts_with("PATCH ");
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
    let request = MoveMutation {
        binding: session.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        node_id: document.node_id,
        expected_meta_etag: node.meta_etag.clone(),
        request: MoveNodeRequest {
            expected_parent_id: node.parent_id.unwrap(),
            parent_id: node.parent_id.unwrap(),
            name: "renamed.md".into(),
        },
    };
    assert_eq!(session.move_node(&request).await, Err(Error::Network));
    assert!(!notes.join("example.md").exists());
    assert_eq!(
        std::fs::read(notes.join("renamed.md")).unwrap(),
        b"committed before loss\n"
    );
    assert!(
        matches!(session.move_operation(&request).await.unwrap(), OperationLookup::Receipt(receipt) if matches!(*receipt, OperationReceipt::Moved { operation_id, operation_seq, .. } if operation_id == request.operation_id && operation_seq.get() == request.operation_seq))
    );
    let paths = proxy_task.await.unwrap();
    assert!(paths[1].starts_with("PATCH "));
    assert!(paths[2].starts_with(&format!("GET /v1/operations/{} ", request.operation_id)));
    server.shutdown().await.unwrap();
}
