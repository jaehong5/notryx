mod support;

use notryx_protocol::{AdminRequest, AdminResponse, PairResponse};
use notryx_server::{admin, config::Config, http::RunningServer};
use serde_json::Value;
use std::net::SocketAddr;
use support::{initialized, pairing_request, post_json, request};

async fn pair(config: &Config, address: SocketAddr) -> PairResponse {
    let (request, _, _) = pairing_request(config).await;
    let response = post_json(address, &serde_json::to_vec(&request).unwrap()).await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).unwrap()
}

#[tokio::test]
async fn self_identity_reports_only_the_authenticated_device_and_rejects_revocation() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let first = pair(&config, server.local_addr()).await;
    let second = pair(&config, server.local_addr()).await;
    assert_ne!(first.device_id, second.device_id);
    assert_ne!(first.device_stream_id, second.device_stream_id);
    assert_eq!(first.workspace_id, second.workspace_id);
    assert_eq!(first.workspace_epoch, second.workspace_epoch);

    for paired in [&first, &second] {
        let authorization = format!("Bearer {}", paired.device_token);
        let response = request(
            server.local_addr(),
            "GET",
            "/v1/devices/self",
            &[("Authorization", &authorization)],
            b"",
        )
        .await;
        assert_eq!(
            response.status, 200,
            "authenticated self identity must be available"
        );
        assert!(
            !response
                .body
                .windows(paired.device_token.len())
                .any(|part| part == paired.device_token.as_bytes())
        );
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        let object = body.as_object().unwrap();
        assert_eq!(object.len(), 5);
        assert_eq!(body["protocol"], notryx_protocol::CONTRACT);
        assert_eq!(body["workspace_id"], paired.workspace_id.to_string());
        assert_eq!(body["workspace_epoch"], paired.workspace_epoch.to_string());
        assert_eq!(body["device_id"], paired.device_id);
        assert_eq!(body["device_stream_id"], paired.device_stream_id);
        assert!(
            response
                .header("cache-control")
                .is_some_and(|header| header.contains("no-store"))
        );
    }
    let anonymous = request(server.local_addr(), "GET", "/v1/devices/self", &[], b"").await;
    assert_eq!(anonymous.status, 401);
    assert!(
        anonymous
            .header("cache-control")
            .is_some_and(|header| header.contains("no-store"))
    );
    let revoked = admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: first.device_id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(revoked, AdminResponse::Ok { .. }));
    let authorization = format!("Bearer {}", first.device_token);
    let denied = request(
        server.local_addr(),
        "GET",
        "/v1/devices/self",
        &[("Authorization", &authorization)],
        b"",
    )
    .await;
    assert_eq!(denied.status, 401);
    assert!(
        denied
            .header("cache-control")
            .is_some_and(|header| header.contains("no-store"))
    );
    assert!(
        !denied
            .body
            .windows(first.device_token.len())
            .any(|part| part == first.device_token.as_bytes())
    );
    let authorization = format!("Bearer {}", second.device_token);
    let other = request(
        server.local_addr(),
        "GET",
        "/v1/devices/self",
        &[("Authorization", &authorization)],
        b"",
    )
    .await;
    assert_eq!(other.status, 200);
    server.shutdown().await.unwrap();
}
