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

async fn mutation(
    address: SocketAddr,
    device: &PairResponse,
    method: &str,
    path: &str,
    identity: (&str, u64),
    etag: Option<&str>,
    body: Option<&Value>,
) -> HttpResponse {
    let authorization = format!("Bearer {}", device.device_token);
    let epoch = device.workspace_epoch.to_string();
    let sequence = identity.1.to_string();
    let mut headers = vec![
        ("Authorization", authorization.as_str()),
        ("X-Workspace-Epoch", epoch.as_str()),
        ("X-Device-Stream", device.device_stream_id.as_str()),
        ("X-Operation-Seq", sequence.as_str()),
        ("Idempotency-Key", identity.0),
    ];
    if let Some(etag) = etag {
        headers.push(("If-Match", etag));
    }
    let bytes = if let Some(body) = body {
        headers.push(("Content-Type", "application/json"));
        serde_json::to_vec(body).unwrap()
    } else {
        Vec::new()
    };
    request(address, method, path, &headers, &bytes).await
}

async fn delete(
    address: SocketAddr,
    device: &PairResponse,
    node: &str,
    identity: (&str, u64),
    etag: &str,
) -> HttpResponse {
    mutation(
        address,
        device,
        "DELETE",
        &format!("/v1/nodes/{node}"),
        identity,
        Some(etag),
        None,
    )
    .await
}

async fn restore(
    address: SocketAddr,
    device: &PairResponse,
    node: &str,
    identity: (&str, u64),
    body: &Value,
) -> HttpResponse {
    mutation(
        address,
        device,
        "POST",
        &format!("/v1/nodes/{node}/restore"),
        identity,
        None,
        Some(body),
    )
    .await
}

fn restore_body(revision: &Value, parent: &str, name: &str) -> Value {
    json!({"expected_meta_rev":revision,"parent_id":parent,"name":name})
}

async fn full(address: SocketAddr, device: &PairResponse, node: &str) -> Value {
    let response = get(address, device, &format!("/v1/nodes/{node}")).await;
    assert_eq!(response.status, 200);
    value(&response)
}

