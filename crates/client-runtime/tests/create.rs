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
            kind: NodeKind::Folder,
            upload_id: None,
        },
    }
}

#[tokio::test]
async fn folder_create_retries_exact_request_and_rejects_payload_change() {
    let rig = rig().await;
    let request = mutation(&rig.editor, 1, "Folder").await;
    assert_eq!(
        rig.editor.create_operation(&request).await.unwrap(),
        OperationLookup::Missing
    );
    let receipt = rig.editor.create(&request).await.unwrap();
    assert!(
        matches!(&receipt, OperationReceipt::Created { operation_id, operation_seq, workspace_epoch, node, commit_seq }
        if *operation_id == request.operation_id
            && operation_seq.get() == 1
            && *workspace_epoch == request.binding.workspace_epoch
            && node.node_id == request.request.node_id
            && node.parent_id == Some(request.request.parent_id)
            && node.name == "Folder" && node.kind == NodeKind::Folder && !node.deleted
            && node.content_rev.is_none() && node.size.is_none()
            && commit_seq.get() > 0 && node.last_change_seq == *commit_seq)
    );
    assert!(rig.notes.join("Folder").is_dir());
    assert_eq!(
        std::fs::read_dir(rig.notes.join("Folder")).unwrap().count(),
        0
    );
    assert_eq!(rig.editor.create(&request).await.unwrap(), receipt);
    assert!(
        matches!(rig.editor.create_operation(&request).await.unwrap(),
        OperationLookup::Receipt(found) if *found == receipt)
    );
    let mut different = request.clone();
    different.request.name = "Different".into();
    assert_eq!(
        rig.editor.create(&different).await,
        Err(Error::IdempotencyMismatch)
    );
    assert!(!rig.notes.join("Different").exists());
    assert_eq!(rig.editor.create(&request).await.unwrap(), receipt);
    rig.editor.acknowledge(1).await.unwrap();
    assert_eq!(
        rig.editor.create_operation(&request).await.unwrap(),
        OperationLookup::Retired
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn name_collision_returns_terminal_receipt_without_replacing_existing_folder() {
    let rig = rig().await;
    let first = mutation(&rig.editor, 1, "Folder").await;
    assert!(matches!(
        rig.editor.create(&first).await.unwrap(),
        OperationReceipt::Created { .. }
    ));
    std::fs::write(
        rig.notes.join("Folder").join("keep.txt"),
        b"synthetic original\n",
    )
    .unwrap();
    let second = mutation(&rig.editor, 2, "Folder").await;
    let receipt = rig.editor.create(&second).await.unwrap();
    assert!(
        matches!(&receipt, OperationReceipt::DestinationExists { operation_id, operation_seq, workspace_epoch }
        if *operation_id == second.operation_id && operation_seq.get() == 2
            && *workspace_epoch == second.binding.workspace_epoch)
    );
    assert_eq!(rig.editor.create(&second).await.unwrap(), receipt);
    assert!(
        matches!(rig.editor.create_operation(&second).await.unwrap(),
        OperationLookup::Receipt(found) if *found == receipt)
    );
    assert_eq!(
        std::fs::read(rig.notes.join("Folder/keep.txt")).unwrap(),
        b"synthetic original\n"
    );
    let nodes = rig.editor.inventory(None).await.unwrap().nodes;
    assert!(
        nodes
            .iter()
            .any(|node| node.node_id == first.request.node_id && !node.deleted)
    );
    assert!(
        !nodes
            .iter()
            .any(|node| node.node_id == second.request.node_id)
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_is_denied_without_admitting_or_creating_a_folder() {
    let rig = rig().await;
    let request = mutation(&rig.reader, 1, "Denied").await;
    assert_eq!(rig.reader.create(&request).await, Err(Error::Forbidden));
    assert_eq!(
        rig.reader.create_operation(&request).await.unwrap(),
        OperationLookup::Missing
    );
    assert!(!rig.notes.join("Denied").exists());
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn foreign_binding_is_rejected_before_create_or_lookup() {
    let rig = rig().await;
    let original = mutation(&rig.editor, 1, "Bound").await;
    let mut foreign = original.clone();
    foreign.binding = rig.reader.binding().clone();
    assert_eq!(rig.editor.create(&foreign).await, Err(Error::Binding));
    assert_eq!(
        rig.editor.create_operation(&foreign).await,
        Err(Error::Binding)
    );
    assert!(!rig.notes.join("Bound").exists());
    assert_eq!(
        rig.editor.create_operation(&original).await.unwrap(),
        OperationLookup::Missing
    );
    assert!(matches!(
        rig.editor.create(&original).await.unwrap(),
        OperationReceipt::Created { .. }
    ));
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn create_rejects_folder_upload_before_sequence_is_admitted() {
    let rig = rig().await;
    let original = mutation(&rig.editor, 1, "Folder").await;
    let mut invalid = original.clone();
    invalid.request.upload_id = Some(Uuid::now_v7());
    assert_eq!(
        rig.editor.create(&invalid).await,
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        rig.editor.create_operation(&original).await.unwrap(),
        OperationLookup::Missing
    );
    assert!(matches!(
        rig.editor.create(&original).await.unwrap(),
        OperationReceipt::Created { .. }
    ));
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn abandon_missing_create_and_recover_existing_create_receipt() {
    let rig = rig().await;
    let cancelled = mutation(&rig.editor, 1, "Cancelled").await;
    let receipt = rig
        .editor
        .prepare_abandon_create(&cancelled)
        .unwrap()
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Abandoned { .. }));
    assert!(
        matches!(rig.editor.create_operation(&cancelled).await.unwrap(),
        OperationLookup::Receipt(found) if *found == receipt)
    );
    assert!(!rig.notes.join("Cancelled").exists());

    let created = mutation(&rig.editor, 2, "Created").await;
    let receipt = rig.editor.create(&created).await.unwrap();
    assert!(matches!(&receipt, OperationReceipt::Created { .. }));
    assert_eq!(
        rig.editor
            .prepare_abandon_create(&created)
            .unwrap()
            .await
            .unwrap(),
        receipt
    );
    assert!(rig.notes.join("Created").is_dir());
    rig.server.shutdown().await.unwrap();
}
