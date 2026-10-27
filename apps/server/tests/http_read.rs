pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, ApiError, CONTRACT, NodeKind, NodeMetadata,
    PairRequest, PairResponse, ServerInfo, WorkspaceRole,
};
use notryx_server::admin;
use notryx_server::config::Config;
use notryx_server::http::RunningServer;
use notryx_server::{Document, Error, Outcome, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::net::SocketAddr;
use support::{HttpResponse, initialized, pairing_request, post_json, request};

const ORIGINAL: &[u8] = b"\xef\xbb\xbf---\r\nsynthetic: true\r\n---\r\n# bytes\r\n\xff\0\r\n";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap()
}

async fn adopt(config: &Config, bytes: &[u8]) -> Document {
    std::fs::write(config.workspace.notes_root.join("Document.md"), bytes).unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    document
}

async fn pair_owner(config: &Config, address: SocketAddr) -> PairResponse {
    let (pair, _, _) = pairing_request(config).await;
    let response = post_json(address, &serde_json::to_vec(&pair).unwrap()).await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body)
        .unwrap_or_else(|_| panic!("pairing must return a valid device response"))
}

async fn pair_at(workspace: &mut Workspace, time: i64) -> PairResponse {
    let status = workspace.auth_status().await.unwrap();
    let grant = workspace
        .issue_pairing(status.owner_principal_id, time)
        .await
        .unwrap();
    workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            time,
        )
        .await
        .unwrap()
}

async fn get(address: SocketAddr, path: &str, token: &str, extra: &[(&str, &str)]) -> HttpResponse {
    let authorization = format!("Bearer {token}");
    let mut headers = vec![("Authorization", authorization.as_str())];
    headers.extend_from_slice(extra);
    let response = request(address, "GET", path, &headers, &[]).await;
    if response.status >= 400 {
        assert!(!String::from_utf8_lossy(&response.body).contains(token));
    }
    response
}

fn assert_private(response: &HttpResponse) {
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
}

fn assert_error(response: &HttpResponse, status: u16, code: &str) {
    assert_eq!(response.status, status);
    assert_private(response);
    if status == 401 {
        assert_eq!(
            response.header("www-authenticate"),
            Some("Bearer realm=\"notryx\"")
        );
    }
    let error = serde_json::from_slice::<ApiError>(&response.body)
        .unwrap_or_else(|_| panic!("read failure must use the public error contract"));
    assert!(error.code == code);
    assert!(!error.message.trim().is_empty());
    let request_id = uuid::Uuid::parse_str(&error.request_id).unwrap();
    assert_eq!(request_id.get_version_num(), 7);
    assert_eq!(request_id.get_variant(), uuid::Variant::RFC4122);
}

fn assert_content(response: &HttpResponse, document: &Document, bytes: &[u8]) {
    assert_eq!(response.status, 200);
    assert_private(response);
    assert!(response.body == bytes);
    assert_eq!(
        response.header("content-type"),
        Some("application/octet-stream")
    );
    let etag = format!("\"{}\"", document.content_rev.as_str());
    assert_eq!(response.header("etag"), Some(etag.as_str()));
    let digest = format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(bytes)));
    assert_eq!(response.header("content-digest"), Some(digest.as_str()));
    assert!(response.header("content-encoding").is_none());
}

async fn save_content(
    workspace: &mut Workspace,
    document: &Document,
    bytes: &[u8],
) -> Result<notryx_server::Receipt, Error> {
    let stream_id = workspace.register_stream().await.unwrap();
    workspace
        .save(SaveContent {
            epoch: workspace.epoch(),
            stream_id,
            operation_seq: 1,
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_content_rev: document.content_rev.clone(),
            content_hash: Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: bytes.to_vec(),
        })
        .await
}