#[tokio::test]
async fn file_delete_and_restore_preserve_bytes_identity_history_and_aba_guards() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Original.md", "file");
    let node = file["node_id"].as_str().unwrap();
    let created = value(&create(address, &device, &id(), 1, &file).await)["node"].clone();
    let bytes = b"\xef\xbb\xbfraw\r\n\xff";
    assert_eq!(
        put(
            address,
            &device,
            node,
            2,
            created["content_etag"].as_str().unwrap(),
            bytes
        )
        .await
        .status,
        200
    );
    let before = full(address, &device, node).await;
    let operation = id();
    let deleted = delete(
        address,
        &device,
        node,
        (&operation, 3),
        before["node_etag"].as_str().unwrap(),
    )
    .await;
    assert_eq!(deleted.status, 200);
    let recorded = value(&deleted);
    assert_eq!(recorded["status"], "deleted");
    let tombstone = full(address, &device, node).await;
    assert_eq!(tombstone, recorded["node"]);
    assert_eq!(tombstone["deleted"], true);
    assert_eq!(tombstone["node_id"], before["node_id"]);
    assert_eq!(tombstone["content_hash"], before["content_hash"]);
    assert_ne!(tombstone["content_rev"], before["content_rev"]);
    assert_ne!(tombstone["meta_rev"], before["meta_rev"]);
    assert!(!config.workspace.notes_root.join("Original.md").exists());
    error(
        &get(address, &device, &format!("/v1/nodes/{node}/content")).await,
        410,
        "node_deleted",
        &device,
    );
    let metadata = value(&get(address, &device, &format!("/v1/nodes/{node}/metadata")).await);
    assert_eq!(metadata["deleted"], true);
    let live = value(&get(address, &device, "/v1/nodes").await);
    assert!(
        live["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["node_id"] != node)
    );
    let trash = value(&get(address, &device, "/v1/trash").await);
    assert_eq!(trash["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(trash["nodes"][0]["node_id"], node);
    assert_eq!(trash["nodes"][0]["restorable"], true);
    error(
        &create(address, &device, &id(), 4, &file).await,
        409,
        "node_id_conflict",
        &device,
    );
    let replacement = create_body(&root, "Original.md", "file");
    assert_eq!(
        create(address, &device, &id(), 5, &replacement)
            .await
            .status,
        200
    );
    error(
        &restore(
            address,
            &device,
            node,
            (&id(), 6),
            &restore_body(&before["meta_rev"], &root, "Restored.md"),
        )
        .await,
        412,
        "metadata_revision_mismatch",
        &device,
    );
    error(
        &restore(
            address,
            &device,
            node,
            (&id(), 7),
            &restore_body(&tombstone["meta_rev"], &root, "Original.md"),
        )
        .await,
        409,
        "destination_exists",
        &device,
    );
    let restore_operation = id();
    let body = restore_body(&tombstone["meta_rev"], &root, "Restored.md");
    let restored = restore(address, &device, node, (&restore_operation, 8), &body).await;
    assert_eq!(restored.status, 200);
    let restored_receipt = value(&restored);
    assert_eq!(restored_receipt["status"], "restored");
    let current = full(address, &device, node).await;
    assert_eq!(current["node_id"], node);
    assert_eq!(current["deleted"], false);
    assert_ne!(current["content_rev"], tombstone["content_rev"]);
    assert_ne!(current["content_rev"], before["content_rev"]);
    assert_ne!(current["meta_rev"], tombstone["meta_rev"]);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Restored.md")).unwrap(),
        bytes
    );
    error(
        &put(
            address,
            &device,
            node,
            9,
            before["content_etag"].as_str().unwrap(),
            b"stale save",
        )
        .await,
        412,
        "content_revision_mismatch",
        &device,
    );
    assert_eq!(
        value(
            &delete(
                address,
                &device,
                node,
                (&operation, 3),
                before["node_etag"].as_str().unwrap()
            )
            .await
        ),
        recorded
    );
    assert_eq!(
        value(&restore(address, &device, node, (&restore_operation, 8), &body).await),
        restored_receipt
    );
    for (field, value) in [
        ("name", json!("Different.md")),
        ("parent_id", json!(id())),
        ("expected_meta_rev", before["meta_rev"].clone()),
    ] {
        let mut changed = body.clone();
        changed[field] = value;
        error(
            &restore(address, &device, node, (&restore_operation, 8), &changed).await,
            409,
            "idempotency_mismatch",
            &device,
        );
    }
    assert_eq!(
        value(
            &get(
                address,
                &device,
                &format!("/v1/operations/{restore_operation}")
            )
            .await
        ),
        restored_receipt
    );
    server.shutdown().await.unwrap();
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    let address = restarted.local_addr();
    assert_eq!(
        value(&get(address, &device, &format!("/v1/operations/{operation}")).await),
        recorded
    );
    assert_eq!(
        value(
            &get(
                address,
                &device,
                &format!("/v1/operations/{restore_operation}")
            )
            .await
        ),
        restored_receipt
    );
    assert!(
        value(&get(address, &device, "/v1/trash").await)["nodes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(ack(address, &device, 9).await.status, 200);
    error(
        &delete(
            address,
            &device,
            node,
            (&operation, 3),
            before["node_etag"].as_str().unwrap(),
        )
        .await,
        410,
        "operation_retired",
        &device,
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Restored.md")).unwrap(),
        bytes
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn folder_deletion_checks_actual_entries_and_restores_only_an_empty_folder() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let folder = create_body(&root, "Folder", "folder");
    let node = folder["node_id"].as_str().unwrap();
    assert_eq!(
        create(address, &device, &id(), 1, &folder).await.status,
        200
    );
    let parent = full(address, &device, &root).await;
    error(
        &delete(
            address,
            &device,
            &root,
            (&id(), 2),
            parent["node_etag"].as_str().unwrap(),
        )
        .await,
        409,
        "root_immutable",
        &device,
    );
    let before = full(address, &device, node).await;
    std::fs::write(
        config.workspace.notes_root.join("Folder/.hidden"),
        b"preserved external file",
    )
    .unwrap();
    error(
        &delete(
            address,
            &device,
            node,
            (&id(), 3),
            before["node_etag"].as_str().unwrap(),
        )
        .await,
        409,
        "folder_not_empty",
        &device,
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Folder/.hidden")).unwrap(),
        b"preserved external file"
    );
    std::fs::remove_file(config.workspace.notes_root.join("Folder/.hidden")).unwrap();
    let child = create_body(node, "Child.md", "file");
    let child_id = child["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 4, &child).await.status, 200);
    let nonempty = full(address, &device, node).await;
    error(
        &delete(
            address,
            &device,
            node,
            (&id(), 5),
            nonempty["node_etag"].as_str().unwrap(),
        )
        .await,
        409,
        "folder_not_empty",
        &device,
    );
    let child_state = full(address, &device, child_id).await;
    assert_eq!(
        delete(
            address,
            &device,
            child_id,
            (&id(), 6),
            child_state["node_etag"].as_str().unwrap()
        )
        .await
        .status,
        200
    );
    let empty = full(address, &device, node).await;
    assert_eq!(
        delete(
            address,
            &device,
            node,
            (&id(), 7),
            empty["node_etag"].as_str().unwrap()
        )
        .await
        .status,
        200
    );
    let tombstone = full(address, &device, node).await;
    server.shutdown().await.unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let address = server.local_addr();
    assert_eq!(full(address, &device, child_id).await["parent_id"], node);
    assert_eq!(
        create(
            address,
            &device,
            &id(),
            8,
            &create_body(&root, "Folder", "folder")
        )
        .await
        .status,
        200
    );
    let restored = restore(
        address,
        &device,
        node,
        (&id(), 9),
        &restore_body(&tombstone["meta_rev"], &root, "Restored"),
    )
    .await;
    assert_eq!(restored.status, 200);
    let current = full(address, &device, node).await;
    assert_eq!(current["children_generation"], empty["children_generation"]);
    assert!(
        std::fs::read_dir(config.workspace.notes_root.join("Restored"))
            .unwrap()
            .next()
            .is_none()
    );
    assert_eq!(full(address, &device, child_id).await["deleted"], true);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_trash_content_does_not_prevent_startup_or_metadata_listing() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Expired.md", "file");
    let node = file["node_id"].as_str().unwrap();
    let created = value(&create(address, &device, &id(), 1, &file).await)["node"].clone();
    assert_eq!(
        put(
            address,
            &device,
            node,
            2,
            created["content_etag"].as_str().unwrap(),
            b"unique retained body"
        )
        .await
        .status,
        200
    );
    let before = full(address, &device, node).await;
    assert_eq!(
        delete(
            address,
            &device,
            node,
            (&id(), 3),
            before["node_etag"].as_str().unwrap()
        )
        .await
        .status,
        200
    );
    let tombstone = full(address, &device, node).await;
    server.shutdown().await.unwrap();
    let hash = tombstone["content_hash"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    std::fs::remove_file(
        config
            .workspace
            .state_root
            .join("objects/sha256")
            .join(hash),
    )
    .unwrap();
    let server = RunningServer::start(config.clone()).await.unwrap();
    let address = server.local_addr();
    let trash = get(address, &device, "/v1/trash").await;
    assert_eq!(trash.status, 200);
    assert_eq!(value(&trash)["nodes"][0]["restorable"], false);
    assert_eq!(full(address, &device, node).await, tombstone);
    error(
        &restore(
            address,
            &device,
            node,
            (&id(), 4),
            &restore_body(&tombstone["meta_rev"], &root, "Restored.md"),
        )
        .await,
        410,
        "history_expired",
        &device,
    );
    assert_eq!(
        value(&get(address, &device, "/v1/server-info").await)["ready"],
        true
    );
    assert!(!config.workspace.notes_root.join("Restored.md").exists());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deletion_protects_the_entire_own_state_and_replays_the_original_failure() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Note.md", "file");
    let node = file["node_id"].as_str().unwrap();
    let created = value(&create(address, &device, &id(), 1, &file).await)["node"].clone();
    assert_eq!(
        put(
            address,
            &device,
            node,
            2,
            created["content_etag"].as_str().unwrap(),
            b"new bytes"
        )
        .await
        .status,
        200
    );
    let operation = id();
    let failed = error(
        &delete(
            address,
            &device,
            node,
            (&operation, 3),
            created["node_etag"].as_str().unwrap(),
        )
        .await,
        412,
        "node_revision_mismatch",
        &device,
    );
    let current = full(address, &device, node).await;
    assert_eq!(
        failed["operation"]["current_node_etag"],
        current["node_etag"]
    );
    error(
        &delete(
            address,
            &device,
            node,
            (&operation, 3),
            current["node_etag"].as_str().unwrap(),
        )
        .await,
        409,
        "idempotency_mismatch",
        &device,
    );
    assert_eq!(
        value(&get(address, &device, &format!("/v1/operations/{operation}")).await),
        failed["operation"]
    );
    let move_body = json!({"expected_parent_id":root,"parent_id":root,"name":"Moved.md"});
    assert_eq!(
        mutation(
            address,
            &device,
            "PATCH",
            &format!("/v1/nodes/{node}/metadata"),
            (&id(), 4),
            current["meta_etag"].as_str(),
            Some(&move_body)
        )
        .await
        .status,
        200
    );
    error(
        &delete(
            address,
            &device,
            node,
            (&id(), 5),
            current["node_etag"].as_str().unwrap(),
        )
        .await,
        412,
        "node_revision_mismatch",
        &device,
    );
    let current = full(address, &device, node).await;
    error(
        &restore(
            address,
            &device,
            node,
            (&id(), 6),
            &restore_body(&current["meta_rev"], &root, "Unexpected.md"),
        )
        .await,
        409,
        "not_deleted",
        &device,
    );
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("Moved.md")).unwrap(),
        b"new bytes"
    );
    assert_eq!(
        delete(
            address,
            &device,
            node,
            (&id(), 7),
            current["node_etag"].as_str().unwrap()
        )
        .await
        .status,
        200
    );
    error(
        &delete(
            address,
            &device,
            node,
            (&id(), 8),
            current["node_etag"].as_str().unwrap(),
        )
        .await,
        410,
        "node_deleted",
        &device,
    );
    error(
        &put(
            address,
            &device,
            node,
            9,
            current["content_etag"].as_str().unwrap(),
            b"must not revive",
        )
        .await,
        410,
        "node_deleted",
        &device,
    );
    assert!(!config.workspace.notes_root.join("Moved.md").exists());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn trash_is_a_reader_visible_live_uuid_keyset_with_strict_queries() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let mut ids = Vec::new();
    for index in 0..3 {
        let file = create_body(&root, &format!("Note{index}.md"), "file");
        let node = file["node_id"].as_str().unwrap().to_owned();
        assert_eq!(
            create(address, &device, &id(), index * 2 + 1, &file)
                .await
                .status,
            200
        );
        let state = full(address, &device, &node).await;
        assert_eq!(
            delete(
                address,
                &device,
                &node,
                (&id(), index * 2 + 2),
                state["node_etag"].as_str().unwrap()
            )
            .await
            .status,
            200
        );
        ids.push(node);
    }
    ids.sort();
    let reader = pair(&config, address, WorkspaceRole::Reader).await;
    let first = get(address, &reader, "/v1/trash?limit=1").await;
    assert_eq!(first.status, 200);
    let first = value(&first);
    assert_eq!(first["workspace_epoch"], json!(device.workspace_epoch));
    assert_eq!(first["nodes"][0]["node_id"], ids[0]);
    assert_eq!(first["next_after_id"], ids[0]);
    let middle = full(address, &device, &ids[1]).await;
    assert_eq!(
        restore(
            address,
            &device,
            &ids[1],
            (&id(), 7),
            &restore_body(&middle["meta_rev"], &root, "Returned.md")
        )
        .await
        .status,
        200
    );
    let rest = get(
        address,
        &reader,
        &format!("/v1/trash?after_id={}&limit=100", ids[0]),
    )
    .await;
    assert_eq!(rest.status, 200);
    let rest = value(&rest);
    assert_eq!(rest["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(rest["nodes"][0]["node_id"], ids[2]);
    assert!(rest["next_after_id"].is_null());
    let empty = value(&get(address, &reader, &format!("/v1/trash?after_id={}", ids[2])).await);
    assert!(empty["nodes"].as_array().unwrap().is_empty());
    assert!(empty["next_after_id"].is_null());
    for query in [
        "limit=0",
        "limit=101",
        "limit=1&limit=2",
        "after_id=invalid",
        "unused=1",
        "limit=+1",
    ] {
        error(
            &get(address, &reader, &format!("/v1/trash?{query}")).await,
            400,
            "invalid_request",
            &reader,
        );
    }
    error(
        &request(address, "GET", "/v1/trash", &[], &[]).await,
        401,
        "unauthorized",
        &reader,
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_endpoints_require_conditions_and_reject_extra_or_oversized_bodies() {
    let (_directory, _config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Note.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 1, &file).await.status, 200);
    let current = full(address, &device, node).await;
    let path = format!("/v1/nodes/{node}");
    error(
        &mutation(address, &device, "DELETE", &path, (&id(), 2), None, None).await,
        428,
        "invalid_request",
        &device,
    );
    for invalid in ["*", "W/\"n.value\"", "\"one\", \"two\""] {
        error(
            &delete(address, &device, node, (&id(), 2), invalid).await,
            400,
            "invalid_request",
            &device,
        );
    }
    error(
        &mutation(
            address,
            &device,
            "DELETE",
            &path,
            (&id(), 2),
            current["node_etag"].as_str(),
            Some(&json!({"recursive":true})),
        )
        .await,
        400,
        "invalid_request",
        &device,
    );
    assert_eq!(
        delete(
            address,
            &device,
            node,
            (&id(), 2),
            current["node_etag"].as_str().unwrap()
        )
        .await
        .status,
        200
    );
    let tombstone = full(address, &device, node).await;
    for bad in [
        json!({"parent_id":root,"name":"Note.md"}),
        json!({"expected_meta_rev":"","parent_id":root,"name":"Note.md"}),
        json!({"expected_meta_rev":tombstone["meta_rev"],"parent_id":root,"name":"../bad"}),
        json!({"expected_meta_rev":tombstone["meta_rev"],"parent_id":root,"name":"Note.md","extra":true}),
    ] {
        error(
            &restore(address, &device, node, (&id(), 3), &bad).await,
            400,
            "invalid_request",
            &device,
        );
    }
    let oversized = json!({"expected_meta_rev":tombstone["meta_rev"],"parent_id":root,"name":"a".repeat(1024*1024+1)});
    error(
        &restore(address, &device, node, (&id(), 3), &oversized).await,
        413,
        "invalid_request",
        &device,
    );
    assert_eq!(
        restore(
            address,
            &device,
            node,
            (&id(), 3),
            &restore_body(&tombstone["meta_rev"], &root, "Note.md")
        )
        .await
        .status,
        200
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_demotion_preserves_lifecycle_receipts_but_blocks_new_changes_and_revoke_blocks_reads()
 {
    let (_directory, config, server, owner, root) = setup().await;
    let address = server.local_addr();
    let device = pair(&config, address, WorkspaceRole::Editor).await;
    let file = create_body(&root, "Note.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &owner, &id(), 1, &file).await.status, 200);
    let before = full(address, &device, node).await;
    let deletion = id();
    let deleted = delete(
        address,
        &device,
        node,
        (&deletion, 1),
        before["node_etag"].as_str().unwrap(),
    )
    .await;
    assert_eq!(deleted.status, 200);
    let recorded = value(&deleted);
    let tombstone = full(address, &device, node).await;
    let body = restore_body(&tombstone["meta_rev"], &root, "Note.md");
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
            &delete(
                address,
                &device,
                node,
                (&deletion, 1),
                before["node_etag"].as_str().unwrap()
            )
            .await
        ),
        recorded
    );
    error(
        &restore(address, &device, node, (&id(), 2), &body).await,
        403,
        "forbidden",
        &device,
    );
    error(
        &delete(
            address,
            &device,
            node,
            (&id(), 2),
            tombstone["node_etag"].as_str().unwrap(),
        )
        .await,
        403,
        "forbidden",
        &device,
    );
    assert_eq!(get(address, &device, "/v1/trash").await.status, 200);
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::SetRole {
            principal_id: device.principal_id.clone(),
            role: WorkspaceRole::Editor,
        },
    )
    .await
    .unwrap();
    let restoration = id();
    let restored = restore(address, &device, node, (&restoration, 2), &body).await;
    assert_eq!(restored.status, 200);
    let restored_receipt = value(&restored);
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
        value(&restore(address, &device, node, (&restoration, 2), &body).await),
        restored_receipt
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
        &get(address, &device, "/v1/trash").await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &delete(
            address,
            &device,
            node,
            (&deletion, 1),
            before["node_etag"].as_str().unwrap(),
        )
        .await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &restore(address, &device, node, (&restoration, 2), &body).await,
        401,
        "unauthorized",
        &device,
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_rejects_foreign_expired_and_wrong_bindings_without_consuming_a_sequence() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let file = create_body(&root, "Note.md", "file");
    let node = file["node_id"].as_str().unwrap();
    assert_eq!(create(address, &device, &id(), 1, &file).await.status, 200);
    let current = full(address, &device, node).await;
    let body = restore_body(&current["meta_rev"], &root, "Note.md");
    let etag = current["node_etag"].as_str().unwrap();
    let mut wrong = device.clone();
    wrong.device_stream_id = uuid::Uuid::new_v4().to_string();
    error(
        &delete(address, &wrong, node, (&id(), 2), etag).await,
        403,
        "forbidden",
        &device,
    );
    error(
        &restore(address, &wrong, node, (&id(), 2), &body).await,
        403,
        "forbidden",
        &device,
    );
    wrong = device.clone();
    wrong.workspace_epoch = uuid::Uuid::new_v4().to_string().parse().unwrap();
    error(
        &delete(address, &wrong, node, (&id(), 2), etag).await,
        409,
        "epoch_mismatch",
        &device,
    );
    error(
        &restore(address, &wrong, node, (&id(), 2), &body).await,
        409,
        "epoch_mismatch",
        &device,
    );
    let (_other_dir, _other_config, other, foreign, _other_root) = setup().await;
    error(
        &delete(address, &foreign, node, (&id(), 2), etag).await,
        401,
        "unauthorized",
        &foreign,
    );
    error(
        &restore(address, &foreign, node, (&id(), 2), &body).await,
        401,
        "unauthorized",
        &foreign,
    );
    error(
        &get(address, &foreign, "/v1/trash").await,
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
        &delete(address, &device, node, (&id(), 2), etag).await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &restore(address, &device, node, (&id(), 2), &body).await,
        401,
        "unauthorized",
        &device,
    );
    error(
        &get(address, &device, "/v1/trash").await,
        401,
        "unauthorized",
        &device,
    );
    let mut db = database(&config).await;
    let last: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(&device.device_stream_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts WHERE stream_id=?")
            .bind(&device.device_stream_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(last, "1");
    assert_eq!(count, 1);
    db.close().await.unwrap();
    assert!(config.workspace.notes_root.join("Note.md").exists());
    server.shutdown().await.unwrap();
}
