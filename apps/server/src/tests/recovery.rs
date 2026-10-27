use super::*;
use notryx_protocol::NodeKind;
use notryx_server_storage::StorageStage;

async fn conflict(bytes: Option<&[u8]>) -> (Rig, SaveContent) {
    let mut rig = Rig::new().await;
    let request = rig.command(b"proposed bytes\r\n");
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.save(request.clone()).await,
        Err(Error::Storage)
    );
    let path = rig.dir.path().join("notes/Document.md");
    match bytes {
        Some(bytes) => fs::write(path, bytes).unwrap(),
        None => fs::remove_file(path).unwrap(),
    }
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert!(!rig.workspace.ready());
    (rig, request)
}

#[tokio::test]
async fn explicit_resolution_adopts_current_bytes_and_preserves_all_evidence_after_ack() {
    let (mut rig, request) = conflict(Some(b"external\r\n")).await;
    let head = rig.workspace.state.head_seq().await.unwrap();
    let detail = rig
        .workspace
        .inspect_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(!detail.summary.resolved);
    assert_eq!(detail.paths, ["Document.md"]);
    assert_eq!(detail.summary.retained_objects, 3);
    assert!(detail.objects.contains(&rig.initial.content_hash));
    assert!(detail.objects.contains(&request.content_hash));
    assert!(detail.objects.contains(&digest(b"external\r\n")));
    assert_eq!(
        rig.workspace.release_recovery(request.operation_id).await,
        Err(Error::OperationPending)
    );
    let receipt = rig
        .workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert_eq!(receipt.outcome, Outcome::RecoveryConflict);
    assert!(rig.workspace.ready());
    let (node, bytes) = rig.workspace.read(rig.initial.node_id).await.unwrap();
    assert_eq!(bytes, b"external\r\n");
    assert_ne!(node.content_rev, rig.initial.content_rev);
    assert_eq!(node.meta_rev, rig.initial.meta_rev);
    assert_eq!(node.last_change_seq.get(), head.get() + 1);
    assert_eq!(rig.workspace.save(request.clone()).await.unwrap(), receipt);
    assert_eq!(
        rig.workspace
            .resolve_recovery(request.operation_id)
            .await
            .unwrap(),
        receipt
    );
    rig.workspace
        .ack_operations(AckOperations {
            epoch: request.epoch,
            stream_id: rig.stream,
            ack_through: 1,
        })
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.save(request.clone()).await,
        Err(Error::OperationRetired)
    );
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    let retained = rig
        .workspace
        .inspect_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(retained.summary.resolved);
    assert_eq!(retained.objects, detail.objects);
    rig.workspace
        .release_recovery(request.operation_id)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.inspect_recovery(request.operation_id).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"external\r\n"
    );
}

#[tokio::test]
async fn explicit_resolution_adopts_absence_without_resurrecting_the_document() {
    let (mut rig, request) = conflict(None).await;
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    let node = rig
        .workspace
        .state
        .node(rig.initial.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(node.deleted);
    assert_ne!(node.content_rev.as_ref(), Some(&rig.initial.content_rev));
    assert_ne!(node.meta_rev, rig.initial.meta_rev);
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await,
        Err(Error::NodeDeleted)
    );
    assert!(!rig.dir.path().join("notes/Document.md").exists());
    rig.workspace.close().await.unwrap();
    assert!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap()
            .ready()
    );
}