#[tokio::test]
async fn anonymous_server_info_requires_device_authentication() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config).await.unwrap();
    let response = request(server.local_addr(), "GET", "/v1/server-info", &[], &[]).await;
    assert_error(&response, 401, "unauthorized");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn read_routes_reject_missing_invalid_and_duplicate_bearer_headers() {
    let (_directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let spaced = format!("Bearer  {}", paired.device_token);
    let response = request(
        server.local_addr(),
        "GET",
        "/v1/server-info",
        &[("Authorization", &spaced)],
        &[],
    )
    .await;
    assert_eq!(response.status, 200);
    let authorization = format!("Bearer {}", paired.device_token);
    let internal_space = format!("Bearer {} extra", paired.device_token);
    let internal_tab = format!("Bearer {}\textra", paired.device_token);
    let tab_separator = format!("Bearer\t{}", paired.device_token);
    let node = uuid::Uuid::now_v7();
    for path in [
        "/v1/server-info".to_owned(),
        format!("/v1/nodes/{node}/metadata"),
        format!("/v1/nodes/{node}/content"),
    ] {
        for headers in [
            vec![],
            vec![("Authorization", "Basic synthetic")],
            vec![("Authorization", "Bearer")],
            vec![("Authorization", "Bearer synthetic-invalid-token")],
            vec![("Authorization", internal_space.as_str())],
            vec![("Authorization", internal_tab.as_str())],
            vec![("Authorization", tab_separator.as_str())],
            vec![
                ("Authorization", authorization.as_str()),
                ("Authorization", authorization.as_str()),
            ],
            vec![
                ("Authorization", authorization.as_str()),
                ("Authorization", "Bearer synthetic-invalid-token"),
            ],
        ] {
            let response = request(server.local_addr(), "GET", &path, &headers, &[]).await;
            assert_error(&response, 401, "unauthorized");
            assert!(!String::from_utf8_lossy(&response.body).contains(&paired.device_token));
        }
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn registered_device_reads_exact_bytes_and_stable_metadata_across_save_restart_and_revoke() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config, ORIGINAL).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let metadata_path = format!("/v1/nodes/{}/metadata", document.node_id);
    let content_path = format!("/v1/nodes/{}/content", document.node_id);

    let response = get(
        server.local_addr(),
        "/v1/server-info",
        &paired.device_token,
        &[],
    )
    .await;
    assert_eq!(response.status, 200);
    assert_private(&response);
    let info: ServerInfo = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(info.protocol, CONTRACT);
    assert_eq!(info.workspace_id, paired.workspace_id);
    assert_eq!(info.workspace_epoch, paired.workspace_epoch);
    assert_eq!(info.root_id, document.parent_id);
    assert_eq!(info.head_seq, document.last_change_seq);
    assert_eq!(info.journal_floor_seq.get(), 0);
    assert!(info.ready);
    assert_eq!(info.max_document_bytes, 5 * 1024 * 1024);

    let metadata = get(
        server.local_addr(),
        &metadata_path,
        &paired.device_token,
        &[],
    )
    .await;
    assert_eq!(metadata.status, 200);
    assert_private(&metadata);
    let node: NodeMetadata = serde_json::from_slice(&metadata.body).unwrap();
    assert_eq!(node.node_id, document.node_id);
    assert_eq!(node.parent_id, Some(document.parent_id));
    assert_eq!(node.name, "Document.md");
    assert_eq!(node.kind, NodeKind::File);
    assert!(!node.deleted);
    assert_eq!(node.meta_rev, document.meta_rev);
    assert_eq!(
        node.meta_etag,
        format!("\"{}\"", document.meta_rev.as_str())
    );
    assert_eq!(metadata.header("etag"), Some(node.meta_etag.as_str()));

    let root = get(
        server.local_addr(),
        &format!("/v1/nodes/{}/metadata", info.root_id),
        &paired.device_token,
        &[],
    )
    .await;
    assert_eq!(root.status, 200);
    let root: NodeMetadata = serde_json::from_slice(&root.body).unwrap();
    assert_eq!(root.node_id, info.root_id);
    assert_eq!(root.parent_id, None);
    assert_eq!(root.kind, NodeKind::Folder);
    assert!(!root.deleted);
    let response = get(
        server.local_addr(),
        &content_path,
        &paired.device_token,
        &[],
    )
    .await;
    assert_content(&response, &document, ORIGINAL);
    server.shutdown().await.unwrap();

    let changed_bytes = b"\xef\xbb\xbf# committed replacement\r\n";
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let saved = save_content(&mut workspace, &document, changed_bytes)
        .await
        .unwrap();
    let Outcome::Saved(changed) = saved.outcome else {
        panic!("conditional local fixture save must succeed")
    };
    assert_ne!(changed.content_rev, document.content_rev);
    workspace.close().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let after = get(
        server.local_addr(),
        &metadata_path,
        &paired.device_token,
        &[],
    )
    .await;
    assert_eq!(after.status, 200);
    assert!(after.body == metadata.body);
    assert_eq!(after.header("etag"), metadata.header("etag"));
    let response = get(
        server.local_addr(),
        &content_path,
        &paired.device_token,
        &[],
    )
    .await;
    assert_content(&response, &changed, changed_bytes);

    let revoked = admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: paired.device_id,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        revoked,
        AdminResponse::Ok {
            result: AdminResult::Done {}
        }
    ));
    for path in ["/v1/server-info", &metadata_path, &content_path] {
        let response = get(server.local_addr(), path, &paired.device_token, &[]).await;
        assert_error(&response, 401, "unauthorized");
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn conditional_content_reads_enforce_etags_and_reject_unsupported_methods_and_remote_admin() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config, ORIGINAL).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let path = format!("/v1/nodes/{}/content", document.node_id);
    let etag = format!("\"{}\"", document.content_rev.as_str());
    let response = get(
        server.local_addr(),
        &path,
        &paired.device_token,
        &[("If-Match", &etag)],
    )
    .await;
    assert_content(&response, &document, ORIGINAL);
    let wrong = get(
        server.local_addr(),
        &path,
        &paired.device_token,
        &[("If-Match", "\"wrong-revision\"")],
    )
    .await;
    assert_error(&wrong, 412, "content_revision_mismatch");
    for invalid in [
        "*",
        "W/\"revision\"",
        "\"first\", \"second\"",
        "unquoted",
        "\"first\"second\"",
    ] {
        let response = get(
            server.local_addr(),
            &path,
            &paired.device_token,
            &[("If-Match", invalid)],
        )
        .await;
        assert_error(&response, 400, "invalid_request");
    }
    let duplicate = get(
        server.local_addr(),
        &path,
        &paired.device_token,
        &[("If-Match", &etag), ("If-Match", &etag)],
    )
    .await;
    assert_error(&duplicate, 400, "invalid_request");
    let missing = uuid::Uuid::now_v7();
    for resource in ["metadata", "content"] {
        let response = get(
            server.local_addr(),
            &format!("/v1/nodes/{missing}/{resource}"),
            &paired.device_token,
            &[],
        )
        .await;
        assert_error(&response, 404, "not_found");
        let response = get(
            server.local_addr(),
            &format!("/v1/nodes/{}/{resource}", uuid::Uuid::new_v4()),
            &paired.device_token,
            &[],
        )
        .await;
        assert_error(&response, 400, "invalid_request");
    }
    let readiness = request(server.local_addr(), "GET", "/health/ready", &[], &[]).await;
    assert_eq!(readiness.status, 200);
    let folder = get(
        server.local_addr(),
        &format!("/v1/nodes/{}/content", document.parent_id),
        &paired.device_token,
        &[],
    )
    .await;
    assert_error(&folder, 404, "not_found");
    let remote_admin = get(
        server.local_addr(),
        "/v1/admin/status",
        &paired.device_token,
        &[],
    )
    .await;
    assert_error(&remote_admin, 404, "not_found");
    let authorization = format!("Bearer {}", paired.device_token);
    let remote_write = request(
        server.local_addr(),
        "PATCH",
        &path,
        &[("Authorization", &authorization)],
        b"must not replace content",
    )
    .await;
    assert_error(&remote_write, 405, "invalid_request");
    assert!(remote_write.header("allow").is_some());
    assert!(!String::from_utf8_lossy(&remote_write.body).contains(&paired.device_token));
    let unchanged = get(server.local_addr(), &path, &paired.device_token, &[]).await;
    assert_content(&unchanged, &document, ORIGINAL);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_and_editor_devices_can_read_all_three_resources() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config, ORIGINAL).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    for role in [WorkspaceRole::Reader, WorkspaceRole::Editor] {
        let principal = admin::request(
            &config.server.admin_socket,
            &AdminRequest::CreatePrincipal { role },
        )
        .await
        .unwrap();
        let AdminResponse::Ok {
            result: AdminResult::Principal { principal_id },
        } = principal
        else {
            panic!("local admin must create the requested principal")
        };
        let pairing = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing { principal_id },
        )
        .await
        .unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Pairing {
                    workspace_id,
                    pairing_secret,
                    ..
                },
        } = pairing
        else {
            panic!("local admin must issue a pairing secret")
        };
        let response = post_json(
            server.local_addr(),
            &serde_json::to_vec(&PairRequest {
                protocol: CONTRACT.into(),
                workspace_id,
                pairing_secret,
            })
            .unwrap(),
        )
        .await;
        assert_eq!(response.status, 200);
        let paired: PairResponse = serde_json::from_slice(&response.body)
            .unwrap_or_else(|_| panic!("pairing response must match the contract"));
        assert_eq!(paired.role, role);
        for path in [
            "/v1/server-info".into(),
            format!("/v1/nodes/{}/metadata", document.node_id),
            format!("/v1/nodes/{}/content", document.node_id),
        ] {
            let response = get(server.local_addr(), &path, &paired.device_token, &[]).await;
            assert_eq!(response.status, 200);
            assert_private(&response);
        }
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_and_foreign_workspace_tokens_fail_actual_http_authentication() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config, ORIGINAL).await;
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let fresh = pair_at(&mut workspace, now()).await;
    let expired = pair_at(&mut workspace, now() - 91 * 24 * 60 * 60).await;
    workspace.close().await.unwrap();
    let server = RunningServer::start(config).await.unwrap();
    let (_other_directory, other_config) = initialized().await;
    let other = RunningServer::start(other_config).await.unwrap();
    let response = get(
        server.local_addr(),
        "/v1/server-info",
        &fresh.device_token,
        &[],
    )
    .await;
    assert_eq!(response.status, 200);
    for path in [
        "/v1/server-info".into(),
        format!("/v1/nodes/{}/metadata", document.node_id),
        format!("/v1/nodes/{}/content", document.node_id),
    ] {
        let response = get(server.local_addr(), &path, &expired.device_token, &[]).await;
        assert_error(&response, 401, "unauthorized");
        let response = get(other.local_addr(), &path, &fresh.device_token, &[]).await;
        assert_error(&response, 401, "unauthorized");
    }
    other.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn quarantine_keeps_committed_reads_available_without_exposing_uncommitted_file_bytes() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config, ORIGINAL).await;
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let paired = pair_at(&mut workspace, now()).await;
    let mut fault = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_read_fixture_finalize BEFORE UPDATE OF content_rev ON nodes BEGIN SELECT RAISE(ABORT, 'synthetic finalization failure'); END;")
        .execute(&mut fault).await.unwrap();
    assert_eq!(
        save_content(&mut workspace, &document, b"uncommitted proposed bytes").await,
        Err(Error::Storage)
    );
    sqlx::query("DROP TRIGGER fail_read_fixture_finalize")
        .execute(&mut fault)
        .await
        .unwrap();
    fault.close().await.unwrap();
    workspace.close().await.unwrap();
    let external = b"synthetic external bytes must not become the committed response";
    std::fs::write(config.workspace.notes_root.join("Document.md"), external).unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let ready = request(server.local_addr(), "GET", "/health/ready", &[], &[]).await;
    assert_eq!(ready.status, 503);
    let response = get(
        server.local_addr(),
        "/v1/server-info",
        &paired.device_token,
        &[],
    )
    .await;
    assert_eq!(response.status, 200);
    let info: ServerInfo = serde_json::from_slice(&response.body).unwrap();
    assert!(!info.ready);
    assert_eq!(info.head_seq, document.last_change_seq);
    let metadata = get(
        server.local_addr(),
        &format!("/v1/nodes/{}/metadata", document.node_id),
        &paired.device_token,
        &[],
    )
    .await;
    assert_eq!(metadata.status, 200);
    let metadata: NodeMetadata = serde_json::from_slice(&metadata.body).unwrap();
    assert_eq!(metadata.meta_rev, document.meta_rev);
    let content = get(
        server.local_addr(),
        &format!("/v1/nodes/{}/content", document.node_id),
        &paired.device_token,
        &[],
    )
    .await;
    assert_content(&content, &document, ORIGINAL);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        external
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_committed_object_returns_a_private_storage_error_and_preserves_the_source() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config, ORIGINAL).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let object = config
        .workspace
        .state_root
        .join("objects/sha256")
        .join(&document.content_hash);
    std::fs::remove_file(&object).unwrap();
    let response = get(
        server.local_addr(),
        &format!("/v1/nodes/{}/content", document.node_id),
        &paired.device_token,
        &[],
    )
    .await;
    assert_error(&response, 503, "storage_unavailable");
    let readiness = request(server.local_addr(), "GET", "/health/ready", &[], &[]).await;
    assert_eq!(readiness.status, 503);
    let info = get(
        server.local_addr(),
        "/v1/server-info",
        &paired.device_token,
        &[],
    )
    .await;
    assert_eq!(info.status, 200);
    let info: ServerInfo = serde_json::from_slice(&info.body).unwrap();
    assert!(!info.ready);
    let error_body = String::from_utf8_lossy(&response.body);
    assert!(!error_body.contains(object.to_str().unwrap()));
    assert!(!error_body.contains(config.workspace.notes_root.to_str().unwrap()));
    assert!(!error_body.contains("synthetic: true"));
    assert!(!error_body.contains(&paired.device_token));
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        ORIGINAL
    );
    server.shutdown().await.unwrap();
}
