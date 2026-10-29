mod support;

use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, CONTRACT, PairRequest, PairResponse, WorkspaceRole,
};
use notryx_server::{admin, config::Config, http::RunningServer};
use serde_json::Value;
use std::net::SocketAddr;
use support::{initialized, pairing_request, post_json, request};

async fn pair_principal(config: &Config, address: SocketAddr, principal_id: &str) -> PairResponse {
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing {
            principal_id: principal_id.into(),
        },
    )
    .await
    .unwrap()
    else {
        panic!("pairing grant");
    };
    let response = post_json(
        address,
        &serde_json::to_vec(&PairRequest {
            protocol: CONTRACT.into(),
            workspace_id,
            pairing_secret,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).unwrap()
}

async fn role(address: SocketAddr, paired: &PairResponse, expected: &str) {
    let authorization = format!("Bearer {}", paired.device_token);
    let response = request(
        address,
        "GET",
        "/v1/devices/self/role",
        &[("Authorization", &authorization)],
        b"",
    )
    .await;
    assert_eq!(
        response.status, 200,
        "current role must be available to an authenticated device"
    );
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert!(
        !response
            .body
            .windows(paired.device_token.len())
            .any(|part| part == paired.device_token.as_bytes())
    );
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 6);
    assert_eq!(body["protocol"], CONTRACT);
    assert_eq!(body["workspace_id"], paired.workspace_id.to_string());
    assert_eq!(body["workspace_epoch"], paired.workspace_epoch.to_string());
    assert_eq!(body["device_id"], paired.device_id);
    assert_eq!(body["device_stream_id"], paired.device_stream_id);
    assert_eq!(body["role"], expected);
}

#[tokio::test]
async fn current_role_is_authenticated_and_tracks_live_principal_role_for_each_device() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let (owner_request, _, _) = pairing_request(&config).await;
    let response = post_json(
        server.local_addr(),
        &serde_json::to_vec(&owner_request).unwrap(),
    )
    .await;
    let owner: PairResponse = serde_json::from_slice(&response.body).unwrap();
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Editor,
        },
    )
    .await
    .unwrap()
    else {
        panic!("principal");
    };
    let first = pair_principal(&config, server.local_addr(), &principal_id).await;
    let second = pair_principal(&config, server.local_addr(), &principal_id).await;
    assert_ne!(first.device_id, second.device_id);
    role(server.local_addr(), &owner, "owner").await;
    role(server.local_addr(), &first, "editor").await;
    role(server.local_addr(), &second, "editor").await;
    assert!(matches!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::SetRole {
                principal_id: principal_id.clone(),
                role: WorkspaceRole::Reader,
            }
        )
        .await
        .unwrap(),
        AdminResponse::Ok { .. }
    ));
    role(server.local_addr(), &first, "reader").await;
    role(server.local_addr(), &second, "reader").await;
    role(server.local_addr(), &owner, "owner").await;
    assert!(matches!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::SetRole {
                principal_id,
                role: WorkspaceRole::Editor,
            }
        )
        .await
        .unwrap(),
        AdminResponse::Ok { .. }
    ));
    role(server.local_addr(), &first, "editor").await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn current_role_rejects_anonymous_and_revoked_device_without_exposing_credentials() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let (request_body, _, _) = pairing_request(&config).await;
    let response = post_json(
        server.local_addr(),
        &serde_json::to_vec(&request_body).unwrap(),
    )
    .await;
    let paired: PairResponse = serde_json::from_slice(&response.body).unwrap();
    let anonymous = request(
        server.local_addr(),
        "GET",
        "/v1/devices/self/role",
        &[],
        b"",
    )
    .await;
    assert_eq!(anonymous.status, 401);
    assert_eq!(anonymous.header("cache-control"), Some("no-store"));
    assert!(matches!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::RevokeDevice {
                device_id: paired.device_id.clone(),
            }
        )
        .await
        .unwrap(),
        AdminResponse::Ok { .. }
    ));
    let authorization = format!("Bearer {}", paired.device_token);
    let revoked = request(
        server.local_addr(),
        "GET",
        "/v1/devices/self/role",
        &[("Authorization", &authorization)],
        b"",
    )
    .await;
    assert_eq!(revoked.status, 401);
    assert_eq!(revoked.header("cache-control"), Some("no-store"));
    assert!(
        !revoked
            .body
            .windows(paired.device_token.len())
            .any(|part| part == paired.device_token.as_bytes())
    );
    server.shutdown().await.unwrap();
}