#[tokio::test]
async fn resolution_sql_failure_preserves_pending_and_rolls_back_metadata() {
    let (mut rig, request) = conflict(Some(b"external")).await;
    let head = rig.workspace.state.head_seq().await.unwrap();
    let before_nodes = rig.workspace.state.all_nodes().await.unwrap();
    let path = rig.dir.path().join("notes/Document.md");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("First.md"), b"first").unwrap();
    fs::write(path.join("Second.md"), b"second").unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_recovery BEFORE UPDATE ON operation_receipts WHEN NEW.pending=0 BEGIN SELECT RAISE(ABORT,'injected'); END;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    assert_eq!(
        rig.workspace.resolve_recovery(request.operation_id).await,
        Err(Error::Storage)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(rig.workspace.state.all_nodes().await.unwrap(), before_nodes);
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    assert_eq!(
        rig.workspace.save(request.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    assert!(
        !rig.workspace
            .inspect_recovery(request.operation_id)
            .await
            .unwrap()
            .summary
            .resolved
    );
    sqlx::query("DROP TRIGGER fail_recovery")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(rig.workspace.ready());
    assert!(
        rig.workspace
            .state
            .node(rig.initial.node_id)
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    assert_eq!(fs::read(path.join("First.md")).unwrap(), b"first");
    assert_eq!(fs::read(path.join("Second.md")).unwrap(), b"second");
}

#[tokio::test]
async fn resolution_fsync_failure_keeps_evidence_and_blocks_writes() {
    let (mut rig, request) = conflict(Some(b"external")).await;
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::Storage(StorageStage::BeforeRecoveryFileSync) {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.resolve_recovery(request.operation_id).await,
        Err(Error::Storage)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.save(request.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    rig.workspace.hook = None;
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().1,
        b"external"
    );
}

#[tokio::test]
async fn resolving_a_kind_change_uses_new_ids_and_valid_parent_order() {
    let (mut rig, request) = conflict(Some(b"initial conflict")).await;
    fs::remove_file(rig.dir.path().join("notes/Document.md")).unwrap();
    fs::create_dir(rig.dir.path().join("notes/Document.md")).unwrap();
    fs::create_dir(rig.dir.path().join("notes/Document.md/Nested")).unwrap();
    fs::write(
        rig.dir.path().join("notes/Document.md/Nested/Child.md"),
        b"child",
    )
    .unwrap();
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(
        rig.workspace
            .state
            .node(rig.initial.node_id)
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    let nodes = rig.workspace.state.all_nodes().await.unwrap();
    let folder = nodes
        .iter()
        .find(|n| !n.deleted && n.name == "Document.md")
        .unwrap();
    assert_eq!(folder.kind, NodeKind::Folder);
    assert_ne!(folder.node_id, rig.initial.node_id);
    let child = nodes
        .iter()
        .find(|n| !n.deleted && n.name == "Child.md")
        .unwrap();
    assert_eq!(rig.workspace.read(child.node_id).await.unwrap().1, b"child");
    rig.workspace.close().await.unwrap();
    assert!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap()
            .ready()
    );
}

#[tokio::test]
async fn unresolved_move_adopts_both_paths_without_guessing_identity() {
    let mut rig = Rig::new().await;
    let request = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: rig.initial.node_id,
        expected_meta_rev: rig.initial.meta_rev.clone(),
        expected_parent_id: rig.initial.parent_id,
        parent_id: rig.initial.parent_id,
        name: "Destination.md".into(),
    };
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.move_node(request.clone()).await,
        Err(Error::Storage)
    );
    fs::write(rig.dir.path().join("notes/Document.md"), b"source third").unwrap();
    fs::write(
        rig.dir.path().join("notes/Destination.md"),
        b"destination third",
    )
    .unwrap();
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().1,
        b"source third"
    );
    let nodes = rig.workspace.state.all_nodes().await.unwrap();
    let destination = nodes
        .iter()
        .find(|n| !n.deleted && n.name == "Destination.md")
        .unwrap();
    assert_ne!(destination.node_id, rig.initial.node_id);
    assert_eq!(
        rig.workspace.read(destination.node_id).await.unwrap().1,
        b"destination third"
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
async fn recovery_schema_requires_explicit_migration_and_preserves_legacy_conflict() {
    let (mut rig, request) = conflict(Some(b"external")).await;
    sqlx::raw_sql("DROP TABLE IF EXISTS uploads; DROP TABLE IF EXISTS recovery_objects; DROP TABLE IF EXISTS recovery_cases; PRAGMA user_version=7;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    rig.workspace.close().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&notes, &state).await.unwrap();
    rig.workspace = Workspace::open(&notes, &state).await.unwrap();
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace
            .inspect_recovery(request.operation_id)
            .await
            .unwrap()
            .summary
            .retained_objects,
        3
    );
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(rig.workspace.ready());
}

#[tokio::test]
async fn recovery_migration_collision_rolls_back_and_preserves_the_existing_table() {
    let mut rig = Rig::new().await;
    sqlx::raw_sql("DROP TABLE IF EXISTS uploads; DROP TABLE IF EXISTS recovery_objects; DROP TABLE IF EXISTS recovery_cases; CREATE TABLE recovery_cases(evidence TEXT); INSERT INTO recovery_cases VALUES('preserve'); PRAGMA user_version=7;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    rig.workspace.close().await.unwrap();
    let notes = rig.dir.path().join("notes");
    let state = rig.dir.path().join("state");
    assert!(Workspace::migrate(&notes, &state).await.is_err());
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(state.join("state.db"))
            .read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 7);
    let evidence: String = sqlx::query_scalar("SELECT evidence FROM recovery_cases")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(evidence, "preserve");
}

#[tokio::test]
async fn resolution_sigkill_before_and_after_commit_keeps_one_terminal_result() {
    for phase in [Phase::BeforeResolveFinish, Phase::AfterResolveFinish] {
        let (mut rig, request) = conflict(Some(b"current source")).await;
        let head = rig.workspace.state.head_seq().await.unwrap();
        fs::write(
            rig.dir.path().join("resolve-request.json"),
            serde_json::to_vec(&request.operation_id).unwrap(),
        )
        .unwrap();
        rig.workspace.close().await.unwrap();
        super::crash::kill_at(rig.dir.path(), phase, false);
        rig.workspace =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert_eq!(rig.workspace.ready(), phase == Phase::AfterResolveFinish);
        let receipt = rig
            .workspace
            .resolve_recovery(request.operation_id)
            .await
            .unwrap();
        assert_eq!(receipt.outcome, Outcome::RecoveryConflict);
        assert_eq!(
            rig.workspace.state.head_seq().await.unwrap().get(),
            head.get() + 1
        );
        assert_eq!(
            rig.workspace.read(rig.initial.node_id).await.unwrap().1,
            b"current source"
        );
        assert_eq!(rig.workspace.save(request).await.unwrap(), receipt);
    }
}

#[tokio::test]
async fn unsafe_links_and_name_collisions_keep_the_conflict_pending() {
    let (mut rig, request) = conflict(Some(b"external")).await;
    let source = rig.dir.path().join("notes/Document.md");
    fs::remove_file(&source).unwrap();
    std::os::unix::fs::symlink("../outside", &source).unwrap();
    fs::write(rig.dir.path().join("outside"), b"do not adopt").unwrap();
    assert!(
        rig.workspace
            .resolve_recovery(request.operation_id)
            .await
            .is_err()
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        fs::read(rig.dir.path().join("outside")).unwrap(),
        b"do not adopt"
    );
    fs::remove_file(&source).unwrap();
    fs::write(&source, b"external").unwrap();
    fs::write(rig.dir.path().join("notes/document.md"), b"collision").unwrap();
    assert!(
        rig.workspace
            .resolve_recovery(request.operation_id)
            .await
            .is_err()
    );
    assert_eq!(
        rig.workspace.save(request.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    fs::remove_file(rig.dir.path().join("notes/document.md")).unwrap();
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(rig.workspace.ready());
}

#[tokio::test]
async fn restored_before_state_settles_the_conflict_without_changing_revisions_or_head() {
    let (mut rig, request) = conflict(Some(b"external")).await;
    let original = rig
        .workspace
        .files
        .read_object(&rig.initial.content_hash)
        .unwrap();
    fs::write(rig.dir.path().join("notes/Document.md"), original).unwrap();
    let head = rig.workspace.state.head_seq().await.unwrap();
    assert_eq!(
        rig.workspace
            .resolve_recovery(request.operation_id)
            .await
            .unwrap()
            .outcome,
        Outcome::RecoveryConflict
    );
    assert_eq!(rig.workspace.state.head_seq().await.unwrap(), head);
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
}

#[tokio::test]
async fn observations_are_revalidated_after_durable_preparation() {
    let (mut rig, request) = conflict(Some(b"first external")).await;
    let path = rig.dir.path().join("notes/Document.md");
    rig.workspace.hook = Some(Box::new(move |phase| {
        if phase == Phase::BeforeResolveFinish {
            fs::write(&path, b"later external").unwrap();
        }
        Ok(())
    }));
    assert_eq!(
        rig.workspace.resolve_recovery(request.operation_id).await,
        Err(Error::ExternalChange)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    rig.workspace.hook = None;
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().1,
        b"later external"
    );
    let detail = rig
        .workspace
        .inspect_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(detail.objects.contains(&digest(b"first external")));
    assert!(detail.objects.contains(&digest(b"later external")));
}

#[tokio::test]
async fn create_delete_and_restore_conflicts_adopt_current_files_conservatively() {
    for kind in ["create", "delete", "restore"] {
        let mut rig = Rig::new().await;
        let mut sequence = 1;
        let mut before = NodeSnapshot::from(&rig.initial);
        if kind == "restore" {
            let deletion = DeleteNode {
                epoch: rig.workspace.epoch(),
                stream_id: rig.stream,
                operation_seq: sequence,
                operation_id: op_id(),
                node_id: before.node_id,
                expected_node_etag: before.etag().unwrap(),
            };
            before = match rig.workspace.delete_node(deletion).await.unwrap().outcome {
                Outcome::Deleted(node) => node,
                _ => panic!("deleted"),
            };
            sequence += 1;
        }
        let operation_id = op_id();
        let proposed_id = node_id();
        rig.workspace.hook = Some(Box::new(|phase| {
            if phase == Phase::AfterAdmission {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        let name = if kind == "create" {
            "Created.md"
        } else {
            "Document.md"
        };
        let result = match kind {
            "create" => {
                rig.workspace
                    .create_node(CreateNode {
                        upload_id: None,
                        epoch: rig.workspace.epoch(),
                        stream_id: rig.stream,
                        operation_seq: sequence,
                        operation_id,
                        node_id: proposed_id,
                        parent_id: rig.initial.parent_id,
                        name: name.into(),
                        kind: NodeKind::File,
                    })
                    .await
            }
            "delete" => {
                rig.workspace
                    .delete_node(DeleteNode {
                        epoch: rig.workspace.epoch(),
                        stream_id: rig.stream,
                        operation_seq: sequence,
                        operation_id,
                        node_id: before.node_id,
                        expected_node_etag: before.etag().unwrap(),
                    })
                    .await
            }
            _ => {
                rig.workspace
                    .restore_node(RestoreNode {
                        epoch: rig.workspace.epoch(),
                        stream_id: rig.stream,
                        operation_seq: sequence,
                        operation_id,
                        node_id: before.node_id,
                        expected_meta_rev: before.meta_rev.clone(),
                        parent_id: rig.initial.parent_id,
                        name: name.into(),
                    })
                    .await
            }
        };
        assert_eq!(result, Err(Error::Storage));
        fs::write(rig.dir.path().join("notes").join(name), b"external source").unwrap();
        rig.workspace.close().await.unwrap();
        rig.workspace =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(!rig.workspace.ready());
        assert_eq!(
            rig.workspace
                .resolve_recovery(operation_id)
                .await
                .unwrap()
                .outcome,
            Outcome::RecoveryConflict
        );
        let all = rig.workspace.state.all_nodes().await.unwrap();
        let current = all.iter().find(|n| !n.deleted && n.name == name).unwrap();
        if kind == "delete" {
            assert_eq!(current.node_id, before.node_id);
        }
        if kind == "restore" {
            assert_ne!(current.node_id, before.node_id);
        }
        if kind == "create" {
            assert_ne!(current.node_id, proposed_id);
        }
        assert_eq!(
            rig.workspace.read(current.node_id).await.unwrap().1,
            b"external source"
        );
        if kind == "create" {
            // Admission alone does not commit a node identity, as with NotApplied creation.
            let receipt = rig
                .workspace
                .create_node(CreateNode {
                    upload_id: None,
                    epoch: rig.workspace.epoch(),
                    stream_id: rig.stream,
                    operation_seq: sequence + 1,
                    operation_id: op_id(),
                    node_id: proposed_id,
                    parent_id: rig.initial.parent_id,
                    name: "Independent.md".into(),
                    kind: NodeKind::File,
                })
                .await
                .unwrap();
            let Outcome::Created(independent) = receipt.outcome else {
                panic!("created independent node")
            };
            assert_eq!(independent.node_id, proposed_id);
            assert_eq!(
                fs::read(rig.dir.path().join("notes/Independent.md")).unwrap(),
                b""
            );
        }
        rig.workspace.close().await.unwrap();
        assert!(
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap()
                .ready()
        );
    }
}

#[tokio::test]
async fn folder_move_conflict_reconciles_children_without_guessing_rename_identity() {
    let mut rig = Rig::new().await;
    let root = rig.initial.parent_id;
    let folder_id = node_id();
    let Outcome::Created(folder) = rig
        .workspace
        .create_node(CreateNode {
            upload_id: None,
            epoch: rig.workspace.epoch(),
            stream_id: rig.stream,
            operation_seq: 1,
            operation_id: op_id(),
            node_id: folder_id,
            parent_id: root,
            name: "Old".into(),
            kind: NodeKind::Folder,
        })
        .await
        .unwrap()
        .outcome
    else {
        panic!("folder")
    };
    let child_id = node_id();
    rig.workspace
        .create_node(CreateNode {
            upload_id: None,
            epoch: rig.workspace.epoch(),
            stream_id: rig.stream,
            operation_seq: 2,
            operation_id: op_id(),
            node_id: child_id,
            parent_id: folder_id,
            name: "Child.md".into(),
            kind: NodeKind::File,
        })
        .await
        .unwrap();
    let request = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: folder_id,
        expected_meta_rev: folder.meta_rev,
        expected_parent_id: root,
        parent_id: root,
        name: "New".into(),
    };
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.move_node(request.clone()).await,
        Err(Error::Storage)
    );
    fs::create_dir(rig.dir.path().join("notes/New")).unwrap();
    fs::write(rig.dir.path().join("notes/New/Child.md"), b"external child").unwrap();
    fs::remove_dir_all(rig.dir.path().join("notes/Old")).unwrap();
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert!(!rig.workspace.ready());
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    let nodes = rig.workspace.state.all_nodes().await.unwrap();
    assert!(
        nodes
            .iter()
            .find(|n| n.node_id == folder_id)
            .unwrap()
            .deleted
    );
    assert!(
        nodes
            .iter()
            .find(|n| n.node_id == child_id)
            .unwrap()
            .deleted
    );
    let child = nodes
        .iter()
        .find(|n| !n.deleted && n.name == "Child.md")
        .unwrap();
    assert_ne!(child.node_id, child_id);
    assert_eq!(
        rig.workspace.read(child.node_id).await.unwrap().1,
        b"external child"
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
async fn startup_rejects_corrupt_retained_objects_even_after_ack() {
    let (mut rig, request) = conflict(Some(b"external")).await;
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    rig.workspace
        .ack_operations(AckOperations {
            epoch: request.epoch,
            stream_id: rig.stream,
            ack_through: 1,
        })
        .await
        .unwrap();
    sqlx::query("INSERT INTO recovery_objects VALUES(?,?)")
        .bind(request.operation_id.to_string())
        .bind("f".repeat(64))
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace.close().await.unwrap();
    assert!(
        Workspace::migrate(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .is_err()
    );
    assert!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"external"
    );
}

#[tokio::test]
async fn migration_checks_legacy_observed_objects_before_changing_schema() {
    let (mut rig, _) = conflict(Some(b"external")).await;
    sqlx::raw_sql("DROP TABLE uploads; DROP TABLE recovery_objects; DROP TABLE recovery_cases; PRAGMA user_version=7;")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace.close().await.unwrap();
    fs::remove_file(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(digest(b"external")),
    )
    .unwrap();
    assert!(
        Workspace::migrate(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .is_err()
    );
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(rig.dir.path().join("state/state.db"))
            .read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 7);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"external"
    );
}

#[tokio::test]
async fn recovery_object_pages_cover_all_retained_hashes_without_duplicates() {
    let (mut rig, request) = conflict(Some(b"external")).await;
    let mut hashes = std::collections::BTreeSet::new();
    for index in 0..70 {
        hashes.insert(
            rig.workspace
                .files
                .put_object(format!("evidence-{index}").as_bytes())
                .unwrap(),
        );
    }
    rig.workspace
        .state
        .retain_resolution_objects(request.operation_id, &hashes)
        .await
        .unwrap();
    let first = rig
        .workspace
        .inspect_recovery(request.operation_id)
        .await
        .unwrap();
    assert_eq!(first.objects.len(), 64);
    assert!(first.next_after_hash.is_some());
    assert_eq!(first.summary.retained_objects, 73);
    let second = rig
        .workspace
        .inspect_recovery_page(request.operation_id, first.next_after_hash.as_deref())
        .await
        .unwrap();
    assert_eq!(second.objects.len(), 9);
    assert!(second.next_after_hash.is_none());
    let all: std::collections::BTreeSet<_> = first
        .objects
        .iter()
        .chain(&second.objects)
        .cloned()
        .collect();
    assert_eq!(all.len(), 73);
    assert!(hashes.is_subset(&all));
    assert_eq!(
        rig.workspace
            .inspect_recovery_page(request.operation_id, Some("invalid"))
            .await,
        Err(Error::InvalidRequest)
    );
}

#[tokio::test]
async fn save_recovery_quarantines_a_replaced_directory_before_admin_resolution() {
    let mut rig = Rig::new().await;
    let request = rig.command(b"proposed");
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.save(request.clone()).await,
        Err(Error::Storage)
    );
    let path = rig.dir.path().join("notes/Document.md");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("Current.md"), b"current child").unwrap();
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.save(request.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    rig.workspace
        .resolve_recovery(request.operation_id)
        .await
        .unwrap();
    assert!(rig.workspace.ready());
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await,
        Err(Error::NodeDeleted)
    );
    let current = rig
        .workspace
        .state
        .all_nodes()
        .await
        .unwrap()
        .into_iter()
        .find(|n| !n.deleted && n.name == "Current.md")
        .unwrap();
    assert_eq!(
        rig.workspace.read(current.node_id).await.unwrap().1,
        b"current child"
    );
}
