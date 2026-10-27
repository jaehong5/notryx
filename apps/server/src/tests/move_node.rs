use super::*;
use notryx_protocol::NodeKind;

fn request(rig: &Rig, node: &NodeSnapshot, parent: NodeId, name: &str, seq: u64) -> MoveNode {
    MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: seq,
        operation_id: op_id(),
        node_id: node.node_id,
        expected_meta_rev: node.meta_rev.clone(),
        expected_parent_id: node.parent_id.unwrap_or(node.node_id),
        parent_id: parent,
        name: name.into(),
    }
}

async fn create(
    rig: &mut Rig,
    parent: NodeId,
    name: &str,
    kind: NodeKind,
    seq: u64,
) -> NodeSnapshot {
    let command = CreateNode {
        upload_id: None,
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: seq,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: parent,
        name: name.into(),
        kind,
    };
    let Outcome::Created(node) = rig.workspace.create_node(command).await.unwrap().outcome else {
        panic!("expected created node")
    };
    node
}

#[tokio::test]
async fn rename_preserves_content_and_records_one_parent_namespace_change() {
    let mut rig = Rig::new().await;
    let before = NodeSnapshot::from(&rig.initial);
    let root = rig.workspace.state.root_id;
    let parent_before = rig.workspace.state.node(root).await.unwrap().unwrap();
    let bytes = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let command = request(&rig, &before, root, "Renamed.md", 1);
    let receipt = rig.workspace.move_node(command.clone()).await.unwrap();
    let Outcome::Moved(after) = &receipt.outcome else {
        panic!("expected moved node")
    };
    assert_eq!(after.node_id, before.node_id);
    assert_eq!(after.parent_id, before.parent_id);
    assert_eq!(after.name, "Renamed.md");
    assert_eq!(after.content_rev, before.content_rev);
    assert_eq!(after.content_hash, before.content_hash);
    assert_eq!(after.size, before.size);
    assert_ne!(after.meta_rev, before.meta_rev);
    assert_eq!(after.last_change_seq.get(), 2);
    let parent = rig.workspace.state.node(root).await.unwrap().unwrap();
    assert_eq!(
        parent.children_generation,
        parent_before.children_generation + 1
    );
    assert_eq!(parent.meta_rev, parent_before.meta_rev);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='2'")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 2);
    assert!(!rig.dir.path().join("notes/Document.md").exists());
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Renamed.md")).unwrap(),
        bytes
    );
    assert_eq!(
        rig.workspace.move_node(command.clone()).await.unwrap(),
        receipt
    );
    let record = rig
        .workspace
        .state
        .operation(rig.stream, command.operation_id)
        .await
        .unwrap();
    assert_eq!(record.node_etag, Some(after.etag().unwrap()));
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert_eq!(reopened.move_node(command).await.unwrap(), receipt);
    assert_eq!(reopened.read(before.node_id).await.unwrap().1, bytes);
}

