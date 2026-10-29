use notryx_client_runtime::{Error, Session};
use notryx_domain::ChangeSeq;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use uuid::Uuid;

fn file(id: Uuid, seq: u64, name: &str) -> Value {
    json!({
        "node_id": id.to_string(), "parent_id": Uuid::now_v7().to_string(),
        "name": name, "kind": "file", "deleted": false,
        "content_rev": "revision", "content_etag": "\"revision\"",
        "meta_rev": "meta", "meta_etag": "\"meta\"", "node_etag": "\"node\"",
        "content_hash": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "size": "0", "children_generation": "0", "last_change_seq": seq.to_string()
    })
}

fn page(epoch: &str) -> Value {
    let id = Uuid::now_v7();
    json!({
        "workspace_epoch": epoch, "from_exclusive": "9", "through_seq": "12",
        "covered_through": "11", "has_more": true,
        "nodes": [file(id, 10, "before.md"), file(id, 11, "after.md")]
    })
}

async fn mock_session<F>(make_reply: F) -> (Session, JoinHandle<String>)
where
    F: FnOnce(&str) -> (String, String),
{
    mock_session_with_media(|epoch| {
        let (status, body) = make_reply(epoch);
        (status, "application/json".to_owned(), body)
    })
    .await
}

async fn mock_session_with_media<F>(make_reply: F) -> (Session, JoinHandle<String>)
where
    F: FnOnce(&str) -> (String, String, String),
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let workspace_id = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let pair = json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace_id,
        "workspace_epoch": epoch, "principal_id": Uuid::new_v4().to_string(),
        "device_id": Uuid::new_v4().to_string(), "device_stream_id": Uuid::new_v4().to_string(),
        "device_token": "synthetic-device-token", "role": "owner",
        "idle_expires_at": "1", "absolute_expires_at": "2"
    })
    .to_string();
    let (status, media_type, body) = make_reply(&epoch);
    let server = tokio::spawn(async move {
        let mut captured = String::new();
        for (index, (status, media_type, body)) in [
            ("200 OK".to_string(), "application/json".to_string(), pair),
            (status, media_type, body),
        ]
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
                if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..position]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= position + 4 + length {
                        break;
                    }
                }
            }
            if index == 1 {
                captured = String::from_utf8(request).unwrap();
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
        captured
    });
    let session = Session::pair(
        &format!("http://{address}"),
        workspace_id.parse().unwrap(),
        "synthetic-secret",
    )
    .await
    .unwrap();
    (session, server)
}

