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

async fn patch(
    address: SocketAddr,
    device: &PairResponse,
    node: &str,
    operation: &str,
    sequence: u64,
    etag: &str,
    body: &Value,
) -> HttpResponse {
    request(
        address,
        "PATCH",
        &format!("/v1/nodes/{node}/metadata"),
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", &sequence.to_string()),
            ("Idempotency-Key", operation),
            ("If-Match", etag),
        ],
        &serde_json::to_vec(body).unwrap(),
    )
    .await
}

fn move_body(from: &str, to: &str, name: &str) -> Value {
    json!({"expected_parent_id":from,"parent_id":to,"name":name})
}

async fn full(address: SocketAddr, device: &PairResponse, node: &str) -> Value {
    let response = get(address, device, &format!("/v1/nodes/{node}")).await;
    assert_eq!(response.status, 200);
    value(&response)
}

#[tokio::test]
async fn file_patch_uses_metadata_conditions_and_preserves_bytes_and_receipts_after_restart() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let folder = create_body(&root, "Target", "folder");
    assert_eq!(
        create(address, &device, &id(), 1, &folder).await.status,
        200
    );
    let target = folder["node_id"].as_str().unwrap();
    let file = create_body(&root, "Raw.md", "file");
    let node = file["node_id"].as_str().unwrap();
    let created = value(&create(address, &device, &id(), 2, &file).await)["node"].clone();
    let bytes = b"\xef\xbb\xbf# raw\r\n\xff";
    assert_eq!(
        put(
            address,
            &device,
            node,
            3,
            created["content_etag"].as_str().unwrap(),
            bytes
        )
        .await
        .status,
        200
    );
    let before = full(address, &device, node).await;
    let operation = id();
    let body = move_body(&root, target, "Renamed.md");
    let response = patch(
        address,
        &device,
        node,
        &operation,
        4,
        created["meta_etag"].as_str().unwrap(),
        &body,
    )
    .await;
    assert_eq!(response.status, 200);
    let recorded = value(&response);
    assert_eq!(recorded["status"], "moved");
    let after = full(address, &device, node).await;
    assert_eq!(recorded["node"], after);
    for field in [
        "node_id",
        "content_rev",
        "content_etag",
        "content_hash",
        "size",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert_ne!(after["meta_rev"], before["meta_rev"]);
    assert_eq!(after["parent_id"], target);
    assert_eq!(after["name"], "Renamed.md");
    assert!(!config.workspace.notes_root.join("Raw.md").exists());
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Target/Renamed.md")).unwrap(),
        bytes
    );
    assert_eq!(
        put(
            address,
            &device,
            node,
            5,
            after["content_etag"].as_str().unwrap(),
            b"later\n"
        )
        .await
        .status,
        200
    );
    server.shutdown().await.unwrap();
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    let address = restarted.local_addr();
    assert_eq!(
        value(
            &patch(
                address,
                &device,
                node,
                &operation,
                4,
                created["meta_etag"].as_str().unwrap(),
                &body
            )
            .await
        ),
        recorded
    );
    assert_eq!(
        value(&get(address, &device, &format!("/v1/operations/{operation}")).await),
        recorded
    );
    for (field, changed) in [
        ("name", json!("Different.md")),
        ("parent_id", json!(root)),
        ("expected_parent_id", json!(target)),
    ] {
        let mut altered = body.clone();
        altered[field] = changed;
        error(
            &patch(
                address,
                &device,
                node,
                &operation,
                4,
                created["meta_etag"].as_str().unwrap(),
                &altered,
            )
            .await,
            409,
            "idempotency_mismatch",
            &device,
        );
    }
    error(
        &patch(
            address,
            &device,
            node,
            &operation,
            4,
            after["meta_etag"].as_str().unwrap(),
            &body,
        )
        .await,
        409,
        "idempotency_mismatch",
        &device,
    );
    assert_eq!(
        value(&get(address, &device, &format!("/v1/operations/{operation}")).await),
        recorded
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Target/Renamed.md")).unwrap(),
        b"later\n"
    );
    assert_eq!(ack(address, &device, 5).await.status, 200);
    error(
        &patch(
            address,
            &device,
            node,
            &operation,
            4,
            created["meta_etag"].as_str().unwrap(),
            &body,
        )
        .await,
        410,
        "operation_retired",
        &device,
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn folder_move_preserves_descendant_own_state_and_updates_only_direct_parents() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let source = create_body(&root, "Source", "folder");
    let target = create_body(&root, "Target", "folder");
    let source_id = source["node_id"].as_str().unwrap();
    let target_id = target["node_id"].as_str().unwrap();
    assert_eq!(
        create(address, &device, &id(), 1, &source).await.status,
        200
    );
    assert_eq!(
        create(address, &device, &id(), 2, &target).await.status,
        200
    );
    let child = create_body(source_id, "Note.md", "file");
    let child_id = child["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 3, &child).await.status, 200);
    let child_before = full(address, &device, child_id).await;
    let before = full(address, &device, source_id).await;
    let root_before = full(address, &device, &root).await;
    let target_before = full(address, &device, target_id).await;
    let response = patch(
        address,
        &device,
        source_id,
        &id(),
        4,
        before["meta_etag"].as_str().unwrap(),
        &move_body(&root, target_id, "Moved"),
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(full(address, &device, child_id).await, child_before);
    assert!(
        config
            .workspace
            .notes_root
            .join("Target/Moved/Note.md")
            .is_file()
    );
    assert!(!config.workspace.notes_root.join("Source").exists());
    for (parent, old) in [(root.as_str(), &root_before), (target_id, &target_before)] {
        let new = full(address, &device, parent).await;
        assert_eq!(new["meta_rev"], old["meta_rev"]);
        assert_eq!(
            new["children_generation"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            old["children_generation"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                + 1
        );
    }
    let moved = full(address, &device, source_id).await;
    assert_eq!(moved["children_generation"], before["children_generation"]);
    let root_now = full(address, &device, &root).await;
    error(
        &patch(
            address,
            &device,
            &root,
            &id(),
            5,
            root_now["meta_etag"].as_str().unwrap(),
            &move_body(&root, target_id, "Invalid"),
        )
        .await,
        409,
        "root_immutable",
        &device,
    );
    error(
        &patch(
            address,
            &device,
            target_id,
            &id(),
            6,
            target_before["meta_etag"].as_str().unwrap(),
            &move_body(&root, source_id, "Cycle"),
        )
        .await,
        409,
        "cycle_detected",
        &device,
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn case_only_rename_and_noop_keep_the_expected_revision_boundaries() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Note.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 1, &file).await.status, 200);
    let before = full(address, &device, node).await;
    assert_eq!(
        patch(
            address,
            &device,
            node,
            &id(),
            2,
            before["meta_etag"].as_str().unwrap(),
            &move_body(&root, &root, "note.md")
        )
        .await
        .status,
        200
    );
    let after = full(address, &device, node).await;
    assert_ne!(before["meta_rev"], after["meta_rev"]);
    assert_eq!(before["content_rev"], after["content_rev"]);
    let info = value(&get(address, &device, "/v1/server-info").await);
    let parent = full(address, &device, &root).await;
    assert_eq!(
        patch(
            address,
            &device,
            node,
            &id(),
            3,
            after["meta_etag"].as_str().unwrap(),
            &move_body(&root, &root, "note.md")
        )
        .await
        .status,
        200
    );
    assert_eq!(after, full(address, &device, node).await);
    assert_eq!(parent, full(address, &device, &root).await);
    assert_eq!(
        info["head_seq"],
        value(&get(address, &device, "/v1/server-info").await)["head_seq"]
    );
    assert!(config.workspace.notes_root.join("note.md").exists());
    assert!(!config.workspace.notes_root.join("Note.md").exists());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_conditions_and_destination_conflicts_are_terminal_replayable_results() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Original.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 1, &file).await.status, 200);
    let before = full(address, &device, node).await;
    let body = move_body(&root, &root, "Moved.md");
    let operation = id();
    let failed = error(
        &patch(address, &device, node, &operation, 2, "\"m.stale\"", &body).await,
        412,
        "metadata_revision_mismatch",
        &device,
    );
    assert_eq!(failed["operation"]["current_meta_rev"], before["meta_rev"]);
    assert_eq!(
        value(&get(address, &device, &format!("/v1/operations/{operation}")).await),
        failed["operation"]
    );
    error(
        &patch(
            address,
            &device,
            node,
            &id(),
            3,
            before["meta_etag"].as_str().unwrap(),
            &move_body(&id(), &root, "Moved.md"),
        )
        .await,
        409,
        "expected_parent_mismatch",
        &device,
    );
    std::fs::write(
        config.workspace.notes_root.join("OCCUPIED.md"),
        b"external evidence",
    )
    .unwrap();
    error(
        &patch(
            address,
            &device,
            node,
            &id(),
            4,
            before["meta_etag"].as_str().unwrap(),
            &move_body(&root, &root, "occupied.md"),
        )
        .await,
        409,
        "destination_exists",
        &device,
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("OCCUPIED.md")).unwrap(),
        b"external evidence"
    );
    assert_eq!(full(address, &device, node).await, before);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_requires_valid_metadata_preconditions_and_strict_control_json() {
    let (_directory, _config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Original.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 1, &file).await.status, 200);
    let before = full(address, &device, node).await;
    let body = serde_json::to_vec(&move_body(&root, &root, "Moved.md")).unwrap();
    let authorization = format!("Bearer {}", device.device_token);
    let epoch = device.workspace_epoch.to_string();
    let operation = id();
    let headers = vec![
        ("Authorization", authorization.as_str()),
        ("Content-Type", "application/json"),
        ("X-Workspace-Epoch", epoch.as_str()),
        ("X-Device-Stream", device.device_stream_id.as_str()),
        ("X-Operation-Seq", "2"),
        ("Idempotency-Key", operation.as_str()),
    ];
    let path = format!("/v1/nodes/{node}/metadata");
    error(
        &request(address, "PATCH", &path, &[], &body).await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &request(address, "PATCH", &path, &headers, &body).await,
        428,
        "invalid_request",
        &device,
    );
    for invalid in ["*", "W/\"m.x\"", "\"\"", "\"m.x\", \"m.y\""] {
        let mut wrong = headers.clone();
        wrong.push(("If-Match", invalid));
        error(
            &request(address, "PATCH", &path, &wrong, &body).await,
            400,
            "invalid_request",
            &device,
        );
    }
    let mut valid = headers.clone();
    valid.push(("If-Match", before["meta_etag"].as_str().unwrap()));
    let mut duplicate = valid.clone();
    duplicate.push(("If-Match", before["meta_etag"].as_str().unwrap()));
    error(
        &request(address, "PATCH", &path, &duplicate, &body).await,
        400,
        "invalid_request",
        &device,
    );
    for bad in [
        json!({"expected_parent_id":root,"parent_id":root,"name":"../bad"}),
        json!({"expected_parent_id":root,"parent_id":root,"name":"Moved.md","extra":true}),
    ] {
        error(
            &request(
                address,
                "PATCH",
                &path,
                &valid,
                &serde_json::to_vec(&bad).unwrap(),
            )
            .await,
            400,
            "invalid_request",
            &device,
        );
    }
    error(
        &request(
            address,
            "PATCH",
            &path,
            &valid,
            &vec![b' '; 1024 * 1024 + 1],
        )
        .await,
        413,
        "invalid_request",
        &device,
    );
    assert_eq!(
        request(address, "PATCH", &path, &valid, &body).await.status,
        200
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn readers_cannot_move_but_can_replay_their_results_until_revocation() {
    let (_directory, config, server, owner, root) = setup().await;
    let address = server.local_addr();
    let device = pair(&config, address, WorkspaceRole::Editor).await;
    let file = create_body(&root, "Original.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &owner, &id(), 1, &file).await.status, 200);
    let before = full(address, &device, node).await;
    let body = move_body(&root, &root, "Moved.md");
    let operation = id();
    let moved = patch(
        address,
        &device,
        node,
        &operation,
        1,
        before["meta_etag"].as_str().unwrap(),
        &body,
    )
    .await;
    assert_eq!(moved.status, 200);
    let recorded = value(&moved);
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::SetRole {
            principal_id: device.principal_id.clone(),
            role: WorkspaceRole::Reader,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        value(
            &patch(
                address,
                &device,
                node,
                &operation,
                1,
                before["meta_etag"].as_str().unwrap(),
                &body
            )
            .await
        ),
        recorded
    );
    let current = full(address, &device, node).await;
    error(
        &patch(
            address,
            &device,
            node,
            &id(),
            2,
            current["meta_etag"].as_str().unwrap(),
            &move_body(&root, &root, "Forbidden.md"),
        )
        .await,
        403,
        "forbidden",
        &device,
    );
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: device.device_id.clone(),
        },
    )
    .await
    .unwrap();
    error(
        &patch(
            address,
            &device,
            node,
            &operation,
            1,
            before["meta_etag"].as_str().unwrap(),
            &body,
        )
        .await,
        401,
        "unauthorized",
        &device,
    );
    assert!(config.workspace.notes_root.join("Moved.md").exists());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_rejects_expired_foreign_and_wrong_stream_bindings_without_admission() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Original.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 1, &file).await.status, 200);
    let before = full(address, &device, node).await;
    let body = move_body(&root, &root, "Moved.md");
    let etag = before["meta_etag"].as_str().unwrap();
    let mut wrong = device.clone();
    wrong.device_stream_id = uuid::Uuid::new_v4().to_string();
    error(
        &patch(address, &wrong, node, &id(), 2, etag, &body).await,
        403,
        "forbidden",
        &device,
    );
    wrong = device.clone();
    wrong.workspace_epoch = uuid::Uuid::new_v4().to_string().parse().unwrap();
    error(
        &patch(address, &wrong, node, &id(), 2, etag, &body).await,
        409,
        "epoch_mismatch",
        &device,
    );
    let (_other_dir, _other_config, other, foreign, _other_root) = setup().await;
    error(
        &patch(address, &foreign, node, &id(), 2, etag, &body).await,
        401,
        "unauthorized",
        &foreign,
    );
    other.shutdown().await.unwrap();
    let mut db = database(&config).await;
    sqlx::query(
        "UPDATE devices SET issued_at=1,idle_expires_at=2,absolute_expires_at=3 WHERE device_id=?",
    )
    .bind(&device.device_id)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    error(
        &patch(address, &device, node, &id(), 2, etag, &body).await,
        401,
        "unauthorized",
        &device,
    );
    assert!(config.workspace.notes_root.join("Original.md").exists());
    let mut db = database(&config).await;
    let last: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(&device.device_stream_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts WHERE stream_id=?")
            .bind(&device.device_stream_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(last, "1");
    assert_eq!(receipts, 1);
    db.close().await.unwrap();
    server.shutdown().await.unwrap();
}
