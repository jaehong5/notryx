mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, PairResponse, WorkspaceRole};
use notryx_server::admin;
use notryx_server::config::Config;
use notryx_server::http::RunningServer;
use notryx_server::{Document, Outcome, SaveContent, Workspace};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::net::SocketAddr;
use support::{HttpResponse, initialized, pairing_request, post_json, request};

const ORIGINAL: &[u8] = b"# Original\r\n";
const UPDATED: &[u8] = b"\xef\xbb\xbf---\r\nsynthetic: true\r\n---\r\n# kept\r\n\xff\0";

async fn adopt(config: &Config) -> Document {
    std::fs::write(config.workspace.notes_root.join("Document.md"), ORIGINAL).unwrap();
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
    serde_json::from_slice(&response.body).ok().unwrap()
}

async fn pair_role(config: &Config, address: SocketAddr, role: WorkspaceRole) -> PairResponse {
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal { role },
    )
    .await
    .unwrap()
    else {
        panic!("local admin must create the synthetic principal");
    };
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing { principal_id },
    )
    .await
    .unwrap()
    else {
        panic!("local admin must issue pairing");
    };
    let response = post_json(address, &serde_json::to_vec(&serde_json::json!({
        "protocol": "mdw-sync/1.2", "workspace_id": workspace_id, "pairing_secret": pairing_secret,
    })).unwrap()).await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).ok().unwrap()
}

fn headers(
    paired: &PairResponse,
    document: &Document,
    operation: &str,
    sequence: u64,
    bytes: &[u8],
) -> Vec<(String, String)> {
    vec![
        (
            "Authorization".into(),
            format!("Bearer {}", paired.device_token),
        ),
        (
            "X-Workspace-Epoch".into(),
            paired.workspace_epoch.to_string(),
        ),
        ("X-Device-Stream".into(), paired.device_stream_id.clone()),
        ("X-Operation-Seq".into(), sequence.to_string()),
        ("Idempotency-Key".into(), operation.into()),
        (
            "If-Match".into(),
            format!("\"{}\"", document.content_rev.as_str()),
        ),
        (
            "Content-Digest".into(),
            format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(bytes))),
        ),
    ]
}

fn replace(headers: &mut [(String, String)], name: &str, value: &str) {
    headers.iter_mut().find(|(key, _)| key == name).unwrap().1 = value.into();
}

async fn put(
    address: SocketAddr,
    document: &Document,
    headers: &[(String, String)],
    bytes: &[u8],
) -> HttpResponse {
    let headers: Vec<_> = headers
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    request(
        address,
        "PUT",
        &format!("/v1/nodes/{}/content", document.node_id),
        &headers,
        bytes,
    )
    .await
}

async fn get_operation(
    address: SocketAddr,
    paired: &PairResponse,
    operation: &str,
) -> HttpResponse {
    request(
        address,
        "GET",
        &format!("/v1/operations/{operation}"),
        &[("Authorization", &format!("Bearer {}", paired.device_token))],
        &[],
    )
    .await
}

fn value(response: &HttpResponse) -> Value {
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    serde_json::from_slice(&response.body).unwrap()
}

fn assert_error(response: &HttpResponse, status: u16, code: &str, token: &str) -> Value {
    assert_eq!(response.status, status);
    if status == 401 {
        assert_eq!(
            response.header("www-authenticate"),
            Some("Bearer realm=\"notryx\"")
        );
    }
    let body = value(response);
    assert!(body["code"] == code);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(
        uuid::Uuid::parse_str(body["request_id"].as_str().unwrap())
            .unwrap()
            .get_version_num(),
        7
    );
    assert!(!String::from_utf8_lossy(&response.body).contains(token));
    assert!(!String::from_utf8_lossy(&response.body).contains("synthetic-private"));
    body
}

