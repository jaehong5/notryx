pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, CONTRACT, PairRequest, PairResponse, WorkspaceRole,
};
use notryx_server::{admin, config::Config, http::RunningServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::net::SocketAddr;
use support::{HttpResponse, initialized, pairing_request, post_json, request};

async fn setup() -> (
    tempfile::TempDir,
    Config,
    RunningServer,
    PairResponse,
    String,
) {
    let (directory, config) = initialized().await;
    let server = RunningServer::start(config.clone()).await.unwrap();
    let device = pair(&config, server.local_addr(), WorkspaceRole::Owner).await;
    let info = get(server.local_addr(), &device, "/v1/server-info").await;
    let root = value(&info)["root_id"].as_str().unwrap().to_owned();
    (directory, config, server, device, root)
}

async fn pair(config: &Config, address: SocketAddr, role: WorkspaceRole) -> PairResponse {
    let body = if role == WorkspaceRole::Owner {
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
    let response = post_json(address, &serde_json::to_vec(&body).unwrap()).await;
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).ok().unwrap()
}

fn id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn create_body(parent: &str, name: &str, kind: &str) -> Value {
    json!({"node_id":id(), "parent_id":parent, "name":name, "kind":kind})
}

async fn create(
    address: SocketAddr,
    device: &PairResponse,
    operation: &str,
    sequence: u64,
    body: &Value,
) -> HttpResponse {
    create_raw(
        address,
        device,
        operation,
        sequence,
        &serde_json::to_vec(body).unwrap(),
        &[],
    )
    .await
}

async fn create_raw(
    address: SocketAddr,
    device: &PairResponse,
    operation: &str,
    sequence: u64,
    body: &[u8],
    extra: &[(&str, &str)],
) -> HttpResponse {
    let authorization = format!("Bearer {}", device.device_token);
    let epoch = device.workspace_epoch.to_string();
    let sequence = sequence.to_string();
    let mut headers = vec![
        ("Authorization", authorization.as_str()),
        ("Content-Type", "application/json"),
        ("X-Workspace-Epoch", epoch.as_str()),
        ("X-Device-Stream", device.device_stream_id.as_str()),
        ("X-Operation-Seq", sequence.as_str()),
        ("Idempotency-Key", operation),
    ];
    headers.extend_from_slice(extra);
    request(address, "POST", "/v1/nodes", &headers, body).await
}

async fn get(address: SocketAddr, device: &PairResponse, path: &str) -> HttpResponse {
    request(
        address,
        "GET",
        path,
        &[("Authorization", &format!("Bearer {}", device.device_token))],
        &[],
    )
    .await
}

async fn ack(address: SocketAddr, device: &PairResponse, through: u64) -> HttpResponse {
    request(
        address,
        "POST",
        "/v1/operation-stream/ack",
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
        ],
        &serde_json::to_vec(&json!({"workspace_epoch":device.workspace_epoch,
        "device_stream_id":device.device_stream_id,"ack_through":through.to_string()}))
        .unwrap(),
    )
    .await
}

