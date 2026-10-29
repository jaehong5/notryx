use notryx_client_runtime::{Error, Session};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::{Duration, sleep},
};
use uuid::Uuid;

async fn mock_session<F>(make_reply: F) -> (Session, JoinHandle<String>)
where
    F: FnOnce(&str) -> (String, String, Vec<Vec<u8>>),
{
    mock_session_with_headers(|epoch| {
        let (status, media, chunks) = make_reply(epoch);
        (status, media, String::new(), chunks)
    })
    .await
}

async fn mock_session_with_headers<F>(make_reply: F) -> (Session, JoinHandle<String>)
where
    F: FnOnce(&str) -> (String, String, String, Vec<Vec<u8>>),
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let workspace_id = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let paired = json!({
        "protocol":"mdw-sync/1.2", "workspace_id":workspace_id, "workspace_epoch":epoch,
        "principal_id":Uuid::new_v4().to_string(), "device_id":Uuid::new_v4().to_string(),
        "device_stream_id":Uuid::new_v4().to_string(), "device_token":"synthetic-device-token",
        "role":"owner", "idle_expires_at":"1", "absolute_expires_at":"2"
    })
    .to_string();
    let (status, content_type, extra_headers, chunks) = make_reply(&epoch);
    let task = tokio::spawn(async move {
        let mut captured = String::new();
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
                if let Some(pos) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..pos]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, val) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| val.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= pos + 4 + length {
                        break;
                    }
                }
            }
            if index == 0 {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{paired}",
                    paired.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            } else {
                captured = String::from_utf8(request).unwrap();
                let length: usize = chunks.iter().map(Vec::len).sum();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
                for chunk in &chunks {
                    if socket.write_all(chunk).await.is_err() {
                        break;
                    }
                    sleep(Duration::from_millis(2)).await;
                }
            }
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
    (session, task)
}

#[tokio::test]
async fn receives_split_crlf_multiline_head_and_uses_bearer_header() {
    let (session, server) = mock_session(|epoch| {
        let chunks = vec![
            b": heartbeat\r\n\r\neve".to_vec(),
            b"nt: head\r\ndata: {\"workspace_epoch\":".to_vec(),
            format!("\"{epoch}\",\r\n").into_bytes(),
            b"data: \"head_seq\":\"42\"}\r\n\r\n".to_vec(),
        ];
        (
            "200 OK".into(),
            "text/event-stream; charset=utf-8".into(),
            chunks,
        )
    })
    .await;
    let mut events = session.events().await.unwrap();
    assert_eq!(
        events.next_head().await.unwrap().unwrap().head_seq.get(),
        42
    );
    assert!(events.next_head().await.unwrap().is_none());
    let request = server.await.unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /v1/events "));
    assert!(request.contains("authorization: bearer synthetic-device-token"));
    assert!(request.contains("accept-encoding: identity"));
}

