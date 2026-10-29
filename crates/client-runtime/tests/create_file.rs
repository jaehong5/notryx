use notryx_client_runtime::{CreateMutation, Error, OperationLookup, Session};
use notryx_protocol::{
    AdminRequest, AdminResponse, AdminResult, CreateNodeRequest, NodeKind, OperationReceipt,
    WorkspaceRole,
};
use notryx_server::{
    Workspace, admin,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
use uuid::Uuid;

async fn pair(config: &Config, origin: &str, principal_id: String) -> Session {
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
    Session::pair(origin, workspace_id, &pairing_secret)
        .await
        .unwrap()
}

struct Rig {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    server: RunningServer,
    editor: Session,
    reader: Session,
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
    Workspace::initialize(&notes, &state)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
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
    let origin = format!("http://{}", server.local_addr());
    let editor = pair(&config, &origin, owner_principal_id).await;
    let reader = pair(&config, &origin, principal_id).await;
    Rig {
        _directory: directory,
        notes,
        server,
        editor,
        reader,
    }
}

async fn mutation(session: &Session, sequence: u64, name: &str) -> CreateMutation {
    CreateMutation {
        binding: session.binding().clone(),
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        operation_seq: sequence,
        request: CreateNodeRequest {
            node_id: Uuid::now_v7().to_string().parse().unwrap(),
            parent_id: session.server_info().await.unwrap().root_id,
            name: name.into(),
            kind: NodeKind::File,
            upload_id: None,
        },
    }
}

#[tokio::test]
async fn empty_markdown_create_and_retry_against_real_server() {
    let rig = rig().await;
    let request = mutation(&rig.editor, 1, "New.md").await;
    assert_eq!(
        rig.editor.create_operation(&request).await.unwrap(),
        OperationLookup::Missing
    );
    let receipt = rig.editor.create(&request).await.unwrap();
    assert!(
        matches!(&receipt, OperationReceipt::Created { node, commit_seq, .. }
        if node.node_id == request.request.node_id && node.parent_id == Some(request.request.parent_id)
            && node.kind == NodeKind::File && node.name == "New.md" && !node.deleted
            && node.size.as_deref() == Some("0")
            && node.content_hash.as_deref() == Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
            && node.content_rev.is_some() && node.content_etag.is_some()
            && node.last_change_seq == *commit_seq)
    );
    assert_eq!(std::fs::read(rig.notes.join("New.md")).unwrap(), b"");
    assert_eq!(rig.editor.create(&request).await.unwrap(), receipt);
    assert!(
        matches!(rig.editor.create_operation(&request).await.unwrap(), OperationLookup::Receipt(found) if *found == receipt)
    );
    let mut different = request.clone();
    different.request.name = "Other.md".into();
    assert_eq!(
        rig.editor.create(&different).await,
        Err(Error::IdempotencyMismatch)
    );
    assert!(!rig.notes.join("Other.md").exists());
    rig.editor.acknowledge(1).await.unwrap();
    assert_eq!(
        rig.editor.create_operation(&request).await.unwrap(),
        OperationLookup::Retired
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn file_name_collision_preserves_existing_markdown_bytes() {
    let rig = rig().await;
    let first = mutation(&rig.editor, 1, "Existing.md").await;
    assert!(matches!(
        rig.editor.create(&first).await.unwrap(),
        OperationReceipt::Created { .. }
    ));
    std::fs::write(rig.notes.join("Existing.md"), b"# synthetic original\n").unwrap();
    let second = mutation(&rig.editor, 2, "Existing.md").await;
    assert!(matches!(
        rig.editor.create(&second).await.unwrap(),
        OperationReceipt::DestinationExists { .. }
    ));
    assert_eq!(
        std::fs::read(rig.notes.join("Existing.md")).unwrap(),
        b"# synthetic original\n"
    );
    assert!(
        !rig.editor
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .iter()
            .any(|node| node.node_id == second.request.node_id)
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_is_denied_without_creating_or_admitting_file() {
    let rig = rig().await;
    let request = mutation(&rig.reader, 1, "Denied.md").await;
    assert_eq!(rig.reader.create(&request).await, Err(Error::Forbidden));
    assert_eq!(
        rig.reader.create_operation(&request).await.unwrap(),
        OperationLookup::Missing
    );
    assert!(!rig.notes.join("Denied.md").exists());
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn file_create_rejects_foreign_binding_and_upload() {
    let rig = rig().await;
    let request = mutation(&rig.editor, 1, "Local.md").await;
    let mut foreign = request.clone();
    foreign.binding = rig.reader.binding().clone();
    assert_eq!(rig.editor.create(&foreign).await, Err(Error::Binding));
    let mut upload = request.clone();
    upload.request.upload_id = Some(Uuid::now_v7());
    assert_eq!(rig.editor.create(&upload).await, Err(Error::InvalidRequest));
    assert_eq!(
        rig.editor.create_operation(&request).await.unwrap(),
        OperationLookup::Missing
    );
    assert!(!rig.notes.join("Local.md").exists());
    rig.server.shutdown().await.unwrap();
}
