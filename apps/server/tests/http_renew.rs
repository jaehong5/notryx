mod support;

use notryx_protocol::{
    AdminRequest, AdminResponse, ApiError, CONTRACT, PairRequest, PairResponse, WorkspaceRole,
};
use notryx_server::{Workspace, admin, config::Config, http::RunningServer};
use serde_json::Value;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::time::{SystemTime, UNIX_EPOCH};
use support::{HttpResponse, initialized, pairing_request, post_json, request};

const DAY: i64 = 24 * 60 * 60;
const PATH: &str = "/v1/devices/self/renew";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn pair_at(config: &Config, role: WorkspaceRole, issued: i64) -> PairResponse {
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let principal = if role == WorkspaceRole::Owner {
        workspace.auth_status().await.unwrap().owner_principal_id
    } else {
        workspace.create_principal(role).await.unwrap()
    };
    let grant = workspace.issue_pairing(principal, issued).await.unwrap();
    let paired = workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            issued,
        )
        .await
        .unwrap();
    workspace.close().await.unwrap();
    paired
}

async fn renew(server: &RunningServer, paired: &PairResponse) -> HttpResponse {
    request(
        server.local_addr(),
        "POST",
        PATH,
        &[("Authorization", &format!("Bearer {}", paired.device_token))],
        b"",
    )
    .await
}

fn successful(response: &HttpResponse, paired: &PairResponse) -> Value {
    assert_eq!(response.status, 200);
    assert_eq!(response.header("cache-control"), Some("no-store"));
    serde_json::from_slice::<notryx_protocol::DeviceRenewal>(&response.body).unwrap();
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 7);
    assert_eq!(body["protocol"], CONTRACT);
    assert_eq!(body["workspace_id"], paired.workspace_id.to_string());
    assert_eq!(body["workspace_epoch"], paired.workspace_epoch.to_string());
    assert_eq!(body["device_id"], paired.device_id);
    assert_eq!(body["device_stream_id"], paired.device_stream_id);
    assert_eq!(body["absolute_expires_at"], paired.absolute_expires_at);
    assert!(!String::from_utf8_lossy(&response.body).contains(&paired.device_token));
    body
}

fn rejected(response: &HttpResponse, status: u16, code: &str) {
    assert_eq!(response.status, status);
    assert_eq!(response.header("cache-control"), Some("no-store"));
    let body: ApiError = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body.code, code);
}

async fn stored_identity(config: &Config) -> Vec<String> {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.workspace.state_root.join("state.db"))
            .read_only(true),
    )
    .await
    .unwrap();
    let rows = sqlx::query_scalar("SELECT json_array(device_id,principal_id,stream_id,token_id,hex(token_hash),issued_at,absolute_expires_at,revoked_at) FROM devices ORDER BY device_id").fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

#[tokio::test]
async fn renew_is_durable_and_retryable_for_every_role_without_changing_device_binding() {
    let (_directory, config) = initialized().await;
    let mut paired = Vec::new();
    for role in [
        WorkspaceRole::Owner,
        WorkspaceRole::Editor,
        WorkspaceRole::Reader,
    ] {
        paired.push(pair_at(&config, role, now() - 61 * DAY).await);
    }
    let before = stored_identity(&config).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let mut renewed = Vec::new();
    for device in &paired {
        let lower = now() + 90 * DAY;
        let first = successful(&renew(&server, device).await, device);
        let idle = first["idle_expires_at"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!((lower..=now() + 90 * DAY).contains(&idle));
        // The caller can recover a lost response using the original credential alone.
        assert_eq!(successful(&renew(&server, device).await, device), first);
        renewed.push(first);
    }
    server.shutdown().await.unwrap();
    assert_eq!(stored_identity(&config).await, before);
    let server = RunningServer::start(config.clone()).await.unwrap();
    for (device, expected) in paired.iter().zip(renewed) {
        assert_eq!(successful(&renew(&server, device).await, device), expected);
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn renew_before_the_window_returns_current_expiry_without_extending_it() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let (pair_request, _, _) = pairing_request(&config).await;
    let response = post_json(
        server.local_addr(),
        &serde_json::to_vec(&pair_request).unwrap(),
    )
    .await;
    assert_eq!(response.status, 200);
    let paired: PairResponse = serde_json::from_slice(&response.body).unwrap();
    let body = successful(&renew(&server, &paired).await, &paired);
    assert_eq!(body["idle_expires_at"], paired.idle_expires_at);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn renew_rejects_missing_malformed_expired_and_revoked_credentials() {
    let (_directory, config) = initialized().await;
    let expired = pair_at(&config, WorkspaceRole::Owner, now() - 91 * DAY).await;
    let absolute_expired = pair_at(&config, WorkspaceRole::Owner, now() - 366 * DAY).await;
    let revoked = pair_at(&config, WorkspaceRole::Owner, now() - 61 * DAY).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    rejected(
        &request(server.local_addr(), "POST", PATH, &[], b"").await,
        401,
        "unauthorized",
    );
    rejected(
        &request(
            server.local_addr(),
            "POST",
            PATH,
            &[("Authorization", "Bearer invalid")],
            b"",
        )
        .await,
        401,
        "unauthorized",
    );
    assert!(matches!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::RevokeDevice {
                device_id: revoked.device_id.clone()
            }
        )
        .await
        .unwrap(),
        AdminResponse::Ok { .. }
    ));
    for device in [&expired, &absolute_expired, &revoked] {
        let response = renew(&server, device).await;
        rejected(&response, 401, "unauthorized");
        assert!(!String::from_utf8_lossy(&response.body).contains(&device.device_token));
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn renew_rejects_undeclared_body_and_query_inputs() {
    let (_directory, config) = initialized().await;
    let paired = pair_at(&config, WorkspaceRole::Owner, now() - 61 * DAY).await;
    let server = RunningServer::start(config).await.unwrap();
    let authorization = format!("Bearer {}", paired.device_token);
    for (path, body) in [
        (PATH, b"{\"idle_expires_at\":\"9999999999\"}".as_slice()),
        ("/v1/devices/self/renew?device_id=other", b"".as_slice()),
    ] {
        rejected(
            &request(
                server.local_addr(),
                "POST",
                path,
                &[("Authorization", &authorization)],
                body,
            )
            .await,
            400,
            "invalid_request",
        );
    }
    server.shutdown().await.unwrap();
}