#[tokio::test]
async fn expired_foreign_and_malformed_credentials_cannot_write_or_query_results() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config).await;
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 91 * 24 * 60 * 60;
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let grant = workspace.issue_pairing(owner, past).await.unwrap();
    let expired = workspace
        .pair(
            notryx_protocol::PairRequest {
                protocol: notryx_protocol::CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            past,
        )
        .await
        .unwrap();
    let operation = uuid::Uuid::now_v7().to_string();
    let receipt = workspace
        .save(SaveContent {
            epoch: expired.workspace_epoch,
            stream_id: expired.device_stream_id.parse().unwrap(),
            operation_seq: 1,
            operation_id: operation.parse().unwrap(),
            node_id: document.node_id,
            expected_content_rev: document.content_rev.clone(),
            content_hash: Sha256::digest(UPDATED)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: UPDATED.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, Outcome::Saved(_)));
    workspace.close().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let valid = headers(&expired, &document, &operation, 1, UPDATED);
    assert_error(
        &put(server.local_addr(), &document, &valid, UPDATED).await,
        401,
        "unauthorized",
        &expired.device_token,
    );
    assert_error(
        &get_operation(server.local_addr(), &expired, &operation).await,
        401,
        "unauthorized",
        &expired.device_token,
    );

    let fresh = pair_owner(&config, server.local_addr()).await;
    let valid = headers(&fresh, &document, &operation, 1, UPDATED);
    let (_foreign_directory, foreign_config) = initialized().await;
    let foreign = RunningServer::start(foreign_config).await.unwrap();
    assert_error(
        &put(foreign.local_addr(), &document, &valid, UPDATED).await,
        401,
        "unauthorized",
        &fresh.device_token,
    );
    assert_error(
        &get_operation(foreign.local_addr(), &fresh, &operation).await,
        401,
        "unauthorized",
        &fresh.device_token,
    );
    foreign.shutdown().await.unwrap();

    let mut duplicate = valid.clone();
    duplicate.push(valid[0].clone());
    let mut malformed = valid.clone();
    replace(
        &mut malformed,
        "Authorization",
        "Bearer synthetic-invalid-token",
    );
    let anonymous: Vec<_> = valid
        .iter()
        .filter(|(name, _)| name != "Authorization")
        .cloned()
        .collect();
    for invalid in [duplicate, malformed, anonymous] {
        assert_error(
            &put(server.local_addr(), &document, &invalid, UPDATED).await,
            401,
            "unauthorized",
            &fresh.device_token,
        );
        let authorization: Vec<_> = invalid
            .iter()
            .filter(|(name, _)| name == "Authorization")
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let response = request(
            server.local_addr(),
            "GET",
            &format!("/v1/operations/{operation}"),
            &authorization,
            &[],
        )
        .await;
        assert_error(&response, 401, "unauthorized", &fresh.device_token);
    }
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        UPDATED
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_finalization_keeps_pending_receipts_and_exposes_recovered_outcomes() {
    for (bytes_after_failure, expected_status, retry_status) in [
        (UPDATED, "committed", 200),
        (ORIGINAL, "not_applied", 409),
        (b"external recovery conflict".as_slice(), "pending", 503),
    ] {
        let (_directory, config) = initialized().await;
        let document = adopt(&config).await;
        let server = RunningServer::start(config.clone()).await.unwrap();
        let paired = pair_owner(&config, server.local_addr()).await;
        let operation = uuid::Uuid::now_v7().to_string();
        let valid = headers(&paired, &document, &operation, 1, UPDATED);
        let mut fault = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_write_fixture_finalize BEFORE UPDATE OF content_rev ON nodes BEGIN SELECT RAISE(ABORT, 'synthetic finalization failure'); END;").execute(&mut fault).await.unwrap();
        let failed = put(server.local_addr(), &document, &valid, UPDATED).await;
        assert_error(&failed, 503, "storage_unavailable", &paired.device_token);
        sqlx::query("DROP TRIGGER fail_write_fixture_finalize")
            .execute(&mut fault)
            .await
            .unwrap();
        fault.close().await.unwrap();
        let found = get_operation(server.local_addr(), &paired, &operation).await;
        assert_eq!(found.status, 200);
        let pending = value(&found);
        assert_eq!(pending["status"], "pending");
        let replay = put(server.local_addr(), &document, &valid, UPDATED).await;
        let error = assert_error(&replay, 503, "recovery_pending", &paired.device_token);
        assert_eq!(error["operation"], pending);
        let next_id = uuid::Uuid::now_v7().to_string();
        let next = headers(&paired, &document, &next_id, 2, b"must wait");
        assert_error(
            &put(server.local_addr(), &document, &next, b"must wait").await,
            503,
            "recovery_pending",
            &paired.device_token,
        );
        assert_eq!(
            get_operation(server.local_addr(), &paired, &next_id)
                .await
                .status,
            404
        );
        server.shutdown().await.unwrap();

        // These are explicit before/proposed/divergent recovery fixtures, not power-loss simulations.
        std::fs::write(
            config.workspace.notes_root.join("Document.md"),
            bytes_after_failure,
        )
        .unwrap();
        let server = RunningServer::start(config.clone()).await.unwrap();
        let found = get_operation(server.local_addr(), &paired, &operation).await;
        assert_eq!(found.status, 200);
        let recovered = value(&found);
        assert_eq!(recovered["status"], expected_status);
        let replay = put(server.local_addr(), &document, &valid, UPDATED).await;
        assert_eq!(replay.status, retry_status);
        if retry_status == 200 {
            assert_eq!(value(&replay), recovered);
        } else {
            let code = if retry_status == 409 {
                "operation_not_applied"
            } else {
                "recovery_pending"
            };
            assert_eq!(
                assert_error(&replay, retry_status, code, &paired.device_token)["operation"],
                recovered
            );
        }
        let readiness = request(server.local_addr(), "GET", "/health/ready", &[], &[]).await;
        assert_eq!(
            readiness.status,
            if expected_status == "pending" {
                503
            } else {
                200
            }
        );
        assert_eq!(
            std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
            bytes_after_failure
        );
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn authenticated_put_preserves_bytes_and_historical_receipts_across_restart() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let operation = uuid::Uuid::now_v7().to_string();
    let original_headers = headers(&paired, &document, &operation, 1, UPDATED);
    let first = put(server.local_addr(), &document, &original_headers, UPDATED).await;
    assert_eq!(first.status, 200);
    let receipt = value(&first);
    assert_eq!(receipt["status"], "committed");
    assert_eq!(receipt["operation_id"], operation);
    assert_eq!(receipt["operation_seq"], "1");
    assert_eq!(
        receipt["workspace_epoch"],
        paired.workspace_epoch.to_string()
    );
    assert_eq!(receipt["node_id"], document.node_id.to_string());
    assert_eq!(
        receipt["content_hash"],
        format!(
            "sha256:{}",
            Sha256::digest(UPDATED)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    );
    assert_eq!(receipt["meta_rev"], document.meta_rev.as_str());
    for field in ["content_etag", "meta_etag", "node_etag"] {
        notryx_protocol::StrongEtag::parse(receipt[field].as_str().unwrap()).unwrap();
    }
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        UPDATED
    );
    let read = request(
        server.local_addr(),
        "GET",
        &format!("/v1/nodes/{}/content", document.node_id),
        &[("Authorization", &format!("Bearer {}", paired.device_token))],
        &[],
    )
    .await;
    assert_eq!(read.status, 200);
    assert!(read.body == UPDATED);
    assert_eq!(read.header("etag"), receipt["content_etag"].as_str());

    let replay = put(server.local_addr(), &document, &original_headers, UPDATED).await;
    assert_eq!(replay.status, 200);
    assert_eq!(value(&replay), receipt);
    let later_operation = uuid::Uuid::now_v7().to_string();
    let mut later_headers = headers(&paired, &document, &later_operation, 2, b"later body");
    replace(
        &mut later_headers,
        "If-Match",
        receipt["content_etag"].as_str().unwrap(),
    );
    let later = put(
        server.local_addr(),
        &document,
        &later_headers,
        b"later body",
    )
    .await;
    assert_eq!(later.status, 200);
    assert_ne!(value(&later)["node_etag"], receipt["node_etag"]);
    let old_hash = receipt["content_hash"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    std::fs::remove_file(
        config
            .workspace
            .state_root
            .join("objects/sha256")
            .join(old_hash),
    )
    .unwrap();
    server.shutdown().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let lookup = get_operation(server.local_addr(), &paired, &operation).await;
    assert_eq!(lookup.status, 200);
    assert_eq!(value(&lookup), receipt);
    let replay = put(server.local_addr(), &document, &original_headers, UPDATED).await;
    assert_eq!(replay.status, 200);
    assert_eq!(value(&replay), receipt);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        b"later body"
    );
    let current = value(&later);
    let current_hash = current["content_hash"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    std::fs::remove_file(
        config
            .workspace
            .state_root
            .join("objects/sha256")
            .join(current_hash),
    )
    .unwrap();
    let content = request(
        server.local_addr(),
        "GET",
        &format!("/v1/nodes/{}/content", document.node_id),
        &[("Authorization", &format!("Bearer {}", paired.device_token))],
        &[],
    )
    .await;
    assert_error(&content, 503, "storage_unavailable", &paired.device_token);
    assert_eq!(
        request(server.local_addr(), "GET", "/health/ready", &[], &[])
            .await
            .status,
        503
    );
    let lookup = get_operation(server.local_addr(), &paired, &operation).await;
    assert_eq!(lookup.status, 200);
    assert_eq!(value(&lookup), receipt);
    let replay = put(server.local_addr(), &document, &original_headers, UPDATED).await;
    assert_eq!(replay.status, 200);
    assert_eq!(value(&replay), receipt);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_devices_get_one_commit_and_one_replayable_precondition_failure() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let first = pair_owner(&config, server.local_addr()).await;
    let second = pair_owner(&config, server.local_addr()).await;
    let first_id = uuid::Uuid::now_v7().to_string();
    let second_id = uuid::Uuid::now_v7().to_string();
    let first_headers = headers(&first, &document, &first_id, 1, b"first contender");
    let second_headers = headers(&second, &document, &second_id, 1, b"second contender");
    let (a, b) = tokio::join!(
        put(
            server.local_addr(),
            &document,
            &first_headers,
            b"first contender"
        ),
        put(
            server.local_addr(),
            &document,
            &second_headers,
            b"second contender"
        ),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [200, 412]);
    let (failed, owner, id, retry_headers, bytes) = if a.status == 412 {
        (
            &a,
            &first,
            &first_id,
            &first_headers,
            b"first contender".as_slice(),
        )
    } else {
        (
            &b,
            &second,
            &second_id,
            &second_headers,
            b"second contender".as_slice(),
        )
    };
    let failure = assert_error(
        failed,
        412,
        "content_revision_mismatch",
        &owner.device_token,
    );
    assert_eq!(failure["operation"]["status"], "precondition_failed");
    let (winner, saved) = if a.status == 200 {
        (&first, value(&a))
    } else {
        (&second, value(&b))
    };
    let mut later = headers(
        winner,
        &document,
        &uuid::Uuid::now_v7().to_string(),
        2,
        b"later revision",
    );
    replace(
        &mut later,
        "If-Match",
        saved["content_etag"].as_str().unwrap(),
    );
    assert_eq!(
        put(server.local_addr(), &document, &later, b"later revision")
            .await
            .status,
        200
    );
    let retry = put(server.local_addr(), &document, retry_headers, bytes).await;
    let repeated = assert_error(
        &retry,
        412,
        "content_revision_mismatch",
        &owner.device_token,
    );
    assert_eq!(repeated["operation"], failure["operation"]);
    let lookup = get_operation(server.local_addr(), owner, id).await;
    assert_eq!(lookup.status, 200);
    assert_eq!(value(&lookup), failure["operation"]);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn writing_requires_current_role_and_device_stream_but_existing_results_survive_demotion() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let editor = pair_role(&config, server.local_addr(), WorkspaceRole::Editor).await;
    let reader = pair_role(&config, server.local_addr(), WorkspaceRole::Reader).await;
    let operation = uuid::Uuid::now_v7().to_string();
    let valid = headers(&editor, &document, &operation, 1, UPDATED);
    let reader_headers = headers(
        &reader,
        &document,
        &uuid::Uuid::now_v7().to_string(),
        1,
        UPDATED,
    );
    let denied = put(server.local_addr(), &document, &reader_headers, UPDATED).await;
    assert_error(&denied, 403, "forbidden", &reader.device_token);
    let mut stolen_stream = valid.clone();
    replace(
        &mut stolen_stream,
        "X-Device-Stream",
        &reader.device_stream_id,
    );
    let denied = put(server.local_addr(), &document, &stolen_stream, UPDATED).await;
    assert_error(&denied, 403, "forbidden", &editor.device_token);
    let saved = put(server.local_addr(), &document, &valid, UPDATED).await;
    assert_eq!(saved.status, 200);
    let saved = value(&saved);
    let hidden = get_operation(server.local_addr(), &reader, &operation).await;
    assert_error(&hidden, 404, "not_found", &reader.device_token);
    assert!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::SetRole {
                principal_id: editor.principal_id.clone(),
                role: WorkspaceRole::Reader,
            }
        )
        .await
        .is_ok()
    );
    let replay = put(server.local_addr(), &document, &valid, UPDATED).await;
    assert_eq!(replay.status, 200);
    assert_eq!(value(&replay), saved);
    let lookup = get_operation(server.local_addr(), &editor, &operation).await;
    assert_eq!(lookup.status, 200);
    assert_eq!(value(&lookup), saved);
    let mut next = headers(
        &editor,
        &document,
        &uuid::Uuid::now_v7().to_string(),
        2,
        b"forbidden next",
    );
    replace(
        &mut next,
        "If-Match",
        saved["content_etag"].as_str().unwrap(),
    );
    let denied = put(server.local_addr(), &document, &next, b"forbidden next").await;
    assert_error(&denied, 403, "forbidden", &editor.device_token);
    assert!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::RevokeDevice {
                device_id: editor.device_id.clone()
            }
        )
        .await
        .is_ok()
    );
    let denied = put(server.local_addr(), &document, &valid, UPDATED).await;
    assert_error(&denied, 401, "unauthorized", &editor.device_token);
    let denied = get_operation(server.local_addr(), &editor, &operation).await;
    assert_error(&denied, 401, "unauthorized", &editor.device_token);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_mutation_headers_do_not_consume_an_operation_or_change_source() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let operation = uuid::Uuid::now_v7().to_string();
    let valid = headers(&paired, &document, &operation, 1, UPDATED);
    for name in [
        "If-Match",
        "X-Workspace-Epoch",
        "X-Device-Stream",
        "X-Operation-Seq",
        "Idempotency-Key",
        "Content-Digest",
    ] {
        let missing: Vec<_> = valid
            .iter()
            .filter(|(key, _)| key != name)
            .cloned()
            .collect();
        let response = put(server.local_addr(), &document, &missing, UPDATED).await;
        assert_error(
            &response,
            if name == "If-Match" { 428 } else { 400 },
            "invalid_request",
            &paired.device_token,
        );
        let mut duplicate = valid.clone();
        duplicate.push(valid.iter().find(|(key, _)| key == name).unwrap().clone());
        let response = put(server.local_addr(), &document, &duplicate, UPDATED).await;
        assert_error(&response, 400, "invalid_request", &paired.device_token);
    }
    for (name, invalid) in [
        ("If-Match", "*"),
        ("If-Match", "W/\"revision\""),
        ("If-Match", "\"first\", \"second\""),
        ("If-Match", "\"\""),
        ("X-Operation-Seq", "0"),
        ("X-Operation-Seq", "+1"),
        ("X-Operation-Seq", "1.0"),
        ("X-Operation-Seq", "18446744073709551616"),
        ("X-Workspace-Epoch", "synthetic-private"),
        ("X-Device-Stream", "00000000-0000-4000-0000-000000000001"),
        ("Idempotency-Key", "00000000-0000-4000-8000-000000000001"),
    ] {
        let mut headers = valid.clone();
        replace(&mut headers, name, invalid);
        let response = put(server.local_addr(), &document, &headers, UPDATED).await;
        assert_error(&response, 400, "invalid_request", &paired.device_token);
    }
    for encoding in ["gzip", "identity, gzip"] {
        let mut headers = valid.clone();
        headers.push(("Content-Encoding".into(), encoding.into()));
        let response = put(server.local_addr(), &document, &headers, UPDATED).await;
        assert_error(&response, 400, "invalid_request", &paired.device_token);
    }
    let mut duplicate_encoding = valid.clone();
    duplicate_encoding.push(("Content-Encoding".into(), "identity".into()));
    duplicate_encoding.push(("Content-Encoding".into(), "identity".into()));
    assert_error(
        &put(server.local_addr(), &document, &duplicate_encoding, UPDATED).await,
        400,
        "invalid_request",
        &paired.device_token,
    );
    let mut long_condition = valid.clone();
    replace(
        &mut long_condition,
        "If-Match",
        &format!("\"{}\"", "x".repeat(257)),
    );
    assert_error(
        &put(server.local_addr(), &document, &long_condition, UPDATED).await,
        400,
        "invalid_request",
        &paired.device_token,
    );
    let mut bad_digest = valid.clone();
    replace(
        &mut bad_digest,
        "Content-Digest",
        "sha-256=:c3ludGhldGljLXByaXZhdGU=:",
    );
    let response = put(server.local_addr(), &document, &bad_digest, UPDATED).await;
    assert_error(&response, 400, "digest_mismatch", &paired.device_token);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        ORIGINAL
    );
    assert_eq!(
        get_operation(server.local_addr(), &paired, &operation)
            .await
            .status,
        404
    );
    let mut identity = valid.clone();
    identity.push(("Content-Encoding".into(), "identity".into()));
    let response = put(server.local_addr(), &document, &identity, UPDATED).await;
    assert_eq!(response.status, 200);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn mutation_identity_and_epoch_conflicts_never_replace_committed_content() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let operation = uuid::Uuid::now_v7().to_string();
    let valid = headers(&paired, &document, &operation, 1, UPDATED);
    let mut wrong_epoch = valid.clone();
    replace(
        &mut wrong_epoch,
        "X-Workspace-Epoch",
        &uuid::Uuid::new_v4().to_string(),
    );
    assert_error(
        &put(server.local_addr(), &document, &wrong_epoch, UPDATED).await,
        409,
        "epoch_mismatch",
        &paired.device_token,
    );
    let mut gap = valid.clone();
    replace(&mut gap, "X-Operation-Seq", "2");
    assert_error(
        &put(server.local_addr(), &document, &gap, UPDATED).await,
        409,
        "operation_gap",
        &paired.device_token,
    );
    assert_eq!(
        put(server.local_addr(), &document, &valid, UPDATED)
            .await
            .status,
        200
    );
    let changed = headers(&paired, &document, &operation, 1, b"different payload");
    assert_error(
        &put(
            server.local_addr(),
            &document,
            &changed,
            b"different payload",
        )
        .await,
        409,
        "idempotency_mismatch",
        &paired.device_token,
    );
    let reused_sequence = headers(
        &paired,
        &document,
        &uuid::Uuid::now_v7().to_string(),
        1,
        UPDATED,
    );
    assert_error(
        &put(server.local_addr(), &document, &reused_sequence, UPDATED).await,
        409,
        "idempotency_mismatch",
        &paired.device_token,
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        UPDATED
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn content_limit_allows_twenty_five_mib_without_raising_control_json_limit() {
    let (_directory, config) = initialized().await;
    let document = adopt(&config).await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let operation = uuid::Uuid::now_v7().to_string();
    let oversized = vec![b'x'; 25 * 1024 * 1024 + 1];
    let valid = headers(&paired, &document, &operation, 1, &oversized);
    let response = put(server.local_addr(), &document, &valid, &oversized).await;
    assert_error(&response, 413, "invalid_request", &paired.device_token);
    let bytes = &oversized[..25 * 1024 * 1024];
    let valid = headers(&paired, &document, &operation, 1, bytes);
    let headers: Vec<_> = valid
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let response = support::request_with_timeout(
        server.local_addr(),
        "PUT",
        &format!("/v1/nodes/{}/content", document.node_id),
        &headers,
        bytes,
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        bytes
    );
    let too_large_control = vec![b' '; 1024 * 1024 + 1];
    let response = post_json(server.local_addr(), &too_large_control).await;
    assert_error(&response, 413, "invalid_request", &paired.device_token);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_nodes_have_terminal_receipts_and_other_devices_cannot_read_them() {
    let (_directory, config) = initialized().await;
    let mut document = adopt(&config).await;
    document.node_id = uuid::Uuid::now_v7().to_string().parse().unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let paired = pair_owner(&config, server.local_addr()).await;
    let other_device = pair_owner(&config, server.local_addr()).await;
    let operation = uuid::Uuid::now_v7().to_string();
    let valid = headers(&paired, &document, &operation, 1, UPDATED);
    let response = put(server.local_addr(), &document, &valid, UPDATED).await;
    let error = assert_error(&response, 404, "not_found", &paired.device_token);
    assert_eq!(error["operation"]["status"], "not_found");
    let found = get_operation(server.local_addr(), &paired, &operation).await;
    assert_eq!(found.status, 200);
    assert_eq!(value(&found), error["operation"]);
    let absent = get_operation(server.local_addr(), &other_device, &operation).await;
    assert_error(&absent, 404, "not_found", &other_device.device_token);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        ORIGINAL
    );
    server.shutdown().await.unwrap();
}
