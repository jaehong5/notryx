use notryx_domain::NodeId;
use notryx_protocol::{AdminRequest, AdminResult, NodeKind, PairRequest, PairResponse};
use notryx_server::{
    CreateNode, OperationRecord, Outcome, SaveContent, Workspace, config::WorkspaceConfig,
    runtime::WorkspaceRuntime,
};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use uuid::Uuid;

struct Fixture {
    directory: tempfile::TempDir,
    runtime: WorkspaceRuntime,
    paired: PairResponse,
    folder: NodeId,
    root: NodeId,
    creation: OperationRecord,
    child: Option<(NodeId, String, OperationRecord)>,
}

impl Fixture {
    async fn new(with_child: bool) -> Self {
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
            notes_root: notes,
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
        let root = handle
            .server_info(paired.device_token.clone())
            .await
            .unwrap()
            .root_id;
        let folder = Uuid::now_v7().to_string().parse().unwrap();
        let creation = handle
            .create_node(
                paired.device_token.clone(),
                CreateNode {
                    epoch: paired.workspace_epoch,
                    stream_id: paired.device_stream_id.parse().unwrap(),
                    operation_seq: 1,
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                    node_id: folder,
                    parent_id: root,
                    name: "Folder".into(),
                    kind: NodeKind::Folder,
                    upload_id: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(&creation.outcome, Outcome::Created(_)));
        let child = if with_child {
            let node_id = Uuid::now_v7().to_string().parse().unwrap();
            let created = handle
                .create_node(
                    paired.device_token.clone(),
                    CreateNode {
                        epoch: paired.workspace_epoch,
                        stream_id: paired.device_stream_id.parse().unwrap(),
                        operation_seq: 2,
                        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                        node_id,
                        parent_id: folder,
                        name: "Child.md".into(),
                        kind: NodeKind::File,
                        upload_id: None,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(&created.outcome, Outcome::Created(_)));
            let before = handle
                .node_state(paired.device_token.clone(), node_id)
                .await
                .unwrap();
            let bytes = b"preserved child source\n";
            let hash: String = Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let saved = handle
                .save_content(
                    paired.device_token.clone(),
                    SaveContent {
                        epoch: paired.workspace_epoch,
                        stream_id: paired.device_stream_id.parse().unwrap(),
                        operation_seq: 3,
                        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                        node_id,
                        expected_content_rev: before.content_rev.unwrap(),
                        content_hash: hash.clone(),
                        bytes: bytes.to_vec(),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(&saved.outcome, Outcome::Saved(_)));
            Some((node_id, hash, saved))
        } else {
            None
        };
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
        assert!(!handle.known_file_watch_degraded());
        Self {
            directory,
            runtime,
            paired,
            folder,
            root,
            creation,
            child,
        }
    }

    fn notes(&self) -> std::path::PathBuf {
        self.directory.path().join("notes")
    }
}

#[tokio::test]
async fn recreated_empty_folder_during_grace_is_not_tombstoned() {
    let fixture = Fixture::new(false).await;
    let handle = fixture.runtime.handle();
    let folder_path = fixture.notes().join("Folder");
    let removed_at = Instant::now();
    std::fs::remove_dir(&folder_path).unwrap();
    tokio::time::timeout(Duration::from_millis(800), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not observe the removed folder within the grace period");
    tokio::time::sleep(Duration::from_millis(450).saturating_sub(removed_at.elapsed())).await;
    assert!(
        removed_at.elapsed() < Duration::from_secs(1),
        "test could not recreate the folder within the grace period"
    );
    std::fs::create_dir(&folder_path).unwrap();
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let folder = handle
        .node_state(fixture.paired.device_token.clone(), fixture.folder)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while handle.known_file_scan_incomplete() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recreated folder watch registration and scan did not complete");
    let before_child = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let child_path = folder_path.join("New.md");
    let child_source = b"new document in recreated folder\n";
    std::fs::write(&child_path, child_source).unwrap();
    let child = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let head = handle
                .server_info(fixture.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if head > before_child {
                let changes = handle
                    .changes(
                        fixture.paired.device_token.clone(),
                        before_child,
                        Some(head),
                    )
                    .await
                    .unwrap();
                if let Some(node) = changes.nodes.into_iter().find(|node| {
                    node.parent_id == Some(fixture.folder)
                        && node.name == "New.md"
                        && node.kind == NodeKind::File
                        && !node.deleted
                }) {
                    break node;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let child_content = if let Ok(node) = &child {
        Some(
            handle
                .content(fixture.paired.device_token.clone(), node.node_id, None)
                .await
                .unwrap()
                .1,
        )
    } else {
        None
    };
    let receipt = handle
        .operation(
            fixture.paired.device_token.clone(),
            fixture.creation.operation_id,
        )
        .await
        .unwrap();
    fixture.runtime.shutdown().await.unwrap();
    assert!(
        !folder.deleted,
        "a folder restored during the grace period was tombstoned"
    );
    assert_eq!(folder.node_id, fixture.folder);
    assert_eq!(receipt, fixture.creation);
    assert!(folder_path.is_dir());
    assert!(
        child.is_ok(),
        "the recreated folder did not discover its new Markdown file"
    );
    assert_eq!(child_content.as_deref(), Some(child_source.as_slice()));
    assert_eq!(std::fs::read(child_path).unwrap(), child_source);
}

#[tokio::test]
async fn moved_nonempty_folder_does_not_delete_parent_before_live_child() {
    let fixture = Fixture::new(true).await;
    let handle = fixture.runtime.handle();
    let (child_id, hash, saved) = fixture.child.as_ref().unwrap();
    let before = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let destination = fixture.directory.path().join("moved-outside-notes");
    std::fs::rename(fixture.notes().join("Folder"), &destination).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not observe the moved folder");
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let (after, folder, child) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let first = handle
                .server_info(fixture.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            let folder = handle
                .node_state(fixture.paired.device_token.clone(), fixture.folder)
                .await
                .unwrap();
            let child = handle
                .node_state(fixture.paired.device_token.clone(), *child_id)
                .await
                .unwrap();
            let last = handle
                .server_info(fixture.paired.device_token.clone())
                .await
                .unwrap()
                .head_seq;
            if first == last {
                break (last, folder, child);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("metadata kept changing while the folder state was read");
    let history = if after > before {
        Some(
            handle
                .changes(fixture.paired.device_token.clone(), before, Some(after))
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let receipt = handle
        .operation(fixture.paired.device_token.clone(), saved.operation_id)
        .await
        .unwrap();
    let retained = std::fs::read(
        fixture
            .directory
            .path()
            .join("state/objects/sha256")
            .join(hash),
    )
    .unwrap();
    fixture.runtime.shutdown().await.unwrap();
    assert!(
        !folder.deleted || child.deleted,
        "parent tombstone was published while its child remained live"
    );
    if folder.deleted {
        let changes = history.expect("deleted folder was not present in the journal");
        let folder_change = changes
            .nodes
            .iter()
            .find(|node| node.node_id == fixture.folder && node.deleted)
            .expect("folder tombstone missing from journal");
        let child_change = changes
            .nodes
            .iter()
            .find(|node| node.node_id == *child_id && node.deleted)
            .expect("child tombstone missing from journal");
        assert!(
            child_change.last_change_seq < folder_change.last_change_seq,
            "parent tombstone must follow the child tombstone"
        );
    }
    assert_eq!(receipt, *saved);
    assert_eq!(retained, b"preserved child source\n");
    assert_eq!(
        std::fs::read(destination.join("Child.md")).unwrap(),
        b"preserved child source\n"
    );
}

#[tokio::test]
async fn missing_root_marker_prevents_external_folder_tombstone() {
    let fixture = Fixture::new(false).await;
    let handle = fixture.runtime.handle();
    let before = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let folder_before = handle
        .node_state(fixture.paired.device_token.clone(), fixture.folder)
        .await
        .unwrap();
    let parent_before = handle
        .node_state(fixture.paired.device_token.clone(), fixture.root)
        .await
        .unwrap();
    let marker = fixture.notes().join(".mdw-workspace.json");
    let saved_marker = fixture.directory.path().join("saved-workspace-marker");
    std::fs::rename(&marker, &saved_marker).unwrap();
    std::fs::remove_dir(fixture.notes().join("Folder")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not observe the removed folder");
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let after = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let folder = handle
        .node_state(fixture.paired.device_token.clone(), fixture.folder)
        .await
        .unwrap();
    let parent = handle
        .node_state(fixture.paired.device_token.clone(), fixture.root)
        .await
        .unwrap();
    let receipt = handle
        .operation(
            fixture.paired.device_token.clone(),
            fixture.creation.operation_id,
        )
        .await
        .unwrap();
    fixture.runtime.shutdown().await.unwrap();
    assert_eq!(after, before);
    assert_eq!(folder, folder_before);
    assert_eq!(parent, parent_before);
    assert_eq!(receipt, fixture.creation);
    assert!(saved_marker.is_file());
}