#[tokio::test]
async fn folder_move_preserves_descendant_revisions_and_updates_both_parents_once() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let folder = create(&mut rig, root, "Source", NodeKind::Folder, 1).await;
    let destination = create(&mut rig, root, "Destination", NodeKind::Folder, 2).await;
    let child = create(&mut rig, folder.node_id, "Child", NodeKind::Folder, 3).await;
    let leaf = create(&mut rig, child.node_id, "Leaf.md", NodeKind::File, 4).await;
    let before = rig
        .workspace
        .state
        .node(folder.node_id)
        .await
        .unwrap()
        .unwrap();
    let child_before = rig
        .workspace
        .state
        .node(child.node_id)
        .await
        .unwrap()
        .unwrap();
    let source_before = rig.workspace.state.node(root).await.unwrap().unwrap();
    let command = request(&rig, &before, destination.node_id, "Moved", 5);
    let Outcome::Moved(moved) = rig.workspace.move_node(command).await.unwrap().outcome else {
        panic!("moved")
    };
    assert_eq!(moved.children_generation, before.children_generation);
    assert_eq!(
        rig.workspace.state.node(child.node_id).await.unwrap(),
        Some(child_before)
    );
    assert_eq!(
        rig.workspace.state.node(leaf.node_id).await.unwrap(),
        Some(leaf.clone())
    );
    for (parent, old_generation, old_meta) in [
        (
            root,
            source_before.children_generation,
            source_before.meta_rev,
        ),
        (destination.node_id, 0, destination.meta_rev),
    ] {
        let actual = rig.workspace.state.node(parent).await.unwrap().unwrap();
        assert_eq!(actual.children_generation, old_generation + 1);
        assert_eq!(actual.meta_rev, old_meta);
        assert_eq!(actual.last_change_seq, moved.last_change_seq);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='6'")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 3);
    assert!(
        rig.dir
            .path()
            .join("notes/Destination/Moved/Child/Leaf.md")
            .is_file()
    );
    assert!(!rig.dir.path().join("notes/Source").exists());
    let save = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 6,
        operation_id: op_id(),
        node_id: leaf.node_id,
        expected_content_rev: leaf.content_rev.unwrap(),
        content_hash: digest(b"after move\r\n"),
        bytes: b"after move\r\n".to_vec(),
    };
    rig.workspace.save(save).await.unwrap();
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Destination/Moved/Child/Leaf.md")).unwrap(),
        b"after move\r\n"
    );
    rig.workspace.close().await.unwrap();
    assert!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap()
            .ready()
    );
}

#[tokio::test]
async fn move_conflicts_are_terminal_without_touching_sources() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = NodeSnapshot::from(&rig.initial);
    let bytes = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let mut stale = request(&rig, &before, root, "Renamed.md", 1);
    stale.expected_meta_rev = MetaRevision::new("stale").unwrap();
    assert_eq!(
        rig.workspace.move_node(stale).await.unwrap().outcome,
        Outcome::MetadataPreconditionFailed {
            current: before.meta_rev.clone()
        }
    );
    let mut wrong_parent = request(&rig, &before, root, "Renamed.md", 2);
    wrong_parent.expected_parent_id = node_id();
    assert_eq!(
        rig.workspace.move_node(wrong_parent).await.unwrap().outcome,
        Outcome::ExpectedParentMismatch
    );
    fs::write(
        rig.dir.path().join("notes/Occupied.md"),
        b"external evidence",
    )
    .unwrap();
    let occupied = request(&rig, &before, root, "OCCUPIED.md", 3);
    assert_eq!(
        rig.workspace
            .move_node(occupied.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::DestinationExists
    );
    let root_node = rig.workspace.state.node(root).await.unwrap().unwrap();
    let root_move = request(&rig, &root_node, root, "Root", 4);
    assert_eq!(
        rig.workspace.move_node(root_move).await.unwrap().outcome,
        Outcome::RootImmutable
    );
    let not_folder = request(&rig, &before, before.node_id, "Other.md", 5);
    assert_eq!(
        rig.workspace.move_node(not_folder).await.unwrap().outcome,
        Outcome::ParentNotFolder
    );
    let mut missing = request(&rig, &before, root, "Missing.md", 6);
    missing.node_id = node_id();
    assert_eq!(
        rig.workspace.move_node(missing).await.unwrap().outcome,
        Outcome::NotFound
    );
    let parent = create(&mut rig, root, "Folder", NodeKind::Folder, 7).await;
    let child = create(&mut rig, parent.node_id, "Child", NodeKind::Folder, 8).await;
    let cycle = request(&rig, &parent, child.node_id, "Cycle", 9);
    assert_eq!(
        rig.workspace.move_node(cycle).await.unwrap().outcome,
        Outcome::CycleDetected
    );
    assert_eq!(
        rig.workspace.move_node(occupied).await.unwrap().outcome,
        Outcome::DestinationExists
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        bytes
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Occupied.md")).unwrap(),
        b"external evidence"
    );
    assert_eq!(
        rig.workspace.state.node(before.node_id).await.unwrap(),
        Some(before)
    );
}

