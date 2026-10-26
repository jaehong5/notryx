mod support;

use notryx_protocol::{ApiError, CONTRACT, PairResponse, WorkspaceRole};
use notryx_server::http::{RunningServer, ServerError};
use notryx_server::runtime::WorkspaceRuntime;
use serde_json::json;
use support::{HttpResponse, initialized, pairing_request, post_json};

fn assert_private_json(response: &HttpResponse) {
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert!(
        response
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    );
}

fn assert_error(response: &HttpResponse, status: u16, code: &str, secret: &str) {
    assert_eq!(response.status, status);
    assert_private_json(response);
    let error = serde_json::from_slice::<ApiError>(&response.body)
        .unwrap_or_else(|_| panic!("error response must use the public error contract"));
    assert!(error.code == code);
    assert!(!error.message.trim().is_empty());
    let request_id = uuid::Uuid::parse_str(&error.request_id).unwrap();
    assert_eq!(request_id.get_version_num(), 7);
    assert_eq!(request_id.get_variant(), uuid::Variant::RFC4122);
    assert!(!String::from_utf8_lossy(&response.body).contains(secret));
}

#[tokio::test]
async fn malformed_pairing_json_returns_a_generic_private_error() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config).await.unwrap();
    let response = post_json(
        server.local_addr(),
        b"{\"pairing_secret\":\"synthetic-secret-must-not-leak\",",
    )
    .await;
    assert_error(
        &response,
        400,
        "invalid_request",
        "synthetic-secret-must-not-leak",
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_admin_pairing_returns_a_private_token_and_rejects_reuse() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let (request, epoch, principal) = pairing_request(&config).await;
    let body = serde_json::to_vec(&request).unwrap();
    let response = post_json(server.local_addr(), &body).await;
    assert_eq!(response.status, 200);
    assert_private_json(&response);
    let paired = serde_json::from_slice::<PairResponse>(&response.body)
        .unwrap_or_else(|_| panic!("successful pairing must match the response contract"));
    assert!(paired.protocol == CONTRACT);
    assert_eq!(paired.workspace_id, request.workspace_id);
    assert_eq!(paired.workspace_epoch, epoch);
    assert!(paired.principal_id == principal);
    assert_eq!(paired.role, WorkspaceRole::Owner);
    for id in [
        &paired.principal_id,
        &paired.device_id,
        &paired.device_stream_id,
    ] {
        let id = uuid::Uuid::parse_str(id).unwrap();
        assert_eq!(id.get_version_num(), 4);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    }
    assert!(!paired.device_token.is_empty());
    assert!(paired.device_token != request.pairing_secret);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let idle = paired.idle_expires_at.parse::<u64>().unwrap();
    let absolute = paired.absolute_expires_at.parse::<u64>().unwrap();
    assert!(now < idle && idle < absolute);
    assert!(!String::from_utf8_lossy(&response.body).contains(&request.pairing_secret));

    let repeated = post_json(server.local_addr(), &body).await;
    assert_error(&repeated, 401, "unauthorized", &request.pairing_secret);
    assert!(!String::from_utf8_lossy(&repeated.body).contains(&paired.device_token));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_pairing_consumes_the_secret_only_once() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let (request, _, _) = pairing_request(&config).await;
    let body = serde_json::to_vec(&request).unwrap();
    let (first, second) = tokio::join!(
        post_json(server.local_addr(), &body),
        post_json(server.local_addr(), &body)
    );
    let mut statuses = [first.status, second.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 401]);
    for response in [first, second] {
        assert_private_json(&response);
        if response.status == 401 {
            assert_error(&response, 401, "unauthorized", &request.pairing_secret);
        }
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_pairing_requests_do_not_consume_a_valid_secret() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let (request, _, _) = pairing_request(&config).await;
    let valid = serde_json::to_value(&request).unwrap();

    let mut unknown = valid.clone();
    unknown["unexpected"] = json!("synthetic-private-field");
    let response = post_json(server.local_addr(), &serde_json::to_vec(&unknown).unwrap()).await;
    assert_error(&response, 400, "invalid_request", &request.pairing_secret);
    assert!(!String::from_utf8_lossy(&response.body).contains("synthetic-private-field"));

    let mut protocol = valid.clone();
    protocol["protocol"] = json!("mdw-sync/1.1");
    let response = post_json(server.local_addr(), &serde_json::to_vec(&protocol).unwrap()).await;
    assert_error(
        &response,
        409,
        "unsupported_protocol",
        &request.pairing_secret,
    );

    let mut invalid_id = valid.clone();
    invalid_id["workspace_id"] = json!(uuid::Uuid::now_v7().to_string());
    let response = post_json(
        server.local_addr(),
        &serde_json::to_vec(&invalid_id).unwrap(),
    )
    .await;
    assert_error(&response, 400, "invalid_request", &request.pairing_secret);

    let mut wrong_workspace = valid.clone();
    wrong_workspace["workspace_id"] = json!(uuid::Uuid::new_v4().to_string());
    let response = post_json(
        server.local_addr(),
        &serde_json::to_vec(&wrong_workspace).unwrap(),
    )
    .await;
    assert_error(&response, 401, "unauthorized", &request.pairing_secret);

    let mut wrong_secret = valid.clone();
    wrong_secret["pairing_secret"] = json!("synthetic-wrong-secret");
    let response = post_json(
        server.local_addr(),
        &serde_json::to_vec(&wrong_secret).unwrap(),
    )
    .await;
    assert_error(&response, 401, "unauthorized", "synthetic-wrong-secret");

    let response = post_json(server.local_addr(), &serde_json::to_vec(&valid).unwrap()).await;
    assert_eq!(response.status, 200);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn pairing_rejects_a_body_over_one_mib_before_deserialization() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config).await.unwrap();
    let body = serde_json::to_vec(&json!({
        "protocol": CONTRACT,
        "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        "pairing_secret": "x".repeat(1024 * 1024),
    }))
    .unwrap();
    assert!(body.len() > 1024 * 1024);
    let response = post_json(server.local_addr(), &body).await;
    assert_error(&response, 413, "invalid_request", "xxxxxxxxxxxxxxxx");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_admin_socket_startup_preserves_the_path_and_releases_workspace_locks() {
    let (_directory, config) = initialized().await;
    let preserved = b"existing socket path must remain";
    std::fs::write(&config.server.admin_socket, preserved).unwrap();
    assert!(matches!(
        RunningServer::start(config.clone()).await,
        Err(ServerError::Admin(_))
    ));
    assert_eq!(
        std::fs::read(&config.server.admin_socket).unwrap(),
        preserved
    );
    let runtime = WorkspaceRuntime::start(config.workspace).await.unwrap();
    assert!(runtime.handle().health().await.unwrap().ready);
    runtime.shutdown().await.unwrap();
}
