use notryx_protocol::{AdminRequest, AdminResult, NodeKind, PairRequest};
use notryx_server::{
    CreateNode, Outcome, Workspace, config::WorkspaceConfig, runtime::WorkspaceRuntime,
};
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn watcher_external_empty_folder_deletion_waits_then_preserves_history() {
    let directory = tempfile::tempdir().unwrap();
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
    let config = WorkspaceConfig {
        notes_root: notes.clone(),
        state_root: state,
        uploads: Default::default(),
    };
    let runtime = WorkspaceRuntime::start(config.clone()).await.unwrap();
    let handle = runtime.handle();
    let AdminResult::Status {
        workspace_id,
        owner_principal_id,
        ..
    } = handle.admin(AdminRequest::Status {}).await.unwrap()
    else {
        panic!("expected workspace status");
    };
    let AdminResult::Pairing { pairing_secret, .. } = handle
        .admin(AdminRequest::IssuePairing {
            principal_id: owner_principal_id,
        })
        .await
        .unwrap()
    else {
        panic!("expected pairing secret");
    };
    let paired = handle
        .pair(PairRequest {
            protocol: notryx_protocol::CONTRACT.into(),
            workspace_id,
            pairing_secret,
        })
        .await
        .unwrap();
    let info = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap();
    let folder_id = Uuid::now_v7().to_string().parse().unwrap();
    let create = CreateNode {
        epoch: paired.workspace_epoch,
        stream_id: paired.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        node_id: folder_id,
        parent_id: info.root_id,
        name: "Empty folder".into(),
        kind: NodeKind::Folder,
        upload_id: None,
    };
    let receipt = handle
        .create_node(paired.device_token.clone(), create.clone())
        .await
        .unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Created(_)));
    runtime.shutdown().await.unwrap();

    let runtime = WorkspaceRuntime::start(config).await.unwrap();
    let handle = runtime.handle();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.known_file_scan_incomplete() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("startup watcher registration and scan did not complete");
    let before = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let folder_before = handle
        .node_state(paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let parent_before = handle
        .node_state(paired.device_token.clone(), info.root_id)
        .await
        .unwrap();
    let path = notes.join("Empty folder");
    std::fs::remove_dir(&path).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let early = handle
        .server_info(paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let observed = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let head = handle
                .server_info(paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head > before {
                break head;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let outcome = if let Ok(head) = observed {
        Some((
            head,
            handle
                .changes(paired.device_token.clone(), before, Some(head))
                .await
                .unwrap(),
            handle
                .node_state(paired.device_token.clone(), folder_id)
                .await
                .unwrap(),
            handle
                .node_state(paired.device_token.clone(), info.root_id)
                .await
                .unwrap(),
            handle
                .trash(paired.device_token.clone(), None, 100)
                .await
                .unwrap(),
        ))
    } else {
        None
    };
    let receipt_after = handle
        .operation(paired.device_token.clone(), create.operation_id)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(
        early, before,
        "folder deletion must wait for the absence grace period"
    );
    let (head, changes, folder, parent, trash) =
        outcome.expect("external empty-folder deletion was not recorded");
    assert_eq!(head.get(), before.get() + 1);
    assert_eq!(folder.node_id, folder_id);
    assert!(folder.deleted);
    assert_ne!(folder.meta_rev, folder_before.meta_rev);
    assert_eq!(
        parent.children_generation.get(),
        parent_before.children_generation.get() + 1
    );
    assert_eq!(parent.meta_rev, parent_before.meta_rev);
    assert_eq!(changes.nodes.len(), 2);
    assert!(
        changes
            .nodes
            .iter()
            .any(|node| node.node_id == folder_id && node.deleted)
    );
    assert!(
        changes
            .nodes
            .iter()
            .any(|node| node.node_id == info.root_id)
    );
    assert!(
        trash
            .nodes
            .iter()
            .any(|node| node.node_id == folder_id && node.restorable)
    );
    assert_eq!(receipt_after, receipt);
    assert!(!path.exists());
}