#[tokio::test]
async fn changes_uses_fixed_through_and_retains_multiple_states_of_same_node() {
    let (session, server) = mock_session(|epoch| ("200 OK".into(), page(epoch).to_string())).await;
    let result = session
        .changes(ChangeSeq::new(9), Some(ChangeSeq::new(12)))
        .await
        .unwrap();
    assert_eq!(result.covered_through.get(), 11);
    assert!(result.has_more);
    assert_eq!(result.nodes.len(), 2);
    assert_eq!(result.nodes[0].node_id, result.nodes[1].node_id);
    assert_eq!(result.nodes[0].name, "before.md");
    assert_eq!(result.nodes[1].name, "after.md");
    let request = server.await.unwrap();
    assert!(request.starts_with("GET /v1/changes?"));
    assert!(request.contains("after=9"));
    assert!(request.contains("through=12"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-device-token")
    );
}

#[tokio::test]
async fn changes_allows_an_empty_complete_interval() {
    let (session, server) = mock_session(|epoch| {
        (
            "200 OK".into(),
            json!({
                "workspace_epoch": epoch, "from_exclusive": "9", "through_seq": "9",
                "covered_through": "9", "has_more": false, "nodes": []
            })
            .to_string(),
        )
    })
    .await;
    let result = session
        .changes(ChangeSeq::new(9), Some(ChangeSeq::new(9)))
        .await
        .unwrap();
    assert!(result.nodes.is_empty());
    assert_eq!(result.covered_through.get(), 9);
    server.await.unwrap();
}

#[tokio::test]
async fn changes_rejects_invalid_server_pages() {
    type PageMutation = Box<dyn Fn(&mut Value)>;
    let bad_pages: Vec<PageMutation> = vec![
        Box::new(|v| v["workspace_epoch"] = json!(Uuid::new_v4().to_string())),
        Box::new(|v| v["from_exclusive"] = json!("8")),
        Box::new(|v| v["through_seq"] = json!("13")),
        Box::new(|v| v["covered_through"] = json!("13")),
        Box::new(|v| v["covered_through"] = json!("9")),
        Box::new(|v| v["has_more"] = json!(false)),
        Box::new(|v| v["nodes"].as_array_mut().unwrap().reverse()),
        Box::new(|v| v["nodes"][0]["last_change_seq"] = json!("9")),
        Box::new(|v| v["nodes"][1]["last_change_seq"] = json!("12")),
        Box::new(|v| {
            let mut conflict = v["nodes"][0].clone();
            conflict["name"] = json!("conflicting.md");
            v["nodes"].as_array_mut().unwrap().insert(1, conflict);
        }),
        Box::new(|v| v["nodes"][0]["meta_etag"] = json!("\"other\"")),
        Box::new(|v| v["nodes"][0]["content_etag"] = json!("\"other\"")),
        Box::new(|v| v["nodes"][0]["node_etag"] = json!("W/\"node\"")),
        Box::new(|v| v["nodes"][0]["content_hash"] = json!("sha256:not-a-digest")),
        Box::new(|v| {
            v["nodes"][0].as_object_mut().unwrap().remove("parent_id");
        }),
        Box::new(|v| v["nodes"][0]["content_rev"] = Value::Null),
    ];
    for (index, mutate) in bad_pages.into_iter().enumerate() {
        let (session, server) = mock_session(|epoch| {
            let mut p = page(epoch);
            mutate(&mut p);
            ("200 OK".into(), p.to_string())
        })
        .await;
        let expected = if index == 0 {
            Error::Binding
        } else {
            Error::Contract
        };
        assert_eq!(
            session
                .changes(ChangeSeq::new(9), Some(ChangeSeq::new(12)))
                .await,
            Err(expected),
            "case {index}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn changes_rejects_auth_failures_html_and_redirects() {
    for (status, media_type, body, expected) in [
        (
            "401 Unauthorized",
            "application/json",
            json!({
                "code": "device_revoked", "message": "synthetic",
                "request_id": Uuid::now_v7().to_string()
            })
            .to_string(),
            Error::Unauthorized,
        ),
        (
            "403 Forbidden",
            "application/json",
            json!({
                "code": "forbidden", "message": "synthetic",
                "request_id": Uuid::now_v7().to_string()
            })
            .to_string(),
            Error::Forbidden,
        ),
        (
            "403 Forbidden",
            "text/html",
            "<html>login</html>".to_owned(),
            Error::GatewayAuthRequired,
        ),
        (
            "200 OK",
            "text/html",
            "<html>login</html>".to_owned(),
            Error::GatewayAuthRequired,
        ),
        (
            "302 Found",
            "application/json",
            "{}".to_owned(),
            Error::GatewayAuthRequired,
        ),
    ] {
        let (session, server) =
            mock_session_with_media(|_| (status.to_owned(), media_type.to_owned(), body)).await;
        assert_eq!(
            session
                .changes(ChangeSeq::new(9), Some(ChangeSeq::new(12)))
                .await,
            Err(expected),
            "status {status} media type {media_type}",
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn changes_rejects_response_over_one_mib() {
    let (session, server) = mock_session(|epoch| {
        let mut p = page(epoch);
        p["padding"] = json!("x".repeat(1024 * 1024));
        ("200 OK".into(), p.to_string())
    })
    .await;
    assert_eq!(
        session
            .changes(ChangeSeq::new(9), Some(ChangeSeq::new(12)))
            .await,
        Err(Error::TooLarge)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn changes_reads_committed_history_from_real_server() {
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("First.md"), b"first\n").unwrap();
    let first = workspace.adopt_root_document("First.md").await.unwrap();
    std::fs::write(notes.join("Second.md"), b"second\n").unwrap();
    let second = workspace.adopt_root_document("Second.md").await.unwrap();
    workspace.close().await.unwrap();

    let config = Config {
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
        result:
            AdminResult::Status {
                workspace_id,
                owner_principal_id,
                ..
            },
    } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
        .await
        .unwrap()
    else {
        panic!("admin status")
    };
    let AdminResponse::Ok {
        result: AdminResult::Pairing { pairing_secret, .. },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing {
            principal_id: owner_principal_id,
        },
    )
    .await
    .unwrap()
    else {
        panic!("pairing grant")
    };
    let session = Session::pair(
        &format!("http://{}", server.local_addr()),
        workspace_id,
        &pairing_secret,
    )
    .await
    .unwrap();
    let first_page = session
        .changes(ChangeSeq::new(0), Some(ChangeSeq::new(1)))
        .await
        .unwrap();
    assert_eq!(first_page.through_seq.get(), 1);
    assert_eq!(first_page.covered_through.get(), 1);
    assert!(!first_page.has_more);
    assert!(
        first_page
            .nodes
            .iter()
            .any(|node| node.node_id == first.node_id)
    );
    assert!(
        !first_page
            .nodes
            .iter()
            .any(|node| node.node_id == second.node_id)
    );
    let second_page = session.changes(ChangeSeq::new(1), None).await.unwrap();
    assert_eq!(second_page.through_seq.get(), 2);
    assert_eq!(second_page.covered_through.get(), 2);
    assert!(
        second_page
            .nodes
            .iter()
            .any(|node| node.node_id == second.node_id)
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn changes_rejects_gaps_and_more_than_one_hundred_change_sets() {
    let (session, server) = mock_session(|epoch| {
        let mut p = page(epoch);
        p["through_seq"] = json!("13");
        p["covered_through"] = json!("12");
        p["nodes"][1]["last_change_seq"] = json!("12");
        ("200 OK".into(), p.to_string())
    })
    .await;
    assert_eq!(
        session
            .changes(ChangeSeq::new(9), Some(ChangeSeq::new(13)))
            .await,
        Err(Error::Contract)
    );
    server.await.unwrap();

    let (session, server) = mock_session(|epoch| {
        let nodes: Vec<_> = (1..=101)
            .map(|seq| file(Uuid::now_v7(), seq, "file.md"))
            .collect();
        (
            "200 OK".into(),
            json!({
                "workspace_epoch": epoch, "from_exclusive": "0", "through_seq": "101",
                "covered_through": "101", "has_more": false, "nodes": nodes
            })
            .to_string(),
        )
    })
    .await;
    assert_eq!(
        session
            .changes(ChangeSeq::new(0), Some(ChangeSeq::new(101)))
            .await,
        Err(Error::Contract)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn changes_distinguishes_resync_from_unrelated_gone_responses() {
    for (code, expected) in [
        ("resync_required", Error::ResyncRequired),
        ("operation_retired", Error::Response),
    ] {
        let (session, server) =
            mock_session(|_| {
                ("410 Gone".into(), json!({
                "code": code, "message": "synthetic", "request_id": Uuid::now_v7().to_string()
            }).to_string())
            })
            .await;
        assert_eq!(
            session
                .changes(ChangeSeq::new(9), Some(ChangeSeq::new(12)))
                .await,
            Err(expected)
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn changes_without_through_accepts_server_selected_head() {
    let (session, server) = mock_session(|epoch| ("200 OK".into(), page(epoch).to_string())).await;
    let page = session.changes(ChangeSeq::new(9), None).await.unwrap();
    assert_eq!(page.through_seq.get(), 12);
    let request = server.await.unwrap();
    assert!(request.contains("after=9"));
    assert!(!request.contains("through="));
}
