pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, CONTRACT, PairRequest, PairResponse, WorkspaceRole,
};
use notryx_server::{Document, Workspace, admin, config::Config, http::RunningServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::net::SocketAddr;
use support::{HttpResponse, initialized, pairing_request, post_json, request};

const ORIGINAL: &[u8] = b"\xef\xbb\xbf# Original\r\n";
const SAVED: &[u8] = b"# saved through HTTP\n";
const ACK: &str = "/v1/operation-stream/ack";
const ABANDON: &str = "/v1/operation-stream/abandon";

async fn setup() -> (tempfile::TempDir, Config, Document, RunningServer) {
    let (directory, config) = initialized().await;
    std::fs::write(config.workspace.notes_root.join("Document.md"), ORIGINAL).unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    (directory, config, document, server)
}

async fn pair(config: &Config, address: SocketAddr, role: WorkspaceRole) -> PairResponse {
    let request = if role == WorkspaceRole::Owner {
        pairing_request(config).await.0
    } else {
        let AdminResponse::Ok {
            result: AdminResult::Principal { principal_id },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::CreatePrincipal { role },
        )
        .await
        .unwrap()
        else {
            panic!("synthetic principal must be created")
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
            panic!("synthetic pairing must be issued")
        };
        PairRequest {
            protocol: CONTRACT.into(),
            workspace_id,
            pairing_secret,
        }
    };
    let response = post_json(address, &serde_json::to_vec(&request).unwrap()).await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).ok().unwrap()
}

fn ack(device: &PairResponse, through: u64) -> Value {
    json!({"workspace_epoch": device.workspace_epoch,
        "device_stream_id": device.device_stream_id, "ack_through": through.to_string()})
}

fn abandon(device: &PairResponse, operation: &str, sequence: u64) -> Value {
    json!({"workspace_epoch": device.workspace_epoch, "device_stream_id": device.device_stream_id,
        "operation_id": operation, "operation_seq": sequence.to_string()})
}

async fn post(
    address: SocketAddr,
    path: &str,
    device: &PairResponse,
    body: &Value,
) -> HttpResponse {
    request(
        address,
        "POST",
        path,
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
        ],
        &serde_json::to_vec(body).unwrap(),
    )
    .await
}

async fn get_operation(address: SocketAddr, device: &PairResponse, id: &str) -> HttpResponse {
    request(
        address,
        "GET",
        &format!("/v1/operations/{id}"),
        &[("Authorization", &format!("Bearer {}", device.device_token))],
        &[],
    )
    .await
}

async fn save(
    address: SocketAddr,
    device: &PairResponse,
    document: &Document,
    id: &str,
    sequence: u64,
    etag: &str,
    bytes: &[u8],
) -> HttpResponse {
    request(
        address,
        "PUT",
        &format!("/v1/nodes/{}/content", document.node_id),
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", &sequence.to_string()),
            ("Idempotency-Key", id),
            ("If-Match", etag),
            (
                "Content-Digest",
                &format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(bytes))),
            ),
        ],
        bytes,
    )
    .await
}

fn value(response: &HttpResponse) -> Value {
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    serde_json::from_slice(&response.body).unwrap()
}

fn error(response: &HttpResponse, status: u16, code: &str, device: &PairResponse) {
    assert_eq!(response.status, status);
    let body = value(response);
    assert_eq!(body["code"], code);
    assert!(!body["message"].as_str().unwrap().is_empty());
    assert_eq!(
        uuid::Uuid::parse_str(body["request_id"].as_str().unwrap())
            .unwrap()
            .get_version_num(),
        7
    );
    assert!(!String::from_utf8_lossy(&response.body).contains(&device.device_token));
    if status == 401 {
        assert_eq!(
            response.header("www-authenticate"),
            Some("Bearer realm=\"notryx\"")
        );
    }
}

