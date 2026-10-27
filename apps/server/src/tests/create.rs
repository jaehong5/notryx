use super::*;
use notryx_protocol::NodeKind;

fn request(rig: &Rig, parent: NodeId, name: &str, kind: NodeKind, sequence: u64) -> CreateNode {
    CreateNode {
        upload_id: None,
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: parent,
        name: name.into(),
        kind,
    }
}

#[tokio::test]
async fn creates_nested_files_and_folders_with_one_parent_and_child_commit() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let root_before = rig.workspace.node_metadata(root).await.unwrap();
    let folder_request = request(&rig, root, "Notes", NodeKind::Folder, 1);
    let folder_receipt = rig
        .workspace
        .create_node(folder_request.clone())
        .await
        .unwrap();
    let Outcome::Created(folder) = &folder_receipt.outcome else {
        panic!("expected created folder")
    };
    assert_eq!(folder.content_rev, None);
    assert_eq!(folder.content_hash, None);
    assert_eq!(folder.size, None);
    assert_eq!(folder.last_change_seq.get(), 2);
    let parent_after = rig.workspace.state.node(root).await.unwrap().unwrap();
    assert_eq!(parent_after.children_generation, 2);
    assert_eq!(parent_after.last_change_seq, folder.last_change_seq);
    let posts: Vec<String> =
        sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq='2'")
            .fetch_all(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(posts.len(), 2);
    let posts: Vec<serde_json::Value> = posts
        .into_iter()
        .map(|value| serde_json::from_str(&value).unwrap())
        .collect();
    assert!(
        posts
            .iter()
            .any(|post| post["node_id"] == folder.node_id.to_string()
                && post["content_rev"].is_null()
                && post["kind"] == "folder")
    );
    assert!(posts.iter().any(|post| post["node_id"] == root.to_string()
        && post["children_generation"] == "2"
        && post["meta_rev"] == root_before.meta_rev.as_str()));
    assert_eq!(
        rig.workspace.node_metadata(root).await.unwrap().meta_rev,
        root_before.meta_rev
    );
    assert!(rig.dir.path().join("notes/Notes").is_dir());
    let file_request = request(&rig, folder.node_id, "Nested.md", NodeKind::File, 2);
    let file_receipt = rig
        .workspace
        .create_node(file_request.clone())
        .await
        .unwrap();
    let Outcome::Created(file) = &file_receipt.outcome else {
        panic!("expected created file")
    };
    assert_eq!(file.size, Some(0));
    assert_eq!(rig.workspace.read(file.node_id).await.unwrap().1, b"");
    let bytes = b"\xef\xbb\xbf# nested\r\nraw\xff";
    let save = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: file.node_id,
        expected_content_rev: file.content_rev.clone().unwrap(),
        content_hash: digest(bytes),
        bytes: bytes.to_vec(),
    };
    assert!(matches!(
        rig.workspace.save(save.clone()).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Notes/Nested.md")).unwrap(),
        bytes
    );
    assert_eq!(
        rig.workspace
            .create_node(folder_request.clone())
            .await
            .unwrap(),
        folder_receipt
    );
    assert_eq!(
        rig.workspace
            .create_node(file_request.clone())
            .await
            .unwrap(),
        file_receipt
    );
    let head = rig.workspace.server_info().await.unwrap().head_seq;
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert_eq!(reopened.read(file.node_id).await.unwrap().1, bytes);
    assert_eq!(
        reopened.create_node(folder_request).await.unwrap(),
        folder_receipt
    );
    assert_eq!(
        reopened.create_node(file_request).await.unwrap(),
        file_receipt
    );
    assert!(matches!(
        reopened.save(save).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
    assert_eq!(reopened.server_info().await.unwrap().head_seq, head);
}

#[tokio::test]
async fn creation_conflicts_are_terminal_and_never_replace_existing_entries() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    fs::write(rig.dir.path().join("notes/Untracked.md"), b"keep external").unwrap();
    let external = request(&rig, root, "unTRACKED.md", NodeKind::File, 1);
    assert_eq!(
        rig.workspace
            .create_node(external.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::DestinationExists
    );
    let existing = request(&rig, root, "document.MD", NodeKind::Folder, 2);
    assert_eq!(
        rig.workspace
            .create_node(existing.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::DestinationExists
    );
    let mut reused = request(&rig, root, "Other.md", NodeKind::File, 3);
    reused.node_id = rig.initial.node_id;
    assert_eq!(
        rig.workspace
            .create_node(reused.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::NodeIdConflict
    );
    let parent_file = request(&rig, rig.initial.node_id, "Child.md", NodeKind::File, 4);
    assert_eq!(
        rig.workspace
            .create_node(parent_file)
            .await
            .unwrap()
            .outcome,
        Outcome::ParentNotFolder
    );
    let missing = request(&rig, node_id(), "Child.md", NodeKind::File, 5);
    assert_eq!(
        rig.workspace.create_node(missing).await.unwrap().outcome,
        Outcome::NotFound
    );
    assert_eq!(
        rig.workspace.create_node(external).await.unwrap().outcome,
        Outcome::DestinationExists
    );
    assert_eq!(
        rig.workspace.create_node(existing).await.unwrap().outcome,
        Outcome::DestinationExists
    );
    assert_eq!(
        rig.workspace.create_node(reused).await.unwrap().outcome,
        Outcome::NodeIdConflict
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Untracked.md")).unwrap(),
        b"keep external"
    );
    assert!(!rig.dir.path().join("notes/Other.md").exists());
    assert_eq!(rig.workspace.server_info().await.unwrap().head_seq.get(), 1);
}

#[tokio::test]
async fn fresh_tree_format_requires_schema_seven() {
    let mut rig = Rig::new().await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(version, 9);
}

#[tokio::test]
async fn create_recovery_distinguishes_absence_from_durable_created_entries() {
    for kind in [NodeKind::File, NodeKind::Folder] {
        for phase in [Phase::AfterAdmission, Phase::BeforeFinish] {
            let mut rig = Rig::new().await;
            let request = request(&rig, rig.workspace.state.root_id, "Created", kind, 1);
            rig.workspace.hook = Some(Box::new(move |current| {
                if current == phase {
                    Err(Error::Storage)
                } else {
                    Ok(())
                }
            }));
            assert_eq!(
                rig.workspace.create_node(request.clone()).await,
                Err(Error::Storage)
            );
            assert!(!rig.workspace.ready());
            rig.workspace.close().await.unwrap();
            let mut reopened =
                Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                    .await
                    .unwrap();
            assert!(reopened.ready());
            let outcome = reopened.create_node(request).await.unwrap().outcome;
            if phase == Phase::AfterAdmission {
                assert_eq!(outcome, Outcome::NotApplied);
                assert!(!rig.dir.path().join("notes/Created").exists());
                assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 1);
            } else {
                assert!(matches!(outcome, Outcome::Created(_)));
                assert!(rig.dir.path().join("notes/Created").exists());
                assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 2);
            }
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn unexpected_entries_after_create_admission_are_preserved_and_quarantined() {
    for kind in [NodeKind::File, NodeKind::Folder] {
        let mut rig = Rig::new().await;
        let request = request(&rig, rig.workspace.state.root_id, "Created", kind, 1);
        rig.workspace.hook = Some(Box::new(|phase| {
            if phase == Phase::AfterAdmission {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        assert_eq!(
            rig.workspace.create_node(request.clone()).await,
            Err(Error::Storage)
        );
        let evidence = if kind == NodeKind::Folder {
            fs::create_dir(rig.dir.path().join("notes/Created")).unwrap();
            rig.dir.path().join("notes/Created/Evidence.md")
        } else {
            rig.dir.path().join("notes/Created")
        };
        fs::write(&evidence, b"external evidence\r\n").unwrap();
        rig.workspace.close().await.unwrap();
        for _ in 0..2 {
            let mut reopened =
                Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                    .await
                    .unwrap();
            assert!(!reopened.ready());
            assert_eq!(
                reopened.create_node(request.clone()).await.unwrap().outcome,
                Outcome::Pending
            );
            assert_eq!(fs::read(&evidence).unwrap(), b"external evidence\r\n");
            assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 1);
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn create_absence_requires_parent_sync_before_terminal_receipt() {
    let mut rig = Rig::new().await;
    let request = request(
        &rig,
        rig.workspace.state.root_id,
        "Absent",
        NodeKind::Folder,
        1,
    );
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.create_node(request.clone()).await,
        Err(Error::Storage)
    );
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::load(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    for boundary in [
        Phase::Storage(notryx_server_storage::StorageStage::BeforeRecoveryDirectorySync),
        Phase::BeforeRecoveryFinish,
    ] {
        reopened.hook = Some(Box::new(move |phase| {
            if phase == boundary {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        assert!(reopened.recover().await.is_err());
        assert!(!reopened.ready());
        assert_eq!(
            reopened.create_node(request.clone()).await.unwrap().outcome,
            Outcome::Pending
        );
    }
    reopened.hook = None;
    reopened.recover().await.unwrap();
    assert_eq!(
        reopened.create_node(request).await.unwrap().outcome,
        Outcome::NotApplied
    );
}

#[tokio::test]
async fn v3_migration_preserves_retirement_and_a_pending_legacy_save() {
    let mut rig = Rig::new().await;
    let first = rig.command(b"first\n");
    let receipt = rig.workspace.save(first.clone()).await.unwrap();
    let Outcome::Saved(saved) = receipt.outcome else {
        panic!("saved")
    };
    rig.workspace
        .ack_operations(AckOperations {
            epoch: rig.workspace.epoch(),
            stream_id: rig.stream,
            ack_through: 1,
        })
        .await
        .unwrap();
    let second = SaveContent {
        operation_seq: 2,
        operation_id: op_id(),
        expected_content_rev: saved.content_rev,
        bytes: b"pending\r\n".to_vec(),
        content_hash: digest(b"pending\r\n"),
        ..first.clone()
    };
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.save(second.clone()).await,
        Err(Error::Storage)
    );
    sqlx::query("ALTER TABLE operation_receipts DROP COLUMN node_etag")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    super::legacy::nodes_before_deletion(&mut rig.workspace.state.connection).await;
    sqlx::query("PRAGMA user_version=3")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace.close().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&notes, &state).await.unwrap();
    let mut reopened = Workspace::load(&notes, &state).await.unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut reopened.state.connection)
        .await
        .unwrap();
    assert_eq!(version, 9);
    assert_eq!(
        reopened
            .state
            .operation(rig.stream, first.operation_id)
            .await,
        Err(Error::OperationRetired)
    );
    assert_eq!(reopened.save(first).await, Err(Error::OperationRetired));
    assert_eq!(
        reopened.save(second.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    reopened.recover().await.unwrap();
    assert_eq!(
        reopened.save(second).await.unwrap().outcome,
        Outcome::NotApplied
    );
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), b"first\n");
}

#[tokio::test]
async fn creation_rejects_invalid_and_oversized_paths_without_consuming_the_sequence() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    for name in [
        "../escape",
        ".git",
        ".MDW-hidden",
        "node_modules",
        "bad\\name",
        "CON",
        "x.",
        "e\u{301}",
    ] {
        let invalid = request(&rig, root, name, NodeKind::File, 1);
        assert_eq!(
            rig.workspace.create_node(invalid).await,
            Err(Error::InvalidRequest)
        );
    }
    let mut parent = root;
    for sequence in 1..=4 {
        let name = "x".repeat(if sequence == 1 { 254 } else { 255 });
        let create = request(&rig, parent, &name, NodeKind::Folder, sequence);
        parent = create.node_id;
        assert!(matches!(
            rig.workspace.create_node(create).await.unwrap().outcome,
            Outcome::Created(_)
        ));
    }
    let invalid = request(&rig, parent, "xx", NodeKind::File, 5);
    assert_eq!(
        rig.workspace.create_node(invalid).await,
        Err(Error::InvalidRequest)
    );
    let valid = request(&rig, parent, "x", NodeKind::File, 5);
    let id = valid.node_id;
    assert!(matches!(
        rig.workspace.create_node(valid).await.unwrap().outcome,
        Outcome::Created(_)
    ));
    assert_eq!(rig.workspace.state.node_path(id).await.unwrap().len(), 1024);
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert!(reopened.read(id).await.unwrap().1.is_empty());
}

#[tokio::test]
async fn create_sql_failures_preserve_atomic_admission_and_recover_published_nodes() {
    for admission in [true, false] {
        let mut rig = Rig::new().await;
        let root = rig.workspace.state.root_id;
        let request = request(&rig, root, "Created", NodeKind::Folder, 1);
        let trigger = if admission {
            "CREATE TRIGGER injected BEFORE INSERT ON operation_receipts BEGIN SELECT RAISE(FAIL, 'synthetic'); END"
        } else {
            "CREATE TRIGGER injected BEFORE INSERT ON node_changes BEGIN SELECT RAISE(FAIL, 'synthetic'); END"
        };
        sqlx::query(trigger)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(
            rig.workspace.create_node(request.clone()).await,
            Err(Error::Storage)
        );
        assert!(!rig.workspace.ready());
        assert_eq!(rig.workspace.server_info().await.unwrap().head_seq.get(), 1);
        assert_eq!(rig.dir.path().join("notes/Created").exists(), !admission);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM write_intents")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(count, i64::from(!admission));
        assert!(
            rig.workspace
                .state
                .node(request.node_id)
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("DROP TRIGGER injected")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        rig.workspace.close().await.unwrap();
        let mut reopened =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(reopened.ready());
        assert!(matches!(
            reopened.create_node(request).await.unwrap().outcome,
            Outcome::Created(_)
        ));
        assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 2);
    }
}

#[tokio::test]
async fn startup_rejects_cycles_missing_parents_and_file_parents_without_touching_sources() {
    for corruption in 0..3 {
        let mut rig = Rig::new().await;
        let root = rig.workspace.state.root_id;
        let create = request(&rig, root, "Folder", NodeKind::Folder, 1);
        let id = create.node_id;
        rig.workspace.create_node(create).await.unwrap();
        let parent = match corruption {
            0 => id,
            1 => node_id(),
            _ => rig.initial.node_id,
        };
        sqlx::query("PRAGMA foreign_keys=OFF")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        sqlx::query("UPDATE nodes SET parent_id=? WHERE node_id=?")
            .bind(parent.to_string())
            .bind(id.to_string())
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        let folders: Vec<String> =
            sqlx::query_scalar("SELECT node_id FROM nodes WHERE kind='folder'")
                .fetch_all(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        for folder in folders {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE parent_id=?")
                .bind(&folder)
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap();
            sqlx::query("UPDATE nodes SET children_generation=? WHERE node_id=?")
                .bind(count.to_string())
                .bind(&folder)
                .execute(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        }
        let ids: Vec<String> = sqlx::query_scalar("SELECT node_id FROM nodes")
            .fetch_all(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        for id in ids {
            let node = rig
                .workspace
                .state
                .node(id.parse().unwrap())
                .await
                .unwrap()
                .unwrap();
            sqlx::query("UPDATE node_changes SET post_state=? WHERE seq=? AND node_id=?")
                .bind(serde_json::to_string(&crate::state::node::node_post_state(&node)).unwrap())
                .bind(node.last_change_seq.get().to_string())
                .bind(&id)
                .execute(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        }
        let source = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
        rig.workspace.close().await.unwrap();
        assert!(matches!(
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await,
            Err(Error::InvalidFormat)
        ));
        assert_eq!(
            fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
            source
        );
        assert!(rig.dir.path().join("notes/Folder").is_dir());
    }
}

#[tokio::test]
async fn nested_save_recovery_keeps_pending_until_actual_parent_is_synced() {
    let mut rig = Rig::new().await;
    let folder = request(
        &rig,
        rig.workspace.state.root_id,
        "Folder",
        NodeKind::Folder,
        1,
    );
    let parent = folder.node_id;
    rig.workspace.create_node(folder).await.unwrap();
    let file = request(&rig, parent, "Nested.md", NodeKind::File, 2);
    let id = file.node_id;
    rig.workspace.create_node(file).await.unwrap();
    let document = rig.workspace.read(id).await.unwrap().0;
    let bytes = b"\xef\xbb\xbfupdated\r\n";
    let save = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: id,
        expected_content_rev: document.content_rev,
        content_hash: digest(bytes),
        bytes: bytes.to_vec(),
    };
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::Storage(notryx_server_storage::StorageStage::AfterRename) {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert!(rig.workspace.save(save.clone()).await.is_err());
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Folder/Nested.md")).unwrap(),
        bytes
    );
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::load(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    reopened.hook = Some(Box::new(|phase| {
        if phase == Phase::Storage(notryx_server_storage::StorageStage::BeforeRecoveryDirectorySync)
        {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert!(reopened.recover().await.is_err());
    assert!(!reopened.ready());
    assert_eq!(
        reopened.save(save.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    reopened.hook = None;
    reopened.recover().await.unwrap();
    assert!(matches!(
        reopened.save(save).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
    assert_eq!(reopened.read(id).await.unwrap().1, bytes);
}
