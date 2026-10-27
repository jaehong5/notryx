mod support;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_domain::OperationId;
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, PairResponse};
use notryx_server::{Document, Workspace, admin, config::Config, http::RunningServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use support::{initialized, pairing_request, post_json, request};

const BEFORE: &[u8] = b"# Synthetic before\r\n";
const PROPOSED: &[u8] = b"# Synthetic proposed\n";
const OBSERVED: &[u8] = b"\xef\xbb\xbf# Synthetic external bytes\r\n";

struct Fixture {
    _directory: tempfile::TempDir,
    config: Config,
    server: RunningServer,
    paired: PairResponse,
    document: Document,
    operation_id: OperationId,
    headers: Vec<(String, String)>,
}

impl Fixture {
    async fn quarantined() -> Self {
        let (directory, config) = initialized().await;
        std::fs::write(config.workspace.notes_root.join("Example.md"), BEFORE).unwrap();
        let mut workspace =
            Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
                .await
                .unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let server = RunningServer::start(config.clone()).await.unwrap();
        let (pair, _, _) = pairing_request(&config).await;
        let response = post_json(server.local_addr(), &serde_json::to_vec(&pair).unwrap()).await;
        assert_eq!(response.status, 200);
        let paired: PairResponse = serde_json::from_slice(&response.body).ok().unwrap();
        let operation_id: OperationId = uuid::Uuid::now_v7().to_string().parse().unwrap();
        let headers: Vec<(String, String)> = vec![
            (
                "Authorization".into(),
                format!("Bearer {}", paired.device_token),
            ),
            (
                "X-Workspace-Epoch".into(),
                paired.workspace_epoch.to_string(),
            ),
            ("X-Device-Stream".into(), paired.device_stream_id.clone()),
            ("X-Operation-Seq".into(), "1".into()),
            ("Idempotency-Key".into(), operation_id.to_string()),
            (
                "If-Match".into(),
                format!("\"{}\"", document.content_rev.as_str()),
            ),
            (
                "Content-Digest".into(),
                format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(PROPOSED))),
            ),
        ];
        let mut database = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(config.workspace.state_root.join("state.db")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_admin_recovery_fixture BEFORE UPDATE OF content_rev ON nodes BEGIN SELECT RAISE(ABORT, 'synthetic finalization failure'); END;")
            .execute(&mut database).await.unwrap();
        let borrowed: Vec<_> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let failed = request(
            server.local_addr(),
            "PUT",
            &format!("/v1/nodes/{}/content", document.node_id),
            &borrowed,
            PROPOSED,
        )
        .await;
        assert_eq!(failed.status, 503);
        sqlx::query("DROP TRIGGER fail_admin_recovery_fixture")
            .execute(&mut database)
            .await
            .unwrap();
        database.close().await.unwrap();
        server.shutdown().await.unwrap();
        std::fs::write(config.workspace.notes_root.join("Example.md"), OBSERVED).unwrap();
        let server = RunningServer::start(config.clone()).await.unwrap();
        Self {
            _directory: directory,
            config,
            server,
            paired,
            document,
            operation_id,
            headers,
        }
    }

    async fn admin(&self, command: AdminRequest) -> AdminResponse {
        admin::request(&self.config.server.admin_socket, &command)
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> support::HttpResponse {
        request(
            self.server.local_addr(),
            "GET",
            path,
            &[(
                "Authorization",
                &format!("Bearer {}", self.paired.device_token),
            )],
            &[],
        )
        .await
    }
}

fn result(response: AdminResponse) -> Value {
    let AdminResponse::Ok { result } = response else {
        panic!("local recovery command must succeed")
    };
    serde_json::to_value(result).unwrap()
}

#[tokio::test]
async fn local_admin_resolution_preserves_reads_and_records_a_replayable_terminal_conflict() {
    let fixture = Fixture::quarantined().await;
    let info = fixture.get("/v1/server-info").await;
    assert_eq!(info.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&info.body).unwrap()["ready"],
        false
    );
    let content_path = format!("/v1/nodes/{}/content", fixture.document.node_id);
    let content = fixture.get(&content_path).await;
    assert_eq!(content.status, 200);
    assert!(content.body == BEFORE);
    let page = result(
        fixture
            .admin(AdminRequest::RecoveryList {
                after_operation_id: None,
                limit: 20,
            })
            .await,
    );
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        page["entries"][0]["operation_id"],
        fixture.operation_id.to_string()
    );
    assert_eq!(page["entries"][0]["resolved"], false);
    let detail = result(
        fixture
            .admin(AdminRequest::RecoveryInspect {
                operation_id: fixture.operation_id,
                after_hash: None,
            })
            .await,
    );
    assert_eq!(detail["paths"], json!(["Example.md"]));
    assert_eq!(detail["summary"]["retained_objects"], "3");
    assert_eq!(detail["objects_truncated"], false);
    assert!(detail["next_after_hash"].is_null());
    let first_hash = detail["objects"][0]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let remaining = result(
        fixture
            .admin(AdminRequest::RecoveryInspect {
                operation_id: fixture.operation_id,
                after_hash: Some(first_hash.into()),
            })
            .await,
    );
    assert_eq!(remaining["objects"].as_array().unwrap().len(), 2);
    assert_eq!(
        remaining["objects"],
        json!(detail["objects"].as_array().unwrap()[1..])
    );
    let resolved = result(
        fixture
            .admin(AdminRequest::RecoveryResolve {
                operation_id: fixture.operation_id,
            })
            .await,
    );
    assert_eq!(
        resolved,
        json!({ "kind": "recovery_resolved", "operation_id": fixture.operation_id, "ready": true })
    );
    assert_eq!(
        result(
            fixture
                .admin(AdminRequest::RecoveryResolve {
                    operation_id: fixture.operation_id
                })
                .await
        ),
        resolved
    );
    let content = fixture.get(&content_path).await;
    assert_eq!(content.status, 200);
    assert!(content.body == OBSERVED);
    assert!(
        std::fs::read(fixture.config.workspace.notes_root.join("Example.md")).unwrap() == OBSERVED
    );
    let operation_path = format!("/v1/operations/{}", fixture.operation_id);
    let found = fixture.get(&operation_path).await;
    assert_eq!(found.status, 200);
    let receipt: Value = serde_json::from_slice(&found.body).unwrap();
    assert_eq!(receipt["status"], "recovery_conflict");
    assert!(receipt.get("commit_seq").is_none());
    let borrowed: Vec<_> = fixture
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let replay = request(
        fixture.server.local_addr(),
        "PUT",
        &content_path,
        &borrowed,
        PROPOSED,
    )
    .await;
    assert_eq!(replay.status, 409);
    let error: Value = serde_json::from_slice(&replay.body).unwrap();
    assert_eq!(error["code"], "recovery_conflict");
    assert_eq!(error["operation"], receipt);
    let ack = request(
        fixture.server.local_addr(),
        "POST",
        "/v1/operation-stream/ack",
        &[
            (
                "Authorization",
                &format!("Bearer {}", fixture.paired.device_token),
            ),
            ("Content-Type", "application/json"),
        ],
        &serde_json::to_vec(&json!({
            "workspace_epoch": fixture.paired.workspace_epoch,
            "device_stream_id": fixture.paired.device_stream_id,
            "ack_through": "1",
        }))
        .unwrap(),
    )
    .await;
    assert_eq!(ack.status, 200);
    assert_eq!(fixture.get(&operation_path).await.status, 410);
    let retained = result(
        fixture
            .admin(AdminRequest::RecoveryInspect {
                operation_id: fixture.operation_id,
                after_hash: None,
            })
            .await,
    );
    assert_eq!(retained["summary"]["resolved"], true);
    assert_eq!(retained["summary"]["retained_objects"], "3");
    assert!(matches!(
        fixture
            .admin(AdminRequest::RecoveryRelease {
                operation_id: fixture.operation_id
            })
            .await,
        AdminResponse::Ok {
            result: AdminResult::Done {}
        }
    ));
    for command in [
        AdminRequest::RecoveryInspect {
            operation_id: fixture.operation_id,
            after_hash: None,
        },
        AdminRequest::RecoveryRelease {
            operation_id: fixture.operation_id,
        },
    ] {
        assert!(
            matches!(fixture.admin(command).await, AdminResponse::Error { code } if code == "not_found")
        );
    }
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn remote_owner_cannot_invoke_local_recovery_and_invalid_pages_are_rejected() {
    let fixture = Fixture::quarantined().await;
    let response = request(
        fixture.server.local_addr(),
        "POST",
        "/v1/admin/recovery-resolve",
        &[
            (
                "Authorization",
                &format!("Bearer {}", fixture.paired.device_token),
            ),
            ("Content-Type", "application/json"),
        ],
        &serde_json::to_vec(&json!({"operation_id": fixture.operation_id})).unwrap(),
    )
    .await;
    assert_eq!(response.status, 404);
    let release = fixture
        .admin(AdminRequest::RecoveryRelease {
            operation_id: fixture.operation_id,
        })
        .await;
    assert!(matches!(release, AdminResponse::Error { code } if code == "operation_pending"));
    for limit in [0, 21] {
        let response = fixture
            .admin(AdminRequest::RecoveryList {
                after_operation_id: None,
                limit,
            })
            .await;
        assert!(matches!(response, AdminResponse::Error { code } if code == "invalid_request"));
    }
    let content = fixture
        .get(&format!("/v1/nodes/{}/content", fixture.document.node_id))
        .await;
    assert!(content.body == BEFORE);
    assert!(
        std::fs::read(fixture.config.workspace.notes_root.join("Example.md")).unwrap() == OBSERVED
    );
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_object_pages_expose_every_object_after_resolving_a_large_folder() {
    let mut fixture = Fixture::quarantined().await;
    fixture.server.shutdown().await.unwrap();
    let folder = fixture.config.workspace.notes_root.join("Example.md");
    std::fs::remove_file(&folder).unwrap();
    std::fs::create_dir(&folder).unwrap();
    for number in 0..65 {
        std::fs::write(
            folder.join(format!("{number:02}.md")),
            format!("# Synthetic child {number}\n"),
        )
        .unwrap();
    }
    fixture.server = RunningServer::start(fixture.config.clone()).await.unwrap();
    let started = std::time::Instant::now();
    let response = admin::request(
        &fixture.config.server.admin_socket,
        &AdminRequest::RecoveryResolve {
            operation_id: fixture.operation_id,
        },
    )
    .await;
    if response.is_err() {
        fixture.server.shutdown().await.unwrap();
        panic!("a supported folder must resolve over the local admin socket");
    }
    let resolved = result(response.unwrap());
    eprintln!(
        "local recovery resolution elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert_eq!(resolved["ready"], true);
    let first = result(
        fixture
            .admin(AdminRequest::RecoveryInspect {
                operation_id: fixture.operation_id,
                after_hash: None,
            })
            .await,
    );
    assert_eq!(first["objects"].as_array().unwrap().len(), 64);
    assert_eq!(first["summary"]["retained_objects"], "68");
    assert_eq!(first["objects_truncated"], true);
    let cursor = first["next_after_hash"]
        .as_str()
        .expect("remaining objects need a cursor");
    assert_eq!(
        first["objects"].as_array().unwrap().last().unwrap(),
        &json!(format!("sha256:{cursor}"))
    );
    let second = result(
        fixture
            .admin(AdminRequest::RecoveryInspect {
                operation_id: fixture.operation_id,
                after_hash: Some(cursor.into()),
            })
            .await,
    );
    assert_eq!(second["objects"].as_array().unwrap().len(), 4);
    assert_eq!(second["objects_truncated"], false);
    assert!(second["next_after_hash"].is_null());
    let objects: std::collections::BTreeSet<_> = first["objects"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["objects"].as_array().unwrap())
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(objects.len(), 68);
    for number in 0..65 {
        let bytes = format!("# Synthetic child {number}\n");
        let hash: String = Sha256::digest(bytes.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(objects.contains(format!("sha256:{hash}").as_str()));
    }
    fixture.server.shutdown().await.unwrap();
}