#[tokio::test]
async fn preserves_multiple_events_without_turning_id_into_cursor() {
    let (session, server) = mock_session(|epoch| {
        let content = format!("id: 999\nevent: head\ndata: {{\"workspace_epoch\":\"{epoch}\",\"head_seq\":\"7\"}}\n\nid: 0\nevent: head\ndata: {{\"workspace_epoch\":\"{epoch}\",\"head_seq\":\"3\"}}\n\n");
        ("200 OK".into(), "text/event-stream".into(), vec![content.into_bytes()])
    }).await;
    let mut events = session.events().await.unwrap();
    assert_eq!(events.next_head().await.unwrap().unwrap().head_seq.get(), 7);
    assert_eq!(events.next_head().await.unwrap().unwrap().head_seq.get(), 3);
    assert!(events.next_head().await.unwrap().is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn rejects_wrong_epoch_and_invalid_data() {
    for wrong in [true, false] {
        let (session, server) = mock_session(|epoch| {
            let data = if wrong {
                format!(
                    "{{\"workspace_epoch\":\"{}\",\"head_seq\":\"1\"}}",
                    Uuid::new_v4()
                )
            } else {
                format!("{{\"workspace_epoch\":\"{epoch}\",\"head_seq\":1}}")
            };
            (
                "200 OK".into(),
                "text/event-stream".into(),
                vec![format!("event: head\ndata: {data}\n\n").into_bytes()],
            )
        })
        .await;
        let mut events = session.events().await.unwrap();
        assert_eq!(
            events.next_head().await,
            Err(if wrong {
                Error::Binding
            } else {
                Error::Contract
            })
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn rejects_redirect_html_unauthorized_and_wrong_media() {
    for (status, media, body, expected) in [
        ("302 Found", "text/plain", "", Error::GatewayAuthRequired),
        ("200 OK", "text/html", "login", Error::GatewayAuthRequired),
        (
            "401 Unauthorized",
            "application/json",
            "{\"code\":\"unauthorized\",\"message\":\"denied\",\"request_id\":\"01900000-0000-7000-8000-000000000001\"}",
            Error::Unauthorized,
        ),
        ("200 OK", "application/json", "{}", Error::Contract),
    ] {
        let (session, server) =
            mock_session(|_| (status.into(), media.into(), vec![body.as_bytes().to_vec()])).await;
        assert!(matches!(session.events().await, Err(error) if error == expected));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn rejects_unbounded_event_and_malformed_utf8() {
    for data in [
        vec![b'x'; 65537],
        vec![b'e', b'v', b'e', b'n', b't', b':', b' ', 0xff, b'\n', b'\n'],
    ] {
        let (session, server) =
            mock_session(|_| ("200 OK".into(), "text/event-stream".into(), vec![data])).await;
        let mut events = session.events().await.unwrap();
        assert!(matches!(
            events.next_head().await,
            Err(Error::TooLarge | Error::Contract)
        ));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn batches_many_valid_hints_without_overflowing_or_losing_latest_head() {
    let (session, server) = mock_session(|epoch| {
        let mut stream = String::new();
        for seq in 0..200 {
            stream.push_str(&format!(
                "event: head\ndata: {{\"workspace_epoch\":\"{epoch}\",\"head_seq\":\"{seq}\"}}\n\n"
            ));
        }
        (
            "200 OK".into(),
            "text/event-stream".into(),
            vec![stream.into_bytes()],
        )
    })
    .await;
    let mut events = session.events().await.unwrap();
    let mut highest = 0;
    let mut count = 0;
    while let Some(hint) = events.next_head().await.unwrap() {
        highest = highest.max(hint.head_seq.get());
        count += 1;
    }
    assert_eq!(highest, 199);
    assert!(count > 0 && count <= 200);
    server.await.unwrap();
}

#[tokio::test]
async fn rejects_unsupported_and_duplicate_content_encodings() {
    for headers in [
        "Content-Encoding: gzip\r\n",
        "Content-Encoding: identity\r\nContent-Encoding: identity\r\n",
    ] {
        let (session, server) = mock_session_with_headers(|_| {
            (
                "200 OK".into(),
                "text/event-stream".into(),
                headers.into(),
                vec![],
            )
        })
        .await;
        assert!(matches!(session.events().await, Err(Error::Contract)));
        server.await.unwrap();
    }
    let (session, server) = mock_session_with_headers(|_| {
        (
            "200 OK".into(),
            "text/event-stream".into(),
            "Content-Encoding: identity\r\n".into(),
            vec![],
        )
    })
    .await;
    assert!(session.events().await.is_ok());
    server.await.unwrap();
}

#[tokio::test]
async fn redirect_never_transmits_bearer_to_the_other_origin() {
    let other = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = other.local_addr().unwrap();
    let (session, server) = mock_session_with_headers(|_| {
        (
            "302 Found".into(),
            "text/plain".into(),
            format!("Location: http://{address}/redirected\r\n"),
            vec![],
        )
    })
    .await;
    assert!(matches!(
        session.events().await,
        Err(Error::GatewayAuthRequired)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(150), other.accept())
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn handles_a_byte_at_a_time_with_utf8_bom_and_crlf() {
    let (session, server) = mock_session(|epoch| {
        let data = format!("\u{feff}: ping\r\n\r\nevent: head\r\ndata: {{\"workspace_epoch\":\"{epoch}\",\"head_seq\":\"8\"}}\r\n\r\n");
        ("200 OK".into(), "text/event-stream".into(), data.into_bytes().into_iter().map(|byte| vec![byte]).collect())
    }).await;
    let mut events = session.events().await.unwrap();
    assert_eq!(events.next_head().await.unwrap().unwrap().head_seq.get(), 8);
    assert!(events.next_head().await.unwrap().is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn real_server_hint_triggers_journal_read_and_revocation_ends_stream() {
    use notryx_client_runtime::ContentMutation;
    use notryx_domain::ChangeSeq;
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
    std::fs::write(notes.join("fixture.md"), b"before\n").unwrap();
    let node = workspace.adopt_root_document("fixture.md").await.unwrap();
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
        panic!("status")
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
        panic!("pairing")
    };
    let session = Session::pair(
        &format!("http://{}", server.local_addr()),
        workspace_id,
        &pairing_secret,
    )
    .await
    .unwrap();
    let mut events = session.events().await.unwrap();
    let initial = tokio::time::timeout(Duration::from_secs(5), events.next_head())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(initial.head_seq.get(), 1);
    let inventory = session.inventory(None).await.unwrap();
    let current = inventory
        .nodes
        .into_iter()
        .find(|item| item.node_id == node.node_id)
        .unwrap();
    let mutation = ContentMutation {
        binding: session.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        node_id: node.node_id,
        expected_revision: current.content_rev.unwrap(),
        bytes: b"after\n".to_vec(),
    };
    session.put_content(&mutation).await.unwrap();
    let changed = tokio::time::timeout(Duration::from_secs(5), events.next_head())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(changed.head_seq.get(), 2);
    let changes = session.changes(ChangeSeq::new(1), None).await.unwrap();
    assert_eq!(changes.covered_through.get(), 2);
    assert!(
        changes
            .nodes
            .iter()
            .any(|item| item.node_id == node.node_id)
    );
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: session.binding().device_id.to_string(),
        },
    )
    .await
    .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), events.next_head())
        .await
        .unwrap();
    assert!(matches!(closed, Ok(None) | Err(Error::Network)));
    drop(events);
    server.shutdown().await.unwrap();
}