#[tokio::test]
async fn exact_noop_checks_source_but_never_changes_node_head_or_parent() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = NodeSnapshot::from(&rig.initial);
    let parent = rig.workspace.state.node(root).await.unwrap().unwrap();
    let command = request(&rig, &before, root, &before.name, 1);
    assert_eq!(
        rig.workspace
            .move_node(command.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::Moved(before.clone())
    );
    assert_eq!(rig.workspace.state.node(root).await.unwrap(), Some(parent));
    assert_eq!(
        rig.workspace.state.node(before.node_id).await.unwrap(),
        Some(before.clone())
    );
    assert_eq!(rig.workspace.server_info().await.unwrap().head_seq.get(), 1);
    fs::write(rig.dir.path().join("notes/Document.md"), b"external edit").unwrap();
    assert_eq!(
        rig.workspace.move_node(command).await.unwrap().outcome,
        Outcome::Moved(before.clone())
    );
    let fresh = request(&rig, &before, root, &before.name, 2);
    assert_eq!(
        rig.workspace.move_node(fresh).await,
        Err(Error::ExternalChange)
    );
    let last: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(rig.stream.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(last, "1");
}

#[tokio::test]
async fn case_only_rename_excludes_only_the_source_entry() {
    let mut rig = Rig::new().await;
    let before = NodeSnapshot::from(&rig.initial);
    let root = rig.workspace.state.root_id;
    let command = request(&rig, &before, root, "document.MD", 1);
    let Outcome::Moved(after) = rig.workspace.move_node(command).await.unwrap().outcome else {
        panic!("moved")
    };
    assert!(!rig.dir.path().join("notes/Document.md").exists());
    assert!(rig.dir.path().join("notes/document.MD").is_file());
    assert_ne!(after.meta_rev, before.meta_rev);
    assert_eq!(after.content_rev, before.content_rev);
    rig.workspace.close().await.unwrap();
    assert!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap()
            .ready()
    );
}

