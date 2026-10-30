use notryx_client_runtime::{Error, Session};
use notryx_protocol::NodeState;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::{Duration, timeout},
};
use uuid::Uuid;

async fn session_with_reply(reply: String) -> (Session, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let workspace_id = Uuid::new_v4().to_string();
    let pair = serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace_id,
        "workspace_epoch": Uuid::new_v4().to_string(), "principal_id": Uuid::new_v4().to_string(),
        "device_id": Uuid::new_v4().to_string(), "device_stream_id": Uuid::new_v4().to_string(),
        "device_token": "synthetic-device-token", "role": "owner", "idle_expires_at": "1", "absolute_expires_at": "2"
    }).to_string();
    let task = tokio::spawn(async move {
        let mut recorded = String::new();
        for (index, response) in [
            response("200 OK", "Content-Type: application/json\r\n", &pair),
            reply,
        ]
        .into_iter()
        .enumerate()
        {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut expected = None;
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if let Some(position) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                    let length = *expected.get_or_insert_with(|| {
                        let headers = String::from_utf8_lossy(&request[..position]);
                        headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0)
                    });
                    if request.len() >= position + 4 + length {
                        break;
                    }
                }
            }
            if index == 1 {
                recorded = String::from_utf8(request).unwrap();
            }
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        recorded
    });
    let session = Session::pair(
        &format!("http://{address}"),
        workspace_id.parse().unwrap(),
        "synthetic-pair-secret",
    )
    .await
    .unwrap();
    (session, task)
}

fn response(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn node() -> NodeState {
    serde_json::from_value(serde_json::json!({
        "node_id": Uuid::now_v7().to_string(), "parent_id": Uuid::now_v7().to_string(),
        "name": "Example.md", "kind": "file", "deleted": false, "content_rev": "revision",
        "content_etag": "\"revision\"", "meta_rev": "meta", "meta_etag": "\"meta\"",
        "node_etag": "\"node\"", "content_hash": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "size": "0", "children_generation": "0", "last_change_seq": "0"
    })).unwrap()
}

#[tokio::test]
async fn transient_read_failures_are_unavailable() {
    for status in ["429 Too Many Requests", "503 Service Unavailable"] {
        let reply = response(status, "Content-Type: application/json\r\n", "{}");
        let (session, server) = session_with_reply(reply.clone()).await;
        assert_eq!(session.inventory(None).await, Err(Error::Unavailable));
        server.await.unwrap();

        let (session, server) = session_with_reply(reply).await;
        assert_eq!(session.content(&node()).await, Err(Error::Unavailable));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn read_failures_keep_authentication_and_contract_errors_distinct() {
    let forbidden = serde_json::json!({
        "code": "forbidden", "message": "permission denied", "request_id": Uuid::now_v7().to_string()
    })
    .to_string();
    for (reply, expected) in [
        (
            response(
                "403 Forbidden",
                "Content-Type: application/json\r\n",
                &forbidden,
            ),
            Error::Forbidden,
        ),
        (
            response(
                "503 Service Unavailable",
                "Content-Type: text/html\r\n",
                "sign in",
            ),
            Error::GatewayAuthRequired,
        ),
        (
            response("404 Not Found", "Content-Type: application/json\r\n", "{}"),
            Error::Response,
        ),
        (
            response("200 OK", "Content-Type: application/json\r\n", "{}"),
            Error::Contract,
        ),
    ] {
        let (session, server) = session_with_reply(reply.clone()).await;
        assert_eq!(session.inventory(None).await, Err(expected));
        server.await.unwrap();

        let (session, server) = session_with_reply(reply).await;
        assert_eq!(session.content(&node()).await, Err(expected));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn gateway_auth_is_distinct_from_a_valid_app_auth_error() {
    let unauthorized = serde_json::json!({"code":"unauthorized", "message":"authentication required", "request_id":Uuid::now_v7().to_string()}).to_string();
    let forbidden = serde_json::json!({"code":"forbidden", "message":"permission denied", "request_id":Uuid::now_v7().to_string()}).to_string();
    for (reply, expected) in [
        (
            response(
                "401 Unauthorized",
                "Content-Type: application/json\r\n",
                &unauthorized,
            ),
            Error::Unauthorized,
        ),
        (
            response(
                "403 Forbidden",
                "Content-Type: application/json\r\n",
                &forbidden,
            ),
            Error::Forbidden,
        ),
        (
            response("403 Forbidden", "Content-Type: text/html\r\n", "sign in"),
            Error::GatewayAuthRequired,
        ),
        (
            response(
                "403 Forbidden",
                "Content-Type: application/json\r\n",
                "{\"error\":\"login\"}",
            ),
            Error::GatewayAuthRequired,
        ),
        (
            response("401 Unauthorized", "Content-Type: text/html\r\n", "sign in"),
            Error::GatewayAuthRequired,
        ),
        (
            response(
                "200 OK",
                "Content-Type: text/html; charset=utf-8\r\n",
                "sign in",
            ),
            Error::GatewayAuthRequired,
        ),
        (
            response("302 Found", "Location: https://example.test/login\r\n", ""),
            Error::GatewayAuthRequired,
        ),
    ] {
        let (session, server) = session_with_reply(reply).await;
        assert_eq!(session.server_info().await, Err(expected));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn authenticated_redirect_does_not_forward_bearer_to_another_origin() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let reply = response(
        "307 Temporary Redirect",
        &format!(
            "Location: http://{}/v1/server-info\r\n",
            target.local_addr().unwrap()
        ),
        "",
    );
    let (session, server) = session_with_reply(reply).await;
    assert_eq!(session.server_info().await, Err(Error::GatewayAuthRequired));
    let first_request = server.await.unwrap();
    assert!(
        first_request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-device-token")
    );
    assert!(
        timeout(Duration::from_millis(250), target.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn content_requires_identity_encoding_and_unambiguous_headers() {
    let valid = "Content-Type: application/octet-stream\r\nETag: \"revision\"\r\nContent-Digest: sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:\r\n";
    for extra in [
        "Content-Encoding: gzip\r\n",
        "Content-Encoding: identity\r\nContent-Encoding: identity\r\n",
        "ETag: \"revision\"\r\n",
        "Content-Digest: sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:\r\n",
        "Content-Type: application/octet-stream\r\n",
    ] {
        let reply = response("200 OK", &format!("{valid}{extra}"), "");
        let (session, server) = session_with_reply(reply).await;
        assert_eq!(session.content(&node()).await, Err(Error::Contract));
        server.await.unwrap();
    }
    let reply = response(
        "200 OK",
        &format!("{valid}Content-Encoding: identity\r\n"),
        "",
    );
    let (session, server) = session_with_reply(reply).await;
    assert_eq!(session.content(&node()).await, Ok(Vec::new()));
    let request = server.await.unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("accept-encoding: identity")
    );
}
