use notryx_client_runtime::{Binding, Error, Session, StoredCredential, TokenExpiry};
use notryx_server::{
    Workspace,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::{Duration, timeout},
};
use uuid::Uuid;

fn renewal(binding: &Binding) -> Value {
    json!({
        "protocol": "mdw-sync/1.2",
        "workspace_id": binding.workspace_id,
        "workspace_epoch": binding.workspace_epoch,
        "device_id": binding.device_id,
        "device_stream_id": binding.device_stream_id,
        "idle_expires_at": "1800000000",
        "absolute_expires_at": "1820000000"
    })
}

fn response(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn mock_session(reply: impl FnOnce(&Binding) -> String) -> (Session, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let binding = Binding {
        origin: format!("http://{}", listener.local_addr().unwrap()),
        workspace_id: Uuid::new_v4().to_string().parse().unwrap(),
        workspace_epoch: Uuid::new_v4().to_string().parse().unwrap(),
        device_id: Uuid::new_v4(),
        device_stream_id: Uuid::new_v4(),
    };
    let reply = reply(&binding);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0 && request.len() + count <= 8192);
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        stream.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8(request).unwrap()
    });
    let credential: StoredCredential = serde_json::from_value(json!({
        "binding": binding,
        "token": "synthetic-device-token"
    }))
    .unwrap();
    (
        Session::from_stored_credential(&binding, &credential).unwrap(),
        server,
    )
}

#[tokio::test]
async fn pairing_retains_expiry_and_real_renewal_preserves_the_credential() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let grant = workspace.issue_pairing(owner, now as i64).await.unwrap();
    workspace.close().await.unwrap();
    let server = RunningServer::start(Config {
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
    })
    .await
    .unwrap();
    let session = Session::pair(
        &format!("http://{}", server.local_addr()),
        grant.workspace_id,
        &grant.pairing_secret,
    )
    .await
    .unwrap();
    let initial = session
        .token_expiry()
        .expect("pairing must retain the server expiry");
    assert!(initial.idle_expires_at >= now + 90 * 24 * 60 * 60);
    assert_eq!(
        initial.absolute_expires_at - initial.idle_expires_at,
        275 * 24 * 60 * 60
    );
    let stored = session.stored_credential().unwrap();
    let before = serde_json::to_vec(&stored).unwrap();
    assert_eq!(
        serde_json::to_value(&stored)
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        2
    );
    let restored = Session::from_stored_credential(session.binding(), &stored).unwrap();
    assert_eq!(restored.token_expiry(), None);
    assert_eq!(restored.renew_device().await, Ok(initial));
    assert_eq!(restored.renew_device().await, Ok(initial));
    assert!(serde_json::to_vec(&restored.stored_credential().unwrap()).unwrap() == before);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn renewal_posts_without_body_and_returns_only_validated_expiry() {
    let (session, server) = mock_session(|binding| {
        response(
            "200 OK",
            "Content-Type: application/json\r\n",
            &renewal(binding).to_string(),
        )
    })
    .await;
    let before = serde_json::to_vec(&session.stored_credential().unwrap()).unwrap();
    assert_eq!(
        session.renew_device().await,
        Ok(TokenExpiry {
            idle_expires_at: 1_800_000_000,
            absolute_expires_at: 1_820_000_000
        })
    );
    assert_eq!(session.token_expiry(), None);
    assert!(serde_json::to_vec(&session.stored_credential().unwrap()).unwrap() == before);
    let request = server.await.unwrap().to_ascii_lowercase();
    assert!(request.starts_with("post /v1/devices/self/renew http/1.1\r\n"));
    assert!(request.contains("authorization: bearer synthetic-device-token\r\n"));
    assert!(request.ends_with("\r\n\r\n"));
    assert!(!request.contains("transfer-encoding:"));
    assert!(!request.contains("content-length:") || request.contains("content-length: 0\r\n"));
}

#[tokio::test]
async fn renewal_rejects_other_registration_bindings() {
    for field in [
        "workspace_id",
        "workspace_epoch",
        "device_id",
        "device_stream_id",
    ] {
        let (session, server) = mock_session(|binding| {
            let mut body = renewal(binding);
            body[field] = json!(Uuid::new_v4());
            response(
                "200 OK",
                "Content-Type: application/json\r\n",
                &body.to_string(),
            )
        })
        .await;
        assert_eq!(session.renew_device().await, Err(Error::Binding), "{field}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn renewal_rejects_invalid_contract_and_expiries() {
    for (field, value) in [
        ("protocol", json!("mdw-sync/1.1")),
        ("unexpected", json!(true)),
        ("idle_expires_at", json!("0")),
        ("idle_expires_at", json!("1820000001")),
        ("idle_expires_at", json!(1_800_000_000u64)),
        ("absolute_expires_at", json!("-1")),
        ("absolute_expires_at", json!("9223372036854775808")),
        ("device_id", json!(Uuid::now_v7())),
    ] {
        let (session, server) = mock_session(|binding| {
            let mut body = renewal(binding);
            body[field] = value;
            response(
                "200 OK",
                "Content-Type: application/json\r\n",
                &body.to_string(),
            )
        })
        .await;
        assert_eq!(
            session.renew_device().await,
            Err(Error::Contract),
            "{field}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn renewal_distinguishes_unsupported_expired_transient_and_gateway_responses() {
    let unauthorized = json!({"code":"unauthorized", "message":"authentication required", "request_id":Uuid::now_v7()}).to_string();
    for (status, media, body, expected) in [
        (
            "404 Not Found",
            "application/json",
            "{}",
            Error::RenewalUnsupported,
        ),
        (
            "401 Unauthorized",
            "application/json",
            unauthorized.as_str(),
            Error::Unauthorized,
        ),
        (
            "429 Too Many Requests",
            "application/json",
            "{}",
            Error::Unavailable,
        ),
        (
            "503 Service Unavailable",
            "application/json",
            "{}",
            Error::Unavailable,
        ),
        (
            "404 Not Found",
            "text/html",
            "sign in",
            Error::GatewayAuthRequired,
        ),
        (
            "401 Unauthorized",
            "text/html",
            "sign in",
            Error::GatewayAuthRequired,
        ),
        ("200 OK", "text/html", "sign in", Error::GatewayAuthRequired),
    ] {
        let (session, server) =
            mock_session(|_| response(status, &format!("Content-Type: {media}\r\n"), body)).await;
        assert_eq!(session.renew_device().await, Err(expected));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn renewal_redirect_never_forwards_the_device_token() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (session, server) = mock_session(|_| {
        response(
            "307 Temporary Redirect",
            &format!(
                "Location: http://{}/v1/devices/self/renew\r\n",
                target.local_addr().unwrap()
            ),
            "",
        )
    })
    .await;
    assert_eq!(
        session.renew_device().await,
        Err(Error::GatewayAuthRequired)
    );
    server.await.unwrap();
    assert!(
        timeout(Duration::from_millis(250), target.accept())
            .await
            .is_err()
    );
}
