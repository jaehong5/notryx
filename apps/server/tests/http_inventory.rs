#[path = "support/legacy.rs"]
mod legacy;
pub mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, PairResponse, WorkspaceRole};
use notryx_server::{Workspace, admin, config::Config, http::RunningServer};
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
    let pairing = pairing_request(&config).await.0;
    let paired = post_json(server.local_addr(), &serde_json::to_vec(&pairing).unwrap()).await;
    assert_eq!(paired.status, 200);
    let device: PairResponse = serde_json::from_slice(&paired.body).ok().unwrap();
    let root = body(&get(server.local_addr(), &device, "/v1/server-info").await)["root_id"]
        .as_str()
        .unwrap()
        .to_owned();
    (directory, config, server, device, root)
}

fn id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn body(response: &HttpResponse) -> Value {
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    serde_json::from_slice(&response.body).unwrap()
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

async fn create(
    address: SocketAddr,
    device: &PairResponse,
    sequence: u64,
    operation: &str,
    node: &str,
    parent: &str,
    kind: &str,
) -> HttpResponse {
    let value = json!({"node_id":node,"parent_id":parent,"name":format!("{node}.md"),"kind":kind});
    request(
        address,
        "POST",
        "/v1/nodes",
        &[
            ("Authorization", &format!("Bearer {}", device.device_token)),
            ("Content-Type", "application/json"),
            ("X-Workspace-Epoch", &device.workspace_epoch.to_string()),
            ("X-Device-Stream", &device.device_stream_id),
            ("X-Operation-Seq", &sequence.to_string()),
            ("Idempotency-Key", operation),
        ],
        &serde_json::to_vec(&value).unwrap(),
    )
    .await
}

async fn put(
    address: SocketAddr,
    device: &PairResponse,
    sequence: u64,
    operation: &str,
    node: &str,
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
            ("Idempotency-Key", operation),
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

async fn database(config: &Config) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn node_get_inventory_and_new_receipts_share_own_state_etags() {
    let (_directory, _config, server, device, root) = setup().await;
    let address = server.local_addr();
    let initial = get(address, &device, &format!("/v1/nodes/{root}")).await;
    assert_eq!(initial.status, 200);
    let initial_root = body(&initial);
    assert_eq!(initial.header("etag"), initial_root["node_etag"].as_str());
    assert_eq!(initial_root["name"], "");
    assert!(initial_root["parent_id"].is_null());
    assert_eq!(initial_root["kind"], "folder");
    let node = id();
    let operation = id();
    let created = create(address, &device, 1, &operation, &node, &root, "file").await;
    assert_eq!(created.status, 200);
    let receipt = body(&created);
    let fetched = get(address, &device, &format!("/v1/nodes/{node}")).await;
    assert_eq!(fetched.status, 200);
    assert_eq!(body(&fetched), receipt["node"]);
    assert_eq!(
        fetched.header("etag"),
        receipt["node"]["node_etag"].as_str()
    );
    assert!(body(&fetched).get("path").is_none());
    let root_after = body(&get(address, &device, &format!("/v1/nodes/{root}")).await);
    assert_ne!(initial_root["node_etag"], root_after["node_etag"]);
    assert_eq!(initial_root["meta_etag"], root_after["meta_etag"]);
    assert_eq!(root_after["children_generation"], "1");
    let raw = b"\xef\xbb\xbf# raw\r\n\xff";
    let saved = put(
        address,
        &device,
        2,
        &id(),
        &node,
        receipt["node"]["content_etag"].as_str().unwrap(),
        raw,
    )
    .await;
    assert_eq!(saved.status, 200);
    let saved = body(&saved);
    let full = body(&get(address, &device, &format!("/v1/nodes/{node}")).await);
    assert_eq!(saved["node_etag"], full["node_etag"]);
    assert_eq!(saved["content_etag"], full["content_etag"]);
    assert_eq!(saved["meta_etag"], full["meta_etag"]);
    let page = get(address, &device, "/v1/nodes").await;
    assert_eq!(page.status, 200);
    assert!(page.header("etag").is_none());
    let page = body(&page);
    assert_eq!(page["workspace_epoch"], device.workspace_epoch.to_string());
    assert!(page["next_after_id"].is_null());
    assert!(page["nodes"].as_array().unwrap().contains(&full));
    let unchanged = put(
        address,
        &device,
        3,
        &id(),
        &node,
        full["content_etag"].as_str().unwrap(),
        raw,
    )
    .await;
    assert_eq!(unchanged.status, 200);
    let unchanged = body(&unchanged);
    assert_eq!(unchanged["content_etag"], full["content_etag"]);
    assert_ne!(unchanged["node_etag"], full["node_etag"]);
    assert_eq!(
        body(&get(address, &device, &format!("/v1/operations/{operation}")).await),
        receipt
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn inventory_is_an_exclusive_uuid_keyset_and_observes_later_commits() {
    let (_directory, _config, server, device, root) = setup().await;
    let address = server.local_addr();
    let ids = [
        "00000000-0000-7000-8000-000000000003",
        "00000000-0000-7000-8000-000000000001",
        "00000000-0000-7000-8000-000000000005",
    ];
    for (index, node) in ids.iter().enumerate() {
        assert_eq!(
            create(
                address,
                &device,
                index as u64 + 1,
                &id(),
                node,
                &root,
                "file"
            )
            .await
            .status,
            200
        );
    }
    let first = get(address, &device, "/v1/nodes?limit=2").await;
    assert_eq!(first.status, 200);
    let first = body(&first);
    assert_eq!(first["nodes"][0]["node_id"], ids[1]);
    assert_eq!(first["nodes"][1]["node_id"], ids[0]);
    assert_eq!(first["next_after_id"], ids[0]);
    let inserted = "00000000-0000-7000-8000-000000000004";
    assert_eq!(
        create(address, &device, 4, &id(), inserted, &root, "folder")
            .await
            .status,
        200
    );
    let second = body(
        &get(
            address,
            &device,
            &format!("/v1/nodes?after_id={}&limit=2", ids[0]),
        )
        .await,
    );
    assert_eq!(second["nodes"][0]["node_id"], inserted);
    assert_eq!(second["nodes"][1]["node_id"], ids[2]);
    assert_eq!(second["next_after_id"], ids[2]);
    let last = body(
        &get(
            address,
            &device,
            &format!("/v1/nodes?after_id={}&limit=2", ids[2]),
        )
        .await,
    );
    assert_eq!(last["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(last["nodes"][0]["node_id"], root);
    assert!(last["next_after_id"].is_null());
    let absent_cursor = body(
        &get(
            address,
            &device,
            "/v1/nodes?after_id=00000000-0000-7000-8000-000000000002&limit=1",
        )
        .await,
    );
    assert_eq!(absent_cursor["nodes"][0]["node_id"], ids[0]);
    let empty = body(
        &get(
            address,
            &device,
            "/v1/nodes?after_id=ffffffff-ffff-7fff-bfff-ffffffffffff",
        )
        .await,
    );
    assert!(empty["nodes"].as_array().unwrap().is_empty());
    assert!(empty["next_after_id"].is_null());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn inventory_rejects_invalid_duplicate_and_unknown_query_fields() {
    let (_directory, _config, server, device, _root) = setup().await;
    for query in [
        "limit=0",
        "limit=101",
        "limit=-1",
        "limit=%2B1",
        "limit=",
        "limit=18446744073709551616",
        "limit=1&limit=2",
        "unknown=1",
        "after_id=",
        "after_id=invalid",
        "after_id=00000000-0000-4000-8000-000000000000",
        "after_id=00000000-0000-7000-8000-000000000001&after_id=00000000-0000-7000-8000-000000000002",
    ] {
        let response = get(server.local_addr(), &device, &format!("/v1/nodes?{query}")).await;
        assert_eq!(response.status, 400, "{query}");
        assert_eq!(body(&response)["code"], "invalid_request");
    }
    let missing = get(server.local_addr(), &device, &format!("/v1/nodes/{}", id())).await;
    assert_eq!(missing.status, 404);
    assert_eq!(body(&missing)["code"], "not_found");
    assert_eq!(
        get(server.local_addr(), &device, "/v1/nodes/not-a-node")
            .await
            .status,
        400
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn inventory_and_node_get_enforce_current_authentication_and_allow_readers() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let principal = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Reader,
        },
    )
    .await
    .unwrap();
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = principal
    else {
        panic!("synthetic principal");
    };
    let grant = admin::request(
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
    } = grant
    else {
        panic!("synthetic pairing");
    };
    let paired = post_json(address, &serde_json::to_vec(&json!({"protocol":notryx_protocol::CONTRACT,"workspace_id":workspace_id,"pairing_secret":pairing_secret})).unwrap()).await;
    assert_eq!(paired.status, 200);
    let reader: PairResponse = serde_json::from_slice(&paired.body).ok().unwrap();
    for path in ["/v1/nodes".to_owned(), format!("/v1/nodes/{root}")] {
        let anonymous = request(address, "GET", &path, &[], &[]).await;
        assert_eq!(anonymous.status, 401);
        assert_eq!(body(&anonymous)["code"], "unauthorized");
        assert_eq!(get(address, &reader, &path).await.status, 200);
    }
    admin::request(
        &config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: reader.device_id.clone(),
        },
    )
    .await
    .unwrap();
    for path in ["/v1/nodes".to_owned(), format!("/v1/nodes/{root}")] {
        assert_eq!(get(address, &reader, &path).await.status, 401);
    }
    let (_other_directory, _other_config, other_server, other, _other_root) = setup().await;
    assert_eq!(get(address, &other, "/v1/nodes").await.status, 401);
    assert_eq!(
        get(address, &other, &format!("/v1/nodes/{root}"))
            .await
            .status,
        401
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
    let restarted = RunningServer::start(config).await.unwrap();
    assert_eq!(
        get(restarted.local_addr(), &device, "/v1/nodes")
            .await
            .status,
        401
    );
    assert_eq!(
        get(
            restarted.local_addr(),
            &device,
            &format!("/v1/nodes/{root}")
        )
        .await
        .status,
        401
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_inventory_remains_readable_when_a_content_object_is_lost() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let node = id();
    let created = create(address, &device, 1, &id(), &node, &root, "file").await;
    assert_eq!(created.status, 200);
    let created = body(&created);
    let path = format!("/v1/nodes/{node}");
    assert_eq!(get(address, &device, &path).await.status, 200);
    let hash = created["node"]["content_hash"]
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
    assert_eq!(
        get(address, &device, &format!("{path}/content"))
            .await
            .status,
        503
    );
    assert_eq!(get(address, &device, &path).await.status, 200);
    assert_eq!(get(address, &device, "/v1/nodes").await.status, 200);
    assert_eq!(
        body(&get(address, &device, "/v1/server-info").await)["ready"],
        false
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn migrating_legacy_receipts_preserves_their_http_results_after_later_content_changes() {
    let (_directory, config, server, device, root) = setup().await;
    let address = server.local_addr();
    let node = id();
    let create_id = id();
    let created = body(&create(address, &device, 1, &create_id, &node, &root, "file").await);
    let first_etag = created["node"]["content_etag"].as_str().unwrap();
    let save_id = id();
    let mut saved = body(&put(address, &device, 2, &save_id, &node, first_etag, b"old").await);
    let latest = put(
        address,
        &device,
        3,
        &id(),
        &node,
        saved["content_etag"].as_str().unwrap(),
        b"latest",
    )
    .await;
    assert_eq!(latest.status, 200);
    server.shutdown().await.unwrap();
    let mut db = database(&config).await;
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM operation_receipts WHERE operation_id=?")
            .bind(&save_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    let outcome: notryx_server::Outcome = serde_json::from_str(&outcome).unwrap();
    let notryx_server::Outcome::Saved(document) = outcome else {
        panic!("saved fixture");
    };
    // Pin schema 4's field order independently of the current Document serializer.
    let legacy_json = format!(
        "{{\"node_id\":{},\"parent_id\":{},\"name\":{},\"content_rev\":{},\"meta_rev\":{},\"content_hash\":{},\"size\":{},\"last_change_seq\":{}}}",
        json!(document.node_id),
        json!(document.parent_id),
        json!(document.name),
        json!(document.content_rev),
        json!(document.meta_rev),
        json!(document.content_hash),
        document.size,
        json!(document.last_change_seq),
    );
    let legacy_hash: String = Sha256::digest(legacy_json.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    saved["node_etag"] = json!(format!("\"n.{legacy_hash}\""));
    legacy::nodes_before_deletion(&mut db).await;
    sqlx::raw_sql("ALTER TABLE operation_receipts DROP COLUMN node_etag; PRAGMA user_version=4;")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    std::fs::remove_file(
        config
            .workspace
            .state_root
            .join("objects/sha256")
            .join(&document.content_hash),
    )
    .unwrap();
    Workspace::migrate(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    let address = restarted.local_addr();
    assert_eq!(
        body(&get(address, &device, &format!("/v1/operations/{save_id}")).await),
        saved
    );
    assert_eq!(
        body(&put(address, &device, 2, &save_id, &node, first_etag, b"old").await),
        saved
    );
    assert_eq!(
        body(&get(address, &device, &format!("/v1/operations/{create_id}")).await),
        created
    );
    assert_eq!(
        body(&create(address, &device, 1, &create_id, &node, &root, "file").await),
        created
    );
    assert_eq!(
        get(address, &device, &format!("/v1/nodes/{node}/content"))
            .await
            .body,
        b"latest"
    );
    restarted.shutdown().await.unwrap();
}
