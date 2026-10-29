use notryx_client_runtime::{Error, RestoreMutation, Session};
use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, OperationReceipt, PairResponse, RestoreNodeRequest,
    WorkspaceRole,
};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;

async fn grant(config: &Config, principal_id: String) -> (String, String) {
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
        panic!("pairing")
    };
    (workspace_id.to_string(), pairing_secret)
}

struct Rig {
    _directory: tempfile::TempDir,
    notes: std::path::PathBuf,
    server: RunningServer,
    config: Config,
    editor: Session,
    reader: Session,
    request: RestoreNodeRequest,
    node_id: notryx_domain::NodeId,
}

async fn rig() -> Rig {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    std::fs::write(notes.join("example.md"), b"synthetic original\n").unwrap();
    let file = workspace.adopt_root_document("example.md").await.unwrap();
    workspace.close().await.unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: notes.clone(),
            state_root: state,
            uploads: Default::default(),
        },
    };
    let server = RunningServer::start(config.clone()).await.unwrap();
    let AdminResponse::Ok {
        result: AdminResult::Status {
            owner_principal_id, ..
        },
    } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
        .await
        .unwrap()
    else {
        panic!("status")
    };
    let (workspace_id, secret) = grant(&config, owner_principal_id).await;
    let origin = format!("http://{}", server.local_addr());
    let http = reqwest::Client::new();
    let owner: PairResponse = http
        .post(format!("{origin}/v1/pair"))
        .json(
            &json!({"protocol":"mdw-sync/1.2","workspace_id":workspace_id,"pairing_secret":secret}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = http
        .get(format!("{origin}/v1/nodes/{}", file.node_id))
        .bearer_auth(&owner.device_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let node: Value = response.json().await.unwrap();
    let response = http
        .delete(format!("{origin}/v1/nodes/{}", file.node_id))
        .bearer_auth(&owner.device_token)
        .header("X-Workspace-Epoch", owner.workspace_epoch.to_string())
        .header("X-Device-Stream", &owner.device_stream_id)
        .header("X-Operation-Seq", "1")
        .header("Idempotency-Key", Uuid::now_v7().to_string())
        .header("If-Match", node["node_etag"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let (workspace_id, secret) = grant(&config, owner.principal_id.to_string()).await;
    let editor = Session::pair(&origin, workspace_id.parse().unwrap(), &secret)
        .await
        .unwrap();
    let trash = editor.trash(None, 100).await.unwrap();
    let tombstone = trash
        .nodes
        .iter()
        .find(|node| node.node_id == file.node_id)
        .unwrap();
    let request = RestoreNodeRequest {
        expected_meta_rev: tombstone.meta_rev.clone(),
        parent_id: tombstone.parent_id,
        name: "restored.md".into(),
    };
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = admin::request(
        &config.server.admin_socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Reader,
        },
    )
    .await
    .unwrap()
    else {
        panic!("reader")
    };
    let (workspace_id, secret) = grant(&config, principal_id).await;
    let reader = Session::pair(&origin, workspace_id.parse().unwrap(), &secret)
        .await
        .unwrap();
    Rig {
        _directory: directory,
        notes,
        server,
        config,
        editor,
        reader,
        request,
        node_id: file.node_id,
    }
}

#[tokio::test]
async fn real_reader_permission_denial_differs_from_device_revocation() {
    let rig = rig().await;
    let request = mutation(&rig.reader, &rig);
    let denied = rig.reader.restore(&request).await.unwrap_err();
    assert_eq!(format!("{denied:?}"), "Forbidden");
    assert!(!rig.notes.join("restored.md").exists());
    admin::request(
        &rig.config.server.admin_socket,
        &AdminRequest::RevokeDevice {
            device_id: rig.reader.binding().device_id.to_string(),
        },
    )
    .await
    .unwrap();
    let revoked = rig.reader.restore(&request).await.unwrap_err();
    assert_eq!(format!("{revoked:?}"), "Unauthorized");
    assert_eq!(rig.editor.trash(None, 100).await.unwrap().nodes.len(), 1);
    rig.server.shutdown().await.unwrap();
}
fn mutation(session: &Session, rig: &Rig) -> RestoreMutation {
    RestoreMutation {
        binding: session.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: 1,
        node_id: rig.node_id,
        request: rig.request.clone(),
    }
}

#[tokio::test]
async fn editor_restores_real_tombstone_and_retries_same_receipt() {
    let rig = rig().await;
    let request = mutation(&rig.editor, &rig);
    let receipt = rig.editor.restore(&request).await.unwrap();
    assert!(
        matches!(&receipt, OperationReceipt::Restored { node, .. } if node.node_id == rig.node_id && node.name == "restored.md")
    );
    assert_eq!(
        std::fs::read(rig.notes.join("restored.md")).unwrap(),
        b"synthetic original\n"
    );
    assert_eq!(rig.editor.restore(&request).await.unwrap(), receipt);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_cannot_restore_real_tombstone() {
    let rig = rig().await;
    let request = mutation(&rig.reader, &rig);
    assert_eq!(rig.reader.restore(&request).await, Err(Error::Forbidden));
    assert!(!rig.notes.join("restored.md").exists());
    assert_eq!(rig.editor.trash(None, 100).await.unwrap().nodes.len(), 1);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_operation_lookup_recovers_response_and_detects_retirement() {
    let rig = rig().await;
    let request = mutation(&rig.editor, &rig);
    assert!(matches!(
        rig.editor.restore_operation(&request).await.unwrap(),
        notryx_client_runtime::OperationLookup::Missing
    ));
    let receipt = rig.editor.restore(&request).await.unwrap();
    let recovered = rig.editor.restore_operation(&request).await.unwrap();
    assert!(
        matches!(recovered, notryx_client_runtime::OperationLookup::Receipt(found) if *found == receipt)
    );
    rig.editor.acknowledge(1).await.unwrap();
    assert!(matches!(
        rig.editor.restore_operation(&request).await.unwrap(),
        notryx_client_runtime::OperationLookup::Retired
    ));
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_abandons_unadmitted_restore_without_changing_files() {
    let rig = rig().await;
    let request = mutation(&rig.reader, &rig);
    assert_eq!(rig.reader.restore(&request).await, Err(Error::Forbidden));
    assert!(matches!(
        rig.reader.restore_operation(&request).await.unwrap(),
        notryx_client_runtime::OperationLookup::Missing
    ));
    let receipt = rig.reader.abandon_restore(&request).await.unwrap();
    assert!(matches!(receipt, OperationReceipt::Abandoned { .. }));
    assert_eq!(rig.reader.abandon_restore(&request).await.unwrap(), receipt);
    assert!(
        matches!(rig.reader.restore_operation(&request).await.unwrap(), notryx_client_runtime::OperationLookup::Receipt(found) if *found == receipt)
    );
    rig.reader.acknowledge(1).await.unwrap();
    assert!(matches!(
        rig.reader.restore_operation(&request).await.unwrap(),
        notryx_client_runtime::OperationLookup::Retired
    ));
    assert!(!rig.notes.join("restored.md").exists());
    assert_eq!(rig.editor.trash(None, 100).await.unwrap().nodes.len(), 1);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn abandon_after_admitted_restore_returns_original_receipt() {
    let rig = rig().await;
    let request = mutation(&rig.editor, &rig);
    let original = rig.editor.restore(&request).await.unwrap();
    let result = rig.editor.abandon_restore(&request).await.unwrap();
    assert_eq!(result, original);
    assert_eq!(
        std::fs::read(rig.notes.join("restored.md")).unwrap(),
        b"synthetic original\n"
    );
    rig.server.shutdown().await.unwrap();
}
