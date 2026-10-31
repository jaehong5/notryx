use super::*;
use crate::Workspace;
use crate::runtime::WorkspaceRuntime;

async fn fixture() -> (tempfile::TempDir, PathBuf, Config, crate::Document) {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = root.path().join("notes");
    let state = root.path().join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let path = root.path().join("server.toml");
    fs::write(&path, format!("[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n", root.path().join("admin.sock").display(), notes.display(), state.display())).unwrap();
    let config = Config::load(&path).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("Document.md"), b"before\r\n").unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    (root, path, config, document)
}

fn control() -> JobControl {
    JobControl {
        deadline: Instant::now() + JOB_LIMIT,
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

#[tokio::test]
async fn materialization_uses_snapshot_after_live_mutation_has_resumed() {
    let (root, path, config, document) = fixture().await;
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let grant = workspace.issue_pairing(owner, now).await.unwrap();
    let device = workspace
        .pair(
            notryx_protocol::PairRequest {
                protocol: notryx_protocol::CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            now,
        )
        .await
        .unwrap();
    let folder_id = uuid::Uuid::now_v7().to_string().parse().unwrap();
    let nested_id = uuid::Uuid::now_v7().to_string().parse().unwrap();
    for (seq, node_id, parent_id, name, kind) in [
        (
            1,
            folder_id,
            workspace.state.root_id,
            "Folder",
            notryx_protocol::NodeKind::Folder,
        ),
        (
            2,
            nested_id,
            folder_id,
            "Nested.md",
            notryx_protocol::NodeKind::File,
        ),
    ] {
        let created = workspace
            .create_node(crate::CreateNode {
                epoch: workspace.epoch(),
                stream_id: device.device_stream_id.parse().unwrap(),
                operation_seq: seq,
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                node_id,
                parent_id,
                name: name.into(),
                kind,
                upload_id: None,
            })
            .await
            .unwrap();
        assert!(matches!(created.outcome, crate::Outcome::Created(_)));
    }
    fs::write(
        config.workspace.notes_root.join("Deleted.md"),
        b"preserved trash\n",
    )
    .unwrap();
    let deleted = workspace.adopt_root_document("Deleted.md").await.unwrap();
    workspace
        .delete_node(crate::DeleteNode {
            epoch: workspace.epoch(),
            stream_id: device.device_stream_id.parse().unwrap(),
            operation_seq: 3,
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            node_id: deleted.node_id,
            expected_node_etag: crate::NodeSnapshot::from(&deleted).etag().unwrap(),
        })
        .await
        .unwrap();
    let deleted_state = workspace.node_state(deleted.node_id).await.unwrap();
    let expected_head = workspace.state.head_seq().await.unwrap();
    workspace.close().await.unwrap();
    let runtime = WorkspaceRuntime::test_start_without_known_file_watch(config.workspace.clone())
        .await
        .unwrap();
    let destination = root.path().join("backup");
    let prepared = Prepared::new(&path, &destination).unwrap();
    let snapshot = runtime
        .handle()
        .backup_snapshot(prepared.state.try_clone().unwrap())
        .await
        .unwrap();
    let bytes = b"after snapshot\n";
    runtime
        .handle()
        .save_content(
            device.device_token.clone(),
            crate::SaveContent {
                epoch: device.workspace_epoch,
                stream_id: device.device_stream_id.parse().unwrap(),
                operation_seq: 4,
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: hex(&Sha256::digest(bytes)),
                bytes: bytes.to_vec(),
            },
        )
        .await
        .unwrap();
    let result = prepared.finish(snapshot, control()).await.unwrap();
    assert!(
        matches!(result, AdminResult::BackupCreated { head_seq, .. } if head_seq == expected_head)
    );
    verify(&destination).unwrap();
    assert_eq!(
        fs::read(destination.join("notes/Document.md")).unwrap(),
        b"before\r\n"
    );
    assert_eq!(
        fs::read(config.workspace.notes_root.join("Document.md")).unwrap(),
        b"after snapshot\n"
    );
    let restored = root.path().join("restored");
    restore(&destination, &restored).await.unwrap();
    assert_eq!(
        fs::read(restored.join("notes/Folder/Nested.md")).unwrap(),
        b""
    );
    assert!(!restored.join("notes/Deleted.md").exists());
    assert_eq!(
        fs::read(
            restored
                .join("state/objects/sha256")
                .join(deleted.content_hash)
        )
        .unwrap(),
        b"preserved trash\n"
    );
    let mut restored_workspace = Workspace::open(&restored.join("notes"), &restored.join("state"))
        .await
        .unwrap();
    assert_eq!(
        restored_workspace
            .node_state(deleted.node_id)
            .await
            .unwrap(),
        deleted_state
    );
    assert!(
        restored_workspace
            .authenticate(&device.device_token, now)
            .await
            .is_err()
    );
    restored_workspace.close().await.unwrap();
    assert_eq!(
        fs::read(restored.join("notes/Document.md")).unwrap(),
        b"before\r\n"
    );
    assert!(runtime.handle().health().await.unwrap().ready);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_or_expired_copy_is_incomplete_and_does_not_block_next_backup() {
    let (root, path, config, _) = fixture().await;
    let runtime = WorkspaceRuntime::test_start_without_known_file_watch(config.workspace.clone())
        .await
        .unwrap();
    for expired in [false, true] {
        let destination = root
            .path()
            .join(if expired { "expired" } else { "cancelled" });
        let prepared = Prepared::new(&path, &destination).unwrap();
        let stage_path = root.path().join(&prepared.stage_name);
        let snapshot = runtime
            .handle()
            .backup_snapshot(prepared.state.try_clone().unwrap())
            .await
            .unwrap();
        let mut control = control();
        if expired {
            control.deadline = Instant::now();
        } else {
            control.cancelled.store(true, Ordering::Release);
        }
        let error = match prepared.finish(snapshot, control).await {
            Err(error) => error,
            Ok(_) => panic!("cancelled backup was published"),
        };
        assert_eq!(
            error.to_string(),
            if expired {
                "backup_timeout"
            } else {
                "backup_cancelled"
            }
        );
        assert!(!destination.exists());
        assert!(!stage_path.join("COMPLETE").exists());
        assert!(verify(&stage_path).is_err());
    }
    let handle = runtime.handle();
    let held = handle.backup_permit.clone().acquire_owned().await.unwrap();
    assert!(matches!(
        create(&handle, &path, &root.path().join("busy")).await,
        Err(RuntimeError::Busy)
    ));
    assert!(!root.path().join("busy").exists());
    drop(held);
    create(&handle, &path, &root.path().join("retry"))
        .await
        .unwrap();
    verify(&root.path().join("retry")).unwrap();
    assert!(handle.health().await.unwrap().ready);
    runtime.shutdown().await.unwrap();
}