#[tokio::test]
async fn move_recovery_finalizes_only_observed_before_or_proposed_state() {
    for boundary in [
        Phase::AfterAdmission,
        Phase::Storage(notryx_server_storage::StorageStage::AfterRename),
        Phase::BeforeFinish,
    ] {
        let mut rig = Rig::new().await;
        let root = rig.workspace.state.root_id;
        let destination = create(&mut rig, root, "Destination", NodeKind::Folder, 1).await;
        let before = NodeSnapshot::from(&rig.initial);
        let command = request(&rig, &before, destination.node_id, "Moved.md", 2);
        rig.workspace.hook = Some(Box::new(move |phase| {
            if phase == boundary {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        assert!(rig.workspace.move_node(command.clone()).await.is_err());
        assert!(!rig.workspace.ready());
        assert_eq!(
            rig.workspace
                .move_node(command.clone())
                .await
                .unwrap()
                .outcome,
            Outcome::Pending
        );
        rig.workspace.close().await.unwrap();
        let notes = rig.dir.path().join("notes");
        let state = rig.dir.path().join("state");
        let mut reopened = Workspace::open(&notes, &state).await.unwrap();
        assert!(reopened.ready());
        let outcome = reopened.move_node(command.clone()).await.unwrap();
        if boundary == Phase::AfterAdmission {
            assert_eq!(outcome.outcome, Outcome::NotApplied);
            assert!(notes.join("Document.md").exists());
            assert!(!notes.join("Destination/Moved.md").exists());
        } else {
            assert!(matches!(outcome.outcome, Outcome::Moved(_)));
            assert!(!notes.join("Document.md").exists());
            assert!(notes.join("Destination/Moved.md").exists());
        }
        assert_eq!(reopened.move_node(command).await.unwrap(), outcome);
    }
}

#[tokio::test]
async fn ambiguous_move_paths_preserve_both_entries_and_remain_pending() {
    for destination_exists in [false, true] {
        let mut rig = Rig::new().await;
        let root = rig.workspace.state.root_id;
        let before = NodeSnapshot::from(&rig.initial);
        let command = request(&rig, &before, root, "Moved.md", 1);
        rig.workspace.hook = Some(Box::new(|phase| {
            if phase == Phase::AfterAdmission {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        assert_eq!(
            rig.workspace.move_node(command.clone()).await,
            Err(Error::Storage)
        );
        let source_bytes = if destination_exists {
            fs::read(rig.dir.path().join("notes/Document.md")).unwrap()
        } else {
            b"third source state".to_vec()
        };
        fs::write(rig.dir.path().join("notes/Document.md"), &source_bytes).unwrap();
        if destination_exists {
            fs::write(
                rig.dir.path().join("notes/Moved.md"),
                b"destination evidence",
            )
            .unwrap();
        }
        rig.workspace.close().await.unwrap();
        for _ in 0..2 {
            let mut reopened =
                Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                    .await
                    .unwrap();
            assert!(!reopened.ready());
            assert_eq!(
                reopened.move_node(command.clone()).await.unwrap().outcome,
                Outcome::Pending
            );
            assert_eq!(
                fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
                source_bytes
            );
            if destination_exists {
                assert_eq!(
                    fs::read(rig.dir.path().join("notes/Moved.md")).unwrap(),
                    b"destination evidence"
                );
            }
            let serialized: String =
                sqlx::query_scalar("SELECT intent FROM write_intents WHERE operation_id=?")
                    .bind(command.operation_id.to_string())
                    .fetch_one(&mut reopened.state.connection)
                    .await
                    .unwrap();
            let intent: serde_json::Value = serde_json::from_str(&serialized).unwrap();
            let hashes = intent["observed_hashes"].as_array().unwrap();
            assert!(hashes.contains(&serde_json::Value::String(digest(&source_bytes))));
            if destination_exists {
                assert!(
                    hashes.contains(&serde_json::Value::String(digest(b"destination evidence")))
                );
            }
            for hash in hashes {
                assert!(reopened.files.read_object(hash.as_str().unwrap()).is_ok());
            }
            assert_eq!(
                reopened.state.node(before.node_id).await.unwrap(),
                Some(before.clone())
            );
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn move_sql_failure_rolls_back_admission_and_keeps_applied_work_pending() {
    for after_admission in [false, true] {
        let mut rig = Rig::new().await;
        let before = NodeSnapshot::from(&rig.initial);
        let command = request(&rig, &before, rig.workspace.state.root_id, "Moved.md", 1);
        let trigger = if after_admission {
            "CREATE TRIGGER injected BEFORE UPDATE OF meta_rev ON nodes BEGIN SELECT RAISE(ABORT,'synthetic failure'); END"
        } else {
            "CREATE TRIGGER injected BEFORE INSERT ON write_intents BEGIN SELECT RAISE(ABORT,'synthetic failure'); END"
        };
        sqlx::raw_sql(trigger)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(
            rig.workspace.move_node(command.clone()).await,
            Err(Error::Storage)
        );
        let pending = rig.workspace.state.pending().await.unwrap().len();
        assert_eq!(pending, usize::from(after_admission));
        let head = rig.workspace.server_info().await.unwrap().head_seq.get();
        assert_eq!(head, 1);
        let last: String =
            sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
                .bind(rig.stream.to_string())
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        assert_eq!(last, if after_admission { "1" } else { "0" });
        sqlx::raw_sql("DROP TRIGGER injected")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        rig.workspace.close().await.unwrap();
        let mut reopened =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(matches!(
            reopened.move_node(command).await.unwrap().outcome,
            Outcome::Moved(_)
        ));
        assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 2);
    }
}

#[tokio::test]
async fn schema_five_migration_preserves_receipts_and_requires_explicit_upgrade() {
    let mut rig = Rig::new().await;
    let saved = rig
        .workspace
        .save(rig.command(b"saved bytes\r\n"))
        .await
        .unwrap();
    let record = rig
        .workspace
        .state
        .operation(rig.stream, saved.operation_id)
        .await
        .unwrap();
    super::legacy::nodes_before_deletion(&mut rig.workspace.state.connection).await;
    sqlx::query("PRAGMA user_version=5")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let epoch = rig.workspace.epoch();
    rig.workspace.close().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&notes, &state).await.unwrap();
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    assert_eq!(reopened.epoch(), epoch);
    assert_eq!(
        reopened
            .state
            .operation(rig.stream, saved.operation_id)
            .await
            .unwrap(),
        record
    );
    assert_eq!(
        reopened.read(rig.initial.node_id).await.unwrap().1,
        b"saved bytes\r\n"
    );
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut reopened.state.connection)
        .await
        .unwrap();
    assert_eq!(version, 9);
}

#[tokio::test]
async fn move_rejects_cycles_and_descendant_path_overflow_before_admission() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let mut parent = root;
    for seq in 1..=3 {
        parent = create(&mut rig, parent, &"d".repeat(250), NodeKind::Folder, seq)
            .await
            .node_id;
    }
    let folder = create(&mut rig, root, "Folder", NodeKind::Folder, 4).await;
    create(
        &mut rig,
        folder.node_id,
        &"f".repeat(250),
        NodeKind::File,
        5,
    )
    .await;
    let current = rig
        .workspace
        .state
        .node(folder.node_id)
        .await
        .unwrap()
        .unwrap();
    let command = request(&rig, &current, parent, &"m".repeat(30), 6);
    assert_eq!(
        rig.workspace.move_node(command).await,
        Err(Error::InvalidRequest)
    );
    let last: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(rig.stream.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(last, "5");
    let valid = request(&rig, &current, root, "Moved", 6);
    assert!(matches!(
        rig.workspace.move_node(valid).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
}

#[tokio::test]
async fn namespace_generation_is_checked_against_historical_changes() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = NodeSnapshot::from(&rig.initial);
    let command = request(&rig, &before, root, "Moved.md", 1);
    rig.workspace.move_node(command).await.unwrap();
    let original: String =
        sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq='1' AND node_id=?")
            .bind(root.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    let mut corrupted: serde_json::Value = serde_json::from_str(&original).unwrap();
    corrupted["children_generation"] = "9".into();
    sqlx::query("UPDATE node_changes SET post_state=? WHERE seq='1' AND node_id=?")
        .bind(serde_json::to_string(&corrupted).unwrap())
        .bind(root.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let bytes = fs::read(rig.dir.path().join("notes/Moved.md")).unwrap();
    rig.workspace.close().await.unwrap();
    assert!(matches!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await,
        Err(Error::InvalidFormat)
    ));
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Moved.md")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn move_recovery_waits_for_both_parent_syncs_and_final_commit() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let destination = create(&mut rig, root, "Destination", NodeKind::Folder, 1).await;
    let before = NodeSnapshot::from(&rig.initial);
    let command = request(&rig, &before, destination.node_id, "Moved.md", 2);
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::Storage(notryx_server_storage::StorageStage::AfterRename) {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert!(rig.workspace.move_node(command.clone()).await.is_err());
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::load(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    for boundary in [
        Phase::Storage(notryx_server_storage::StorageStage::BeforeRecoverySourceDirectorySync),
        Phase::Storage(notryx_server_storage::StorageStage::BeforeRecoveryDestinationDirectorySync),
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
            reopened.move_node(command.clone()).await.unwrap().outcome,
            Outcome::Pending
        );
        assert_eq!(
            reopened.state.node(before.node_id).await.unwrap(),
            Some(before.clone())
        );
    }
    reopened.hook = None;
    reopened.recover().await.unwrap();
    assert!(matches!(
        reopened.move_node(command).await.unwrap().outcome,
        Outcome::Moved(_)
    ));
    assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 3);
}
