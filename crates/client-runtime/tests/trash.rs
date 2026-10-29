use notryx_client_runtime::{Error, Session};
use notryx_domain::NodeId;
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, PairResponse, WorkspaceRole};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use uuid::Uuid;

fn node(id: Uuid, parent: Uuid, name: &str, kind: &str, restorable: bool) -> Value {
    json!({"node_id":id.to_string(), "parent_id":parent.to_string(), "name":name,
        "kind":kind, "meta_rev":"m.example", "restorable":restorable})
}

async fn mock_session<F>(reply: F) -> (Session, JoinHandle<String>)
where
    F: FnOnce(&str) -> (String, String, String),
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let workspace = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let pair = json!({"protocol":"mdw-sync/1.2", "workspace_id":workspace,
        "workspace_epoch":epoch, "principal_id":Uuid::new_v4().to_string(),
        "device_id":Uuid::new_v4().to_string(), "device_stream_id":Uuid::new_v4().to_string(),
        "device_token":"synthetic-device-token", "role":"reader",
        "idle_expires_at":"1", "absolute_expires_at":"2"})
    .to_string();
    let (status, content_type, body) = reply(&epoch);
    let server = tokio::spawn(async move {
        let mut captured = String::new();
        for (index, (status, content_type, body)) in [
            ("200 OK".to_string(), "application/json".to_string(), pair),
            (status, content_type, body),
        ]
        .into_iter()
        .enumerate()
        {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let length = stream.read(&mut bytes).await.unwrap();
                if length == 0 {
                    break;
                }
                request.extend_from_slice(&bytes[..length]);
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
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

#[tokio::test]
async fn trash_sends_authenticated_keyset_request_and_preserves_restorability() {
    let parent = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let (session, server) = mock_session(|epoch| ("200 OK".into(), "application/json".into(),
        json!({"workspace_epoch":epoch, "nodes":[node(first,parent,"first.md","file",false), node(second,parent,"Folder","folder",true)], "next_after_id":second.to_string()}).to_string())).await;
    let page = session.trash(None, 2).await.unwrap();
    assert_eq!(page.nodes.len(), 2);
    assert!(!page.nodes[0].restorable);
    assert!(page.nodes[1].restorable);
    assert_eq!(
        page.next_after_id,
        Some(second.to_string().parse().unwrap())
    );
    let request = server.await.unwrap();
    assert!(request.starts_with("GET /v1/trash?limit=2 "));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-device-token")
    );
}

#[tokio::test]
async fn trash_rejects_invalid_pages_and_untrusted_state() {
    type PageMutation = Box<dyn Fn(&mut Value)>;
    let parent = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let invalid: Vec<PageMutation> = vec![
        Box::new(|v| v["workspace_epoch"] = json!(Uuid::new_v4().to_string())),
        Box::new(|v| v["nodes"].as_array_mut().unwrap().reverse()),
        Box::new(|v| v["nodes"][1] = v["nodes"][0].clone()),
        Box::new(|v| v["next_after_id"] = json!(Uuid::now_v7().to_string())),
        Box::new(|v| {
            v["nodes"][0]
                .as_object_mut()
                .unwrap()
                .remove("restorable")
                .map(|_| ())
                .unwrap()
        }),
        Box::new(|v| v["nodes"][0]["parent_id"] = Value::Null),
        Box::new(|v| v["nodes"][0]["name"] = json!("../escape")),
        Box::new(|v| v["nodes"][0]["kind"] = json!("unknown")),
        Box::new(|v| v["nodes"][0]["extra"] = json!(true)),
        Box::new(|v| v["nodes"][0]["parent_id"] = v["nodes"][0]["node_id"].clone()),
        Box::new(|v| v["nodes"][0]["meta_rev"] = json!("m".repeat(257))),
    ];
    for (index, mutate) in invalid.into_iter().enumerate() {
        let (session, server) = mock_session(|epoch| {
            let mut page = json!({"workspace_epoch":epoch,"nodes":[node(first,parent,"a.md","file",true),node(second,parent,"b.md","file",true)],"next_after_id":second.to_string()});
            mutate(&mut page);
            ("200 OK".into(),"application/json".into(),page.to_string())
        }).await;
        assert_eq!(
            session.trash(None, 2).await,
            Err(if index == 0 {
                Error::Binding
            } else {
                Error::Contract
            }),
            "case {index}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn trash_enforces_limits_and_cursor_boundaries() {
    let parent = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let (session, server) = mock_session(|epoch| ("200 OK".into(),"application/json".into(),
        json!({"workspace_epoch":epoch,"nodes":[node(first,parent,"a.md","file",true),node(second,parent,"b.md","file",true)],"next_after_id":null}).to_string())).await;
    assert_eq!(session.trash(None, 1).await, Err(Error::Contract));
    server.await.unwrap();

    let (session, server) = mock_session(|epoch| ("200 OK".into(),"application/json".into(),
        json!({"workspace_epoch":epoch,"nodes":[node(first,parent,"a.md","file",true)],"next_after_id":null}).to_string())).await;
    assert_eq!(
        session
            .trash(Some(first.to_string().parse().unwrap()), 100)
            .await,
        Err(Error::Contract)
    );
    let request = server.await.unwrap();
    assert!(request.starts_with(&format!("GET /v1/trash?after_id={first}&limit=100 ")));

    let (session, server) = mock_session(|epoch| {
        (
            "200 OK".into(),
            "application/json".into(),
            json!({"workspace_epoch":epoch,"nodes":[],"next_after_id":null}).to_string(),
        )
    })
    .await;
    assert_eq!(session.trash(None, 0).await, Err(Error::InvalidRequest));
    assert_eq!(session.trash(None, 101).await, Err(Error::InvalidRequest));
    server.abort();
}

#[tokio::test]
async fn trash_rejects_oversized_and_gateway_responses() {
    let (session, server) = mock_session(|epoch| ("200 OK".into(), "application/json".into(),
        json!({"workspace_epoch":epoch,"nodes":[],"next_after_id":null,"extra":"x".repeat(1024*1024)}).to_string())).await;
    assert_eq!(session.trash(None, 100).await, Err(Error::TooLarge));
    server.await.unwrap();
    for (status, content_type, body, expected) in [
        ("302 Found", "text/plain", "", Error::GatewayAuthRequired),
        (
            "200 OK",
            "text/html",
            "<html></html>",
            Error::GatewayAuthRequired,
        ),
        (
            "403 Forbidden",
            "text/plain",
            "denied",
            Error::GatewayAuthRequired,
        ),
        (
            "401 Unauthorized",
            "application/json",
            "{\"code\":\"unauthorized\",\"message\":\"denied\",\"request_id\":\"018f0000-0000-7000-8000-000000000000\"}",
            Error::Unauthorized,
        ),
    ] {
        let (session, server) =
            mock_session(|_| (status.into(), content_type.into(), body.into())).await;
        assert_eq!(session.trash(None, 100).await, Err(expected));
        server.await.unwrap();
    }
}

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
async fn reader_lists_real_deleted_files_in_uuid_order_and_after_revocation_is_denied() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("one.md"), b"one\n").unwrap();
    let first = workspace.adopt_root_document("one.md").await.unwrap();
    std::fs::write(notes.join("two.md"), b"two\n").unwrap();
    let second = workspace.adopt_root_document("two.md").await.unwrap();
    workspace.close().await.unwrap();
    let mut config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes,
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
    let origin = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let owner: PairResponse = client
        .post(format!("{origin}/v1/pair"))
        .json(
            &json!({"protocol":"mdw-sync/1.2","workspace_id":workspace_id,"pairing_secret":secret}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for (index, document) in [&first, &second].into_iter().enumerate() {
        let response = client
            .get(format!("{origin}/v1/nodes/{}", document.node_id))
            .bearer_auth(&owner.device_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let node: Value = response.json().await.unwrap();
        let response = client
            .delete(format!("{origin}/v1/nodes/{}", document.node_id))
            .bearer_auth(&owner.device_token)
            .header("X-Workspace-Epoch", owner.workspace_epoch.to_string())
            .header("X-Device-Stream", &owner.device_stream_id)
            .header("X-Operation-Seq", (index + 1).to_string())
            .header("Idempotency-Key", Uuid::now_v7().to_string())
            .header("If-Match", node["node_etag"].as_str().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
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
    let reader = Session::pair(&origin, workspace_id.parse().unwrap(), &secret)
        .await
        .unwrap();
    let one = reader.trash(None, 1).await.unwrap();
    assert_eq!(one.nodes.len(), 1);
    assert!(one.nodes[0].restorable);
    let after = one.next_after_id.expect("more deleted entries");
    assert_eq!(after, one.nodes[0].node_id);
    let two = reader.trash(Some(after), 1).await.unwrap();
    assert_eq!(two.nodes.len(), 1);
    assert!(two.nodes[0].node_id.to_string() > after.to_string());
    assert_eq!(two.next_after_id, None);
    let ids: Vec<NodeId> = vec![one.nodes[0].node_id, two.nodes[0].node_id];
    assert!(ids.contains(&first.node_id) && ids.contains(&second.node_id));
    config.server.listen = server.local_addr();
    server.shutdown().await.unwrap();
    let expired_hash: String = Sha256::digest(b"one\n")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    std::fs::remove_file(
        config
            .workspace
            .state_root
            .join("objects/sha256")
            .join(expired_hash),
    )
    .unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let expired = reader.trash(None, 100).await.unwrap();
    assert!(
        !expired
            .nodes
            .iter()
            .find(|entry| entry.node_id == first.node_id)
            .unwrap()
            .restorable
    );
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: reader.binding().device_id.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(reader.trash(None, 100).await, Err(Error::Unauthorized));
    server.shutdown().await.unwrap();
}