async fn put(
    address: SocketAddr,
    device: &PairResponse,
    node: &str,
    sequence: u64,
    etag: &str,
    bytes: &[u8],
) -> HttpResponse {
    request(
        address,
        "PUT",
        &format!("/v1/nodes/{node}/content"),
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", &sequence.to_string()),
            ("Idempotency-Key", &id()),
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

fn error(response: &HttpResponse, status: u16, code: &str, device: &PairResponse) -> Value {
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
    body
}

async fn database(config: &Config) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn nested_creation_preserves_raw_content_and_original_receipt_after_restart() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let root_before = value(&get(address, &device, &format!("/v1/nodes/{root}/metadata")).await);
    let folder = create_body(&root, "문서", "folder");
    let folder_id = folder["node_id"].as_str().unwrap();
    let folder_receipt = create(address, &device, &id(), 1, &folder).await;
    assert_eq!(folder_receipt.status, 200);
    let folder_value = value(&folder_receipt);
    assert_eq!(folder_value["status"], "created");
    assert_eq!(folder_value["node"]["kind"], "folder");
    for field in ["content_rev", "content_etag", "content_hash", "size"] {
        assert!(folder_value["node"][field].is_null(), "{field}");
    }
    let root_after = value(&get(address, &device, &format!("/v1/nodes/{root}/metadata")).await);
    assert_eq!(root_before, root_after);
    let file = create_body(folder_id, "Note.md", "file");
    let node = file["node_id"].as_str().unwrap();
    let operation = id();
    let response = create(address, &device, &operation, 2, &file).await;
    assert_eq!(response.status, 200);
    let recorded = value(&response);
    assert_eq!(recorded["node"]["size"], "0");
    assert_eq!(recorded["node"]["parent_id"], folder_id);
    let etag = recorded["node"]["content_etag"].as_str().unwrap();
    let empty = get(address, &device, &format!("/v1/nodes/{node}/content")).await;
    assert_eq!(empty.status, 200);
    assert!(empty.body.is_empty());
    let bytes = b"\xef\xbb\xbf# raw\r\n\xff";
    assert_eq!(
        put(address, &device, node, 3, etag, bytes).await.status,
        200
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("문서/Note.md")).unwrap(),
        bytes
    );
    assert_eq!(
        value(&create(address, &device, &operation, 2, &file).await),
        recorded
    );
    let pretty = serde_json::to_vec_pretty(&file).unwrap();
    assert_eq!(
        value(&create_raw(address, &device, &operation, 2, &pretty, &[]).await),
        recorded
    );
    server.shutdown().await.unwrap();
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    let address = restarted.local_addr();
    assert_eq!(
        get(address, &device, &format!("/v1/nodes/{node}/content"))
            .await
            .body,
        bytes
    );
    assert_eq!(
        value(&get(address, &device, &format!("/v1/operations/{operation}")).await),
        recorded
    );
    assert_eq!(
        value(&create(address, &device, &operation, 2, &file).await),
        recorded
    );
    let metadata = value(&get(address, &device, &format!("/v1/nodes/{node}/metadata")).await);
    assert_eq!(metadata["parent_id"], folder_id);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn creation_conflicts_are_terminal_and_never_overwrite_tracked_or_external_entries() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let initial = create_body(&root, "Note.md", "file");
    assert_eq!(
        create(address, &device, &id(), 1, &initial).await.status,
        200
    );
    let mut attempts = Vec::new();
    attempts.push((
        create_body(&root, "NOTE.md", "folder"),
        "destination_exists",
        409,
    ));
    let mut reused_id = create_body(&root, "new.md", "file");
    reused_id["node_id"] = initial["node_id"].clone();
    attempts.push((reused_id, "node_id_conflict", 409));
    attempts.push((
        create_body(initial["node_id"].as_str().unwrap(), "child.md", "file"),
        "parent_not_folder",
        409,
    ));
    attempts.push((create_body(&id(), "orphan.md", "file"), "not_found", 404));
    std::fs::write(
        config.workspace.notes_root.join("External.md"),
        b"external evidence",
    )
    .unwrap();
    attempts.push((
        create_body(&root, "external.MD", "file"),
        "destination_exists",
        409,
    ));
    for (index, (body, code, status)) in attempts.iter().enumerate() {
        let operation = id();
        let sequence = index as u64 + 2;
        let response = create(address, &device, &operation, sequence, body).await;
        let first = error(&response, *status, code, &device);
        assert_eq!(first["operation"]["status"], *code);
        let retry = create(address, &device, &operation, sequence, body).await;
        assert_eq!(
            error(&retry, *status, code, &device)["operation"],
            first["operation"]
        );
        assert_eq!(
            value(&get(address, &device, &format!("/v1/operations/{operation}")).await),
            first["operation"]
        );
        let mut different = body.clone();
        different["name"] = json!("different.md");
        error(
            &create(address, &device, &operation, sequence, &different).await,
            409,
            "idempotency_mismatch",
            &device,
        );
    }
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("External.md")).unwrap(),
        b"external evidence"
    );
    assert!(!config.workspace.notes_root.join("external.MD").exists());
    assert!(
        std::fs::read(config.workspace.notes_root.join("Note.md"))
            .unwrap()
            .is_empty()
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn current_role_and_revocation_apply_to_new_creation_but_existing_results_remain_readable() {
    let (_directory, config, server, _owner, root) = setup().await;
    let address = server.local_addr();
    let editor = pair(&config, address, WorkspaceRole::Editor).await;
    let reader = pair(&config, address, WorkspaceRole::Reader).await;
    let body = create_body(&root, "owned.md", "file");
    error(
        &create(address, &reader, &id(), 1, &body).await,
        403,
        "forbidden",
        &reader,
    );
    let operation = id();
    let result = create(address, &editor, &operation, 1, &body).await;
    assert_eq!(result.status, 200);
    let recorded = value(&result);
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::SetRole {
            principal_id: editor.principal_id.clone(),
            role: WorkspaceRole::Reader,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        value(&create(address, &editor, &operation, 1, &body).await),
        recorded
    );
    assert_eq!(
        value(&get(address, &editor, &format!("/v1/operations/{operation}")).await),
        recorded
    );
    error(
        &create(
            address,
            &editor,
            &id(),
            2,
            &create_body(&root, "new.md", "file"),
        )
        .await,
        403,
        "forbidden",
        &editor,
    );
    error(
        &get(address, &reader, &format!("/v1/operations/{operation}")).await,
        404,
        "not_found",
        &reader,
    );
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: editor.device_id.clone(),
        },
    )
    .await
    .unwrap();
    error(
        &create(address, &editor, &operation, 1, &body).await,
        401,
        "unauthorized",
        &editor,
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn creation_rejects_malformed_inputs_and_binding_without_consuming_the_sequence() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let body = create_body(&root, "valid.md", "file");
    let encoded = serde_json::to_vec(&body).unwrap();
    error(
        &request(
            address,
            "POST",
            "/v1/nodes",
            &[("Content-Type", "application/json")],
            &encoded,
        )
        .await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &request(
            address,
            "POST",
            "/v1/nodes",
            &[
                ("Authorization", &format!("Bearer {}", device.device_token)),
                ("Content-Type", "application/json"),
            ],
            &encoded,
        )
        .await,
        400,
        "invalid_request",
        &device,
    );
    error(
        &create_raw(
            address,
            &device,
            &id(),
            1,
            &encoded,
            &[("Content-Encoding", "gzip")],
        )
        .await,
        400,
        "invalid_request",
        &device,
    );
    error(
        &create(address, &device, &id(), 2, &body).await,
        409,
        "operation_gap",
        &device,
    );
    for field in ["node_id", "parent_id", "name", "kind"] {
        let mut invalid = body.clone();
        invalid.as_object_mut().unwrap().remove(field);
        error(
            &create(address, &device, &id(), 1, &invalid).await,
            400,
            "invalid_request",
            &device,
        );
    }
    for name in [
        "../escape",
        "a/b",
        ".git",
        ".MDW-hidden",
        "e\u{301}",
        "COM1",
        "name ",
    ] {
        let mut invalid = body.clone();
        invalid["name"] = json!(name);
        error(
            &create(address, &device, &id(), 1, &invalid).await,
            400,
            "invalid_request",
            &device,
        );
    }
    let mut invalid = body.clone();
    invalid["upload_id"] = json!(id());
    error(
        &create(address, &device, &id(), 1, &invalid).await,
        400,
        "invalid_request",
        &device,
    );
    invalid = body.clone();
    invalid["node_id"] = json!(uuid::Uuid::new_v4().to_string());
    error(
        &create(address, &device, &id(), 1, &invalid).await,
        400,
        "invalid_request",
        &device,
    );
    for (header, duplicate) in [
        ("X-Operation-Seq", "1"),
        ("X-Workspace-Epoch", "invalid"),
        ("X-Device-Stream", "invalid"),
        ("Idempotency-Key", "invalid"),
    ] {
        error(
            &create_raw(
                address,
                &device,
                &id(),
                1,
                &serde_json::to_vec(&body).unwrap(),
                &[(header, duplicate)],
            )
            .await,
            400,
            "invalid_request",
            &device,
        );
    }
    error(
        &create(address, &device, &id(), 0, &body).await,
        400,
        "invalid_request",
        &device,
    );
    error(
        &create_raw(address, &device, &id(), 1, b"{", &[]).await,
        400,
        "invalid_request",
        &device,
    );
    error(
        &create_raw(
            address,
            &device,
            &id(),
            1,
            &vec![b' '; 1024 * 1024 + 1],
            &[],
        )
        .await,
        413,
        "invalid_request",
        &device,
    );
    let mut foreign = device.clone();
    foreign.device_stream_id = uuid::Uuid::new_v4().to_string();
    error(
        &create(address, &foreign, &id(), 1, &body).await,
        403,
        "forbidden",
        &device,
    );
    foreign = device.clone();
    foreign.workspace_epoch = uuid::Uuid::new_v4().to_string().parse().unwrap();
    error(
        &create(address, &foreign, &id(), 1, &body).await,
        409,
        "epoch_mismatch",
        &device,
    );
    assert_eq!(create(address, &device, &id(), 1, &body).await.status, 200);
    assert_eq!(
        std::fs::read_dir(&config.workspace.notes_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() == "valid.md")
            .count(),
        1
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn competing_creations_have_one_effect_and_ack_blocks_replay() {
    let (_directory, config, server, first, root) = setup().await;
    let address = server.local_addr();
    let second = pair(&config, address, WorkspaceRole::Editor).await;
    let first_body = create_body(&root, "same.md", "file");
    let second_body = create_body(&root, "same.md", "folder");
    let first_id = id();
    let second_id = id();
    let (a, b) = tokio::join!(
        create(address, &first, &first_id, 1, &first_body),
        create(address, &second, &second_id, 1, &second_body)
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
    let head = value(&get(address, &first, "/v1/server-info").await)["head_seq"].clone();
    assert_eq!(head, "1");
    for (device, operation, body) in [
        (&first, &first_id, &first_body),
        (&second, &second_id, &second_body),
    ] {
        assert_eq!(ack(address, device, 1).await.status, 200);
        error(
            &create(address, device, operation, 1, body).await,
            410,
            "operation_retired",
            device,
        );
        error(
            &get(address, device, &format!("/v1/operations/{operation}")).await,
            410,
            "operation_retired",
            device,
        );
    }
    assert_eq!(
        value(&get(address, &first, "/v1/server-info").await)["head_seq"],
        head
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_device_and_foreign_workspace_credentials_cannot_create() {
    let (_directory, config, server, device, root) = setup().await;
    let body = create_body(&root, "expired.md", "file");
    let (_other_directory, _other_config, other_server, other, _other_root) = setup().await;
    error(
        &create(server.local_addr(), &other, &id(), 1, &body).await,
        401,
        "unauthorized",
        &other,
    );
    other_server.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    let mut db = database(&config).await;
    sqlx::query(
        "UPDATE devices SET issued_at=1,idle_expires_at=2,absolute_expires_at=3 WHERE device_id=?",
    )
    .bind(&device.device_id)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    error(
        &create(restarted.local_addr(), &device, &id(), 1, &body).await,
        401,
        "unauthorized",
        &device,
    );
    assert!(!config.workspace.notes_root.join("expired.md").exists());
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn creation_commit_failure_keeps_pending_and_restart_finalizes_the_existing_file() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let mut db = database(&config).await;
    sqlx::raw_sql("CREATE TRIGGER synthetic_create_failure BEFORE INSERT ON nodes WHEN NEW.name='pending.md' BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let body = create_body(&root, "pending.md", "file");
    let operation = id();
    error(
        &create(address, &device, &operation, 1, &body).await,
        503,
        "storage_unavailable",
        &device,
    );
    assert!(
        std::fs::read(config.workspace.notes_root.join("pending.md"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        value(&get(address, &device, &format!("/v1/operations/{operation}")).await)["status"],
        "pending"
    );
    error(
        &create(address, &device, &operation, 1, &body).await,
        503,
        "recovery_pending",
        &device,
    );
    error(
        &ack(address, &device, 1).await,
        409,
        "operation_pending",
        &device,
    );
    error(
        &create(
            address,
            &device,
            &id(),
            2,
            &create_body(&root, "later.md", "file"),
        )
        .await,
        503,
        "recovery_pending",
        &device,
    );
    assert!(!config.workspace.notes_root.join("later.md").exists());
    server.shutdown().await.unwrap();
    let mut db = database(&config).await;
    sqlx::raw_sql("DROP TRIGGER synthetic_create_failure")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    let response = create(restarted.local_addr(), &device, &operation, 1, &body).await;
    assert_eq!(response.status, 200);
    assert_eq!(value(&response)["status"], "created");
    assert_eq!(
        value(&get(restarted.local_addr(), &device, "/v1/server-info").await)["head_seq"],
        "1"
    );
    assert_eq!(ack(restarted.local_addr(), &device, 1).await.status, 200);
    restarted.shutdown().await.unwrap();
}