#[tokio::test]
async fn reader_abandon_and_ack_survive_restart_without_changing_document_or_head() {
    let (_directory, config, document, server) = setup().await;
    let device = pair(&config, server.local_addr(), WorkspaceRole::Reader).await;
    let zero = post(server.local_addr(), ACK, &device, &ack(&device, 0)).await;
    assert_eq!(zero.status, 200);
    assert_eq!(value(&zero)["last_seq"], "0");
    assert_eq!(value(&zero)["retired_through"], "0");
    let id = uuid::Uuid::now_v7().to_string();
    let body = abandon(&device, &id, 1);
    let first = post(server.local_addr(), ABANDON, &device, &body).await;
    assert_eq!(first.status, 200);
    let receipt = value(&first);
    assert_eq!(receipt["status"], "abandoned");
    assert_eq!(receipt["operation_id"], id);
    assert_eq!(receipt["operation_seq"], "1");
    assert_eq!(
        value(&post(server.local_addr(), ABANDON, &device, &body).await),
        receipt
    );
    assert_eq!(
        value(&get_operation(server.local_addr(), &device, &id).await),
        receipt
    );
    let done = post(server.local_addr(), ACK, &device, &ack(&device, 1)).await;
    assert_eq!(done.status, 200);
    assert_eq!(value(&done)["retired_through"], "1");
    error(
        &get_operation(server.local_addr(), &device, &id).await,
        410,
        "operation_retired",
        &device,
    );
    error(
        &post(server.local_addr(), ABANDON, &device, &body).await,
        410,
        "operation_retired",
        &device,
    );
    error(
        &save(
            server.local_addr(),
            &device,
            &document,
            &id,
            1,
            &format!("\"{}\"", document.content_rev.as_str()),
            SAVED,
        )
        .await,
        410,
        "operation_retired",
        &device,
    );
    let next_id = uuid::Uuid::now_v7().to_string();
    assert_eq!(
        post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &next_id, 2)
        )
        .await
        .status,
        200
    );
    server.shutdown().await.unwrap();

    let server = RunningServer::start(config.clone()).await.unwrap();
    let stale = post(server.local_addr(), ACK, &device, &ack(&device, 0)).await;
    assert_eq!(stale.status, 200);
    assert_eq!(value(&stale)["last_seq"], "2");
    assert_eq!(value(&stale)["retired_through"], "1");
    error(
        &post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &id, 3),
        )
        .await,
        409,
        "idempotency_mismatch",
        &device,
    );
    let info = request(
        server.local_addr(),
        "GET",
        "/v1/server-info",
        &[("Authorization", &format!("Bearer {}", device.device_token))],
        &[],
    )
    .await;
    assert_eq!(info.status, 200);
    assert_eq!(
        value(&info)["head_seq"],
        document.last_change_seq.get().to_string()
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        ORIGINAL
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ack_retires_success_and_failure_while_abandon_keeps_their_original_results() {
    let (_directory, config, document, server) = setup().await;
    let device = pair(&config, server.local_addr(), WorkspaceRole::Owner).await;
    let other = pair(&config, server.local_addr(), WorkspaceRole::Owner).await;
    let id = uuid::Uuid::now_v7().to_string();
    let etag = format!("\"{}\"", document.content_rev.as_str());
    let saved = save(
        server.local_addr(),
        &device,
        &document,
        &id,
        1,
        &etag,
        SAVED,
    )
    .await;
    assert_eq!(saved.status, 200);
    let saved = value(&saved);
    let unchanged = post(
        server.local_addr(),
        ABANDON,
        &device,
        &abandon(&device, &id, 1),
    )
    .await;
    assert_eq!(unchanged.status, 200);
    assert_eq!(value(&unchanged), saved);
    let rejected_id = uuid::Uuid::now_v7().to_string();
    let rejected = save(
        server.local_addr(),
        &device,
        &document,
        &rejected_id,
        2,
        &etag,
        b"stale write",
    )
    .await;
    error(&rejected, 412, "content_revision_mismatch", &device);
    let original_failure = value(&rejected)["operation"].clone();
    let unchanged = post(
        server.local_addr(),
        ABANDON,
        &device,
        &abandon(&device, &rejected_id, 2),
    )
    .await;
    error(&unchanged, 412, "content_revision_mismatch", &device);
    assert_eq!(value(&unchanged)["operation"], original_failure);
    let done = post(server.local_addr(), ACK, &device, &ack(&device, 2)).await;
    assert_eq!(done.status, 200);
    assert_eq!(value(&done)["retired_through"], "2");
    for retired in [&id, &rejected_id] {
        error(
            &get_operation(server.local_addr(), &device, retired).await,
            410,
            "operation_retired",
            &device,
        );
        error(
            &get_operation(server.local_addr(), &other, retired).await,
            404,
            "not_found",
            &other,
        );
    }
    error(
        &save(
            server.local_addr(),
            &device,
            &document,
            &id,
            1,
            &etag,
            SAVED,
        )
        .await,
        410,
        "operation_retired",
        &device,
    );
    let next_id = uuid::Uuid::now_v7().to_string();
    let next = save(
        server.local_addr(),
        &device,
        &document,
        &next_id,
        3,
        saved["content_etag"].as_str().unwrap(),
        b"next saved body",
    )
    .await;
    assert_eq!(next.status, 200);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        b"next saved body"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_demotion_preserves_own_stream_management_but_revocation_blocks_it() {
    let (_directory, config, document, server) = setup().await;
    let device = pair(&config, server.local_addr(), WorkspaceRole::Editor).await;
    let id = uuid::Uuid::now_v7().to_string();
    assert_eq!(
        save(
            server.local_addr(),
            &device,
            &document,
            &id,
            1,
            &format!("\"{}\"", document.content_rev.as_str()),
            SAVED
        )
        .await
        .status,
        200
    );
    assert!(matches!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::SetRole {
                principal_id: device.principal_id.clone(),
                role: WorkspaceRole::Reader,
            }
        )
        .await
        .unwrap(),
        AdminResponse::Ok { .. }
    ));
    assert_eq!(
        post(server.local_addr(), ACK, &device, &ack(&device, 1))
            .await
            .status,
        200
    );
    let next_id = uuid::Uuid::now_v7().to_string();
    assert_eq!(
        post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &next_id, 2)
        )
        .await
        .status,
        200
    );
    assert!(matches!(
        admin::request(
            &config.server.admin_socket,
            &AdminRequest::RevokeDevice {
                device_id: device.device_id.clone(),
            }
        )
        .await
        .unwrap(),
        AdminResponse::Ok { .. }
    ));
    error(
        &post(server.local_addr(), ACK, &device, &ack(&device, 2)).await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &uuid::Uuid::now_v7().to_string(), 3),
        )
        .await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &get_operation(server.local_addr(), &device, &next_id).await,
        401,
        "unauthorized",
        &device,
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn stream_endpoints_reject_expired_and_foreign_workspace_credentials() {
    let (_directory, config) = initialized().await;
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
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            past,
        )
        .await
        .unwrap();
    workspace.close().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    for (path, body) in [
        (ACK, ack(&expired, 0)),
        (ABANDON, abandon(&expired, &id, 1)),
    ] {
        error(
            &post(server.local_addr(), path, &expired, &body).await,
            401,
            "unauthorized",
            &expired,
        );
    }
    let fresh = pair(&config, server.local_addr(), WorkspaceRole::Owner).await;
    let (_other_directory, other_config) = initialized().await;
    let other = RunningServer::start(other_config).await.unwrap();
    for (path, body) in [(ACK, ack(&fresh, 0)), (ABANDON, abandon(&fresh, &id, 1))] {
        error(
            &post(other.local_addr(), path, &fresh, &body).await,
            401,
            "unauthorized",
            &fresh,
        );
    }
    other.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn stream_inputs_and_binding_failures_do_not_consume_or_retire_sequences() {
    let (_directory, config, document, server) = setup().await;
    let device = pair(&config, server.local_addr(), WorkspaceRole::Reader).await;
    let other = pair(&config, server.local_addr(), WorkspaceRole::Reader).await;
    let id = uuid::Uuid::now_v7().to_string();
    for (path, valid) in [(ACK, ack(&device, 0)), (ABANDON, abandon(&device, &id, 1))] {
        let bytes = serde_json::to_vec(&valid).unwrap();
        let token = format!("Bearer {}", device.device_token);
        for headers in [
            vec![("Content-Type", "application/json")],
            vec![
                ("Content-Type", "application/json"),
                ("Authorization", "Bearer synthetic-invalid-token"),
            ],
            vec![
                ("Content-Type", "application/json"),
                ("Authorization", token.as_str()),
                ("Authorization", token.as_str()),
            ],
        ] {
            error(
                &request(server.local_addr(), "POST", path, &headers, &bytes).await,
                401,
                "unauthorized",
                &device,
            );
        }
        let mut foreign = valid.clone();
        foreign["device_stream_id"] = json!(other.device_stream_id);
        error(
            &post(server.local_addr(), path, &device, &foreign).await,
            403,
            "forbidden",
            &device,
        );
        let mut stale = valid.clone();
        stale["workspace_epoch"] = json!(uuid::Uuid::new_v4());
        error(
            &post(server.local_addr(), path, &device, &stale).await,
            409,
            "epoch_mismatch",
            &device,
        );
        let mut extra = valid.clone();
        extra["unexpected"] = json!(true);
        error(
            &post(server.local_addr(), path, &device, &extra).await,
            400,
            "invalid_request",
            &device,
        );
        for field in ["workspace_epoch", "device_stream_id"] {
            let mut invalid = valid.clone();
            invalid[field] = json!(uuid::Uuid::now_v7());
            error(
                &post(server.local_addr(), path, &device, &invalid).await,
                400,
                "invalid_request",
                &device,
            );
        }
        let headers = [
            ("Authorization", token.as_str()),
            ("Content-Type", "application/json"),
        ];
        error(
            &request(server.local_addr(), "POST", path, &headers, b"{").await,
            400,
            "invalid_request",
            &device,
        );
        error(
            &request(
                server.local_addr(),
                "POST",
                path,
                &headers,
                &vec![b' '; 1024 * 1024 + 1],
            )
            .await,
            413,
            "invalid_request",
            &device,
        );
    }
    error(
        &post(server.local_addr(), ACK, &device, &ack(&device, 1)).await,
        409,
        "invalid_ack",
        &device,
    );
    error(
        &post(server.local_addr(), ACK, &device, &ack(&device, u64::MAX)).await,
        409,
        "invalid_ack",
        &device,
    );
    error(
        &post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &id, 2),
        )
        .await,
        409,
        "operation_gap",
        &device,
    );
    let mut zero = abandon(&device, &id, 0);
    error(
        &post(server.local_addr(), ABANDON, &device, &zero).await,
        400,
        "invalid_request",
        &device,
    );
    zero["operation_seq"] = json!("18446744073709551616");
    error(
        &post(server.local_addr(), ABANDON, &device, &zero).await,
        400,
        "invalid_request",
        &device,
    );
    let status = post(server.local_addr(), ACK, &device, &ack(&device, 0)).await;
    assert_eq!(status.status, 200);
    assert_eq!(value(&status)["last_seq"], "0");
    assert_eq!(value(&status)["retired_through"], "0");
    assert_eq!(
        post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &id, 1)
        )
        .await
        .status,
        200
    );
    let conflict = uuid::Uuid::now_v7().to_string();
    error(
        &post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &conflict, 1),
        )
        .await,
        409,
        "idempotency_mismatch",
        &device,
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        ORIGINAL
    );
    let result = request(
        server.local_addr(),
        "GET",
        "/v1/server-info",
        &[("Authorization", &format!("Bearer {}", device.device_token))],
        &[],
    )
    .await;
    assert_eq!(
        value(&result)["head_seq"],
        document.last_change_seq.get().to_string()
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_work_cannot_be_acknowledged_or_abandoned_but_terminal_prefix_can_be_retired() {
    let (_directory, config, document, server) = setup().await;
    let device = pair(&config, server.local_addr(), WorkspaceRole::Owner).await;
    let first_id = uuid::Uuid::now_v7().to_string();
    let first = save(
        server.local_addr(),
        &device,
        &document,
        &first_id,
        1,
        &format!("\"{}\"", document.content_rev.as_str()),
        SAVED,
    )
    .await;
    assert_eq!(first.status, 200);
    let first = value(&first);
    let mut fault = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_stream_fixture_finalize BEFORE UPDATE OF content_rev ON nodes BEGIN SELECT RAISE(ABORT, 'synthetic finalization failure'); END;").execute(&mut fault).await.unwrap();
    let pending_id = uuid::Uuid::now_v7().to_string();
    let proposed = b"pending proposed bytes";
    let failed = save(
        server.local_addr(),
        &device,
        &document,
        &pending_id,
        2,
        first["content_etag"].as_str().unwrap(),
        proposed,
    )
    .await;
    error(&failed, 503, "storage_unavailable", &device);
    sqlx::query("DROP TRIGGER fail_stream_fixture_finalize")
        .execute(&mut fault)
        .await
        .unwrap();
    fault.close().await.unwrap();
    let pending = get_operation(server.local_addr(), &device, &pending_id).await;
    assert_eq!(pending.status, 200);
    let pending = value(&pending);
    assert_eq!(pending["status"], "pending");
    error(
        &post(server.local_addr(), ACK, &device, &ack(&device, 2)).await,
        409,
        "operation_pending",
        &device,
    );
    error(
        &post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &pending_id, 2),
        )
        .await,
        409,
        "operation_pending",
        &device,
    );
    let old = post(
        server.local_addr(),
        ABANDON,
        &device,
        &abandon(&device, &first_id, 1),
    )
    .await;
    assert_eq!(old.status, 200);
    assert_eq!(value(&old), first);
    let blocked_id = uuid::Uuid::now_v7().to_string();
    error(
        &post(
            server.local_addr(),
            ABANDON,
            &device,
            &abandon(&device, &blocked_id, 3),
        )
        .await,
        503,
        "recovery_pending",
        &device,
    );
    error(
        &get_operation(server.local_addr(), &device, &blocked_id).await,
        404,
        "not_found",
        &device,
    );
    let partial = post(server.local_addr(), ACK, &device, &ack(&device, 1)).await;
    assert_eq!(partial.status, 200);
    assert_eq!(value(&partial)["last_seq"], "2");
    assert_eq!(value(&partial)["retired_through"], "1");
    error(
        &get_operation(server.local_addr(), &device, &first_id).await,
        410,
        "operation_retired",
        &device,
    );
    assert_eq!(
        value(&get_operation(server.local_addr(), &device, &pending_id).await),
        pending
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        proposed
    );
    server.shutdown().await.unwrap();
    let server = RunningServer::start(config).await.unwrap();
    let recovered = get_operation(server.local_addr(), &device, &pending_id).await;
    assert_eq!(recovered.status, 200);
    assert_eq!(value(&recovered)["status"], "committed");
    assert_eq!(
        post(server.local_addr(), ACK, &device, &ack(&device, 2))
            .await
            .status,
        200
    );
    error(
        &get_operation(server.local_addr(), &device, &pending_id).await,
        410,
        "operation_retired",
        &device,
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_ack_transaction_never_reports_retirement_or_loses_the_receipt() {
    let (_directory, config, document, server) = setup().await;
    let device = pair(&config, server.local_addr(), WorkspaceRole::Owner).await;
    let id = uuid::Uuid::now_v7().to_string();
    let saved = save(
        server.local_addr(),
        &device,
        &document,
        &id,
        1,
        &format!("\"{}\"", document.content_rev.as_str()),
        SAVED,
    )
    .await;
    assert_eq!(saved.status, 200);
    let saved = value(&saved);
    let mut fault = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_ack_fixture_delete BEFORE DELETE ON operation_receipts BEGIN SELECT RAISE(ABORT, 'synthetic retirement failure'); END;").execute(&mut fault).await.unwrap();
    error(
        &post(server.local_addr(), ACK, &device, &ack(&device, 1)).await,
        503,
        "storage_unavailable",
        &device,
    );
    sqlx::query("DROP TRIGGER fail_ack_fixture_delete")
        .execute(&mut fault)
        .await
        .unwrap();
    fault.close().await.unwrap();
    let unchanged = get_operation(server.local_addr(), &device, &id).await;
    assert_eq!(unchanged.status, 200);
    assert_eq!(value(&unchanged), saved);
    let watermark = post(server.local_addr(), ACK, &device, &ack(&device, 0)).await;
    assert_eq!(watermark.status, 200);
    assert_eq!(value(&watermark)["retired_through"], "0");
    assert_eq!(
        post(server.local_addr(), ACK, &device, &ack(&device, 1))
            .await
            .status,
        200
    );
    error(
        &get_operation(server.local_addr(), &device, &id).await,
        410,
        "operation_retired",
        &device,
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        SAVED
    );
    server.shutdown().await.unwrap();
}
