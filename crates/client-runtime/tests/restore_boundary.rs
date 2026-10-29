use notryx_client_runtime::{Error, RestoreMutation, Session};
use notryx_protocol::{OperationReceipt, RestoreNodeRequest};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use uuid::Uuid;

const NODE: &str = "019a1000-0000-7000-8000-000000000002";
const PARENT: &str = "019a1000-0000-7000-8000-000000000001";
const OP: &str = "019a1000-0000-7000-8000-000000000003";
fn good(epoch: &str) -> Value {
    json!({"status":"restored", "operation_id":OP, "operation_seq":"1", "workspace_epoch":epoch,
        "node":{"node_id":NODE,"parent_id":PARENT,"name":"restored.md","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"meta-2","meta_etag":"\"meta-2\"","node_etag":"\"node-2\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"11"}, "commit_seq":"11"})
}
async fn mock_session<F>(reply: F) -> (Session, JoinHandle<String>)
where
    F: FnOnce(&str) -> (String, String, String),
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let workspace = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let pair = json!({"protocol":"mdw-sync/1.2", "workspace_id":workspace,"workspace_epoch":epoch,"principal_id":Uuid::new_v4().to_string(),"device_id":Uuid::new_v4().to_string(),"device_stream_id":Uuid::new_v4().to_string(),"device_token":"synthetic-token","role":"editor","idle_expires_at":"1","absolute_expires_at":"2"}).to_string();
    let answer = reply(&epoch);
    let server = tokio::spawn(async move {
        let mut captured = String::new();
        for (index, (status, media, body)) in [
            ("200 OK".to_string(), "application/json".to_string(), pair),
            answer,
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
fn mutation(session: &Session) -> RestoreMutation {
    RestoreMutation {
        binding: session.binding().clone(),
        operation_id: OP.parse().unwrap(),
        operation_seq: 1,
        node_id: NODE.parse().unwrap(),
        request: serde_json::from_value::<RestoreNodeRequest>(
            json!({"expected_meta_rev":"meta-1","parent_id":PARENT,"name":"restored.md"}),
        )
        .unwrap(),
    }
}

#[tokio::test]
async fn untrusted_restore_receipts_are_rejected() {
    for case in [
        "operation_id",
        "operation_seq",
        "workspace_epoch",
        "node_id",
        "parent_id",
        "name",
        "meta_etag",
        "node_etag",
        "commit_seq",
    ] {
        let (session, server) = mock_session(|epoch| {
            let mut v = good(epoch);
            match case {
                "operation_id" => v["operation_id"] = json!("019a1000-0000-7000-8000-000000000004"),
                "operation_seq" => v["operation_seq"] = json!("2"),
                "workspace_epoch" => v["workspace_epoch"] = json!(Uuid::new_v4().to_string()),
                "node_id" => v["node"]["node_id"] = json!("019a1000-0000-7000-8000-000000000004"),
                "parent_id" => {
                    v["node"]["parent_id"] = json!("019a1000-0000-7000-8000-000000000004")
                }
                "name" => v["node"]["name"] = json!("other.md"),
                "meta_etag" => v["node"]["meta_etag"] = json!("\"wrong\""),
                "node_etag" => v["node"]["node_etag"] = json!("W/\"weak\""),
                "commit_seq" => v["commit_seq"] = json!("12"),
                _ => unreachable!(),
            };
            ("200 OK".into(), "application/json".into(), v.to_string())
        })
        .await;
        assert!(
            matches!(
                session.restore(&mutation(&session)).await,
                Err(Error::Binding | Error::Contract)
            ),
            "{case}"
        );
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with(&format!("post /v1/nodes/{node}/restore ", node = NODE)));
        assert!(request.contains("authorization: bearer synthetic-token"));
    }
}

#[tokio::test]
async fn restore_transport_and_terminal_status_are_validated() {
    for (status, media, body, expected) in [
        (
            "200 OK",
            "text/html",
            "<html>gateway</html>",
            Error::GatewayAuthRequired,
        ),
        (
            "302 Found",
            "application/json",
            "{}",
            Error::GatewayAuthRequired,
        ),
        (
            "403 Forbidden",
            "application/json",
            "{\"code\":\"forbidden\",\"message\":\"denied\",\"request_id\":\"018f0000-0000-7000-8000-000000000000\"}",
            Error::Forbidden,
        ),
        ("200 OK", "application/json", "{}", Error::Contract),
    ] {
        let (session, server) = mock_session(|_| (status.into(), media.into(), body.into())).await;
        assert_eq!(session.restore(&mutation(&session)).await, Err(expected));
        server.await.unwrap();
    }
    let (session, server) = mock_session(|epoch| { let operation = json!({"status":"history_expired","operation_id":OP,"operation_seq":"1","workspace_epoch":epoch}); ("410 Gone".into(),"application/json".into(),json!({"code":"history_expired","message":"expired","request_id":"018f0000-0000-7000-8000-000000000000","operation":operation}).to_string()) }).await;
    assert!(matches!(
        session.restore(&mutation(&session)).await.unwrap(),
        OperationReceipt::HistoryExpired { .. }
    ));
    server.await.unwrap();
    let (session, server) = mock_session(|epoch| { let operation = json!({"status":"history_expired","operation_id":OP,"operation_seq":"1","workspace_epoch":epoch}); ("409 Conflict".into(),"application/json".into(),json!({"code":"history_expired","message":"expired","request_id":"018f0000-0000-7000-8000-000000000000","operation":operation}).to_string()) }).await;
    assert_eq!(
        session.restore(&mutation(&session)).await,
        Err(Error::Contract)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn permission_denial_is_distinct_from_expired_auth_and_gateway() {
    for (status, media, body, expected) in [
        (
            "401 Unauthorized",
            "application/json",
            "{\"code\":\"unauthorized\",\"message\":\"denied\",\"request_id\":\"018f0000-0000-7000-8000-000000000000\"}",
            "Unauthorized",
        ),
        (
            "403 Forbidden",
            "application/json",
            "{\"code\":\"forbidden\",\"message\":\"denied\",\"request_id\":\"018f0000-0000-7000-8000-000000000000\"}",
            "Forbidden",
        ),
        (
            "403 Forbidden",
            "text/html",
            "<html>gateway</html>",
            "GatewayAuthRequired",
        ),
    ] {
        let (session, server) = mock_session(|_| (status.into(), media.into(), body.into())).await;
        let error = session.restore(&mutation(&session)).await.unwrap_err();
        assert_eq!(format!("{error:?}"), expected);
        server.await.unwrap();
    }
}
