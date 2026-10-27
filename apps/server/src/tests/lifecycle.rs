use super::*;
use notryx_protocol::NodeKind;

fn delete(rig: &Rig, node: &NodeSnapshot, sequence: u64) -> DeleteNode {
    DeleteNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node.node_id,
        expected_node_etag: node.etag().unwrap(),
    }
}

fn restore(
    rig: &Rig,
    node: &NodeSnapshot,
    parent: NodeId,
    name: &str,
    sequence: u64,
) -> RestoreNode {
    RestoreNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node.node_id,
        expected_meta_rev: node.meta_rev.clone(),
        parent_id: parent,
        name: name.into(),
    }
}

async fn create(
    rig: &mut Rig,
    parent: NodeId,
    name: &str,
    kind: NodeKind,
    sequence: u64,
) -> NodeSnapshot {
    let command = CreateNode {
        upload_id: None,
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: sequence,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: parent,
        name: name.into(),
        kind,
    };
    let Outcome::Created(node) = rig.workspace.create_node(command).await.unwrap().outcome else {
        panic!("created")
    };
    node
}

async fn deleted(rig: &mut Rig, node: &NodeSnapshot, sequence: u64) -> NodeSnapshot {
    let command = delete(rig, node, sequence);
    let Outcome::Deleted(node) = rig.workspace.delete_node(command).await.unwrap().outcome else {
        panic!("deleted")
    };
    node
}

#[tokio::test]
async fn delete_and_restore_preserve_bytes_identity_and_reject_stale_lifecycle_validators() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = NodeSnapshot::from(&rig.initial);
    let bytes = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let delete_request = delete(&rig, &before, 1);
    let receipt = rig
        .workspace
        .delete_node(delete_request.clone())
        .await
        .unwrap();
    let Outcome::Deleted(tombstone) = &receipt.outcome else {
        panic!("deleted")
    };
    assert!(tombstone.deleted);
    assert_eq!(tombstone.node_id, before.node_id);
    assert_eq!(tombstone.kind, before.kind);
    assert_eq!(tombstone.content_hash, before.content_hash);
    assert_eq!(tombstone.size, before.size);
    assert_ne!(tombstone.content_rev, before.content_rev);
    assert_ne!(tombstone.meta_rev, before.meta_rev);
    assert!(!rig.dir.path().join("notes/Document.md").exists());
    assert_eq!(
        rig.workspace.read(before.node_id).await,
        Err(Error::NodeDeleted)
    );
    assert!(
        rig.workspace
            .node_state(before.node_id)
            .await
            .unwrap()
            .deleted
    );
    assert!(
        rig.workspace
            .node_metadata(before.node_id)
            .await
            .unwrap()
            .deleted
    );
    assert_eq!(
        rig.workspace
            .files
            .read_object(before.content_hash.as_ref().unwrap())
            .unwrap(),
        bytes
    );
    let restore_request = restore(&rig, tombstone, root, "Restored.md", 2);
    let restored_receipt = rig
        .workspace
        .restore_node(restore_request.clone())
        .await
        .unwrap();
    let Outcome::Restored(restored) = &restored_receipt.outcome else {
        panic!("restored")
    };
    assert!(!restored.deleted);
    assert_eq!(restored.node_id, before.node_id);
    assert_eq!(restored.kind, before.kind);
    assert_ne!(restored.content_rev, before.content_rev);
    assert_ne!(restored.content_rev, tombstone.content_rev);
    assert_ne!(restored.meta_rev, tombstone.meta_rev);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Restored.md")).unwrap(),
        bytes
    );
    assert_eq!(
        rig.workspace.delete_node(delete_request).await.unwrap(),
        receipt
    );
    assert_eq!(
        rig.workspace.restore_node(restore_request).await.unwrap(),
        restored_receipt
    );
    let stale_save = SaveContent {
        operation_seq: 3,
        ..rig.command(b"stale overwrite")
    };
    assert_eq!(
        rig.workspace.save(stale_save).await.unwrap().outcome,
        Outcome::PreconditionFailed {
            current: restored.content_rev.clone().unwrap()
        }
    );
    let stale_delete = delete(&rig, &before, 4);
    assert_eq!(
        rig.workspace
            .delete_node(stale_delete)
            .await
            .unwrap()
            .outcome,
        Outcome::NodePreconditionFailed {
            current: restored.etag().unwrap()
        }
    );
    let next_tombstone = deleted(&mut rig, restored, 5).await;
    let stale_restore = restore(&rig, tombstone, root, "Again.md", 6);
    assert_eq!(
        rig.workspace
            .restore_node(stale_restore)
            .await
            .unwrap()
            .outcome,
        Outcome::MetadataPreconditionFailed {
            current: next_tombstone.meta_rev
        }
    );
    assert_eq!(
        rig.workspace
            .state
            .node(root)
            .await
            .unwrap()
            .unwrap()
            .children_generation,
        4
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
async fn deleted_names_are_reusable_but_ids_and_historical_receipts_are_preserved() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let original = NodeSnapshot::from(&rig.initial);
    let command = delete(&rig, &original, 1);
    let deleted_receipt = rig.workspace.delete_node(command.clone()).await.unwrap();
    let Outcome::Deleted(tombstone) = &deleted_receipt.outcome else {
        panic!("deleted")
    };
    let replacement = create(&mut rig, root, "Document.md", NodeKind::File, 2).await;
    assert_ne!(replacement.node_id, original.node_id);
    let reused_id = CreateNode {
        upload_id: None,
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: original.node_id,
        parent_id: root,
        name: "Other.md".into(),
        kind: NodeKind::File,
    };
    assert_eq!(
        rig.workspace.create_node(reused_id).await.unwrap().outcome,
        Outcome::NodeIdConflict
    );
    let occupied = restore(&rig, tombstone, root, "document.MD", 4);
    assert_eq!(
        rig.workspace
            .restore_node(occupied.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::DestinationExists
    );
    let renamed = restore(&rig, tombstone, root, "Recovered.md", 5);
    assert!(matches!(
        rig.workspace.restore_node(renamed).await.unwrap().outcome,
        Outcome::Restored(_)
    ));
    assert_eq!(
        rig.workspace.delete_node(command.clone()).await.unwrap(),
        deleted_receipt
    );
    assert_eq!(
        rig.workspace.restore_node(occupied).await.unwrap().outcome,
        Outcome::DestinationExists
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b""
    );
    rig.workspace
        .ack_operations(AckOperations {
            epoch: rig.workspace.epoch(),
            stream_id: rig.stream,
            ack_through: 5,
        })
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.delete_node(command).await,
        Err(Error::OperationRetired)
    );
}

#[tokio::test]
async fn folder_deletion_checks_root_live_children_and_untracked_hidden_entries() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let root_node = rig.workspace.state.node(root).await.unwrap().unwrap();
    let root_request = delete(&rig, &root_node, 1);
    assert_eq!(
        rig.workspace
            .delete_node(root_request)
            .await
            .unwrap()
            .outcome,
        Outcome::RootImmutable
    );
    let folder = create(&mut rig, root, "Folder", NodeKind::Folder, 2).await;
    fs::write(rig.dir.path().join("notes/Folder/.hidden"), b"keep").unwrap();
    let hidden_request = delete(&rig, &folder, 3);
    assert_eq!(
        rig.workspace
            .delete_node(hidden_request)
            .await
            .unwrap()
            .outcome,
        Outcome::FolderNotEmpty
    );
    fs::remove_file(rig.dir.path().join("notes/Folder/.hidden")).unwrap();
    let child = create(&mut rig, folder.node_id, "Child.md", NodeKind::File, 4).await;
    let current = rig
        .workspace
        .state
        .node(folder.node_id)
        .await
        .unwrap()
        .unwrap();
    let children_request = delete(&rig, &current, 5);
    assert_eq!(
        rig.workspace
            .delete_node(children_request)
            .await
            .unwrap()
            .outcome,
        Outcome::FolderNotEmpty
    );
    deleted(&mut rig, &child, 6).await;
    let current = rig
        .workspace
        .state
        .node(folder.node_id)
        .await
        .unwrap()
        .unwrap();
    let tombstone = deleted(&mut rig, &current, 7).await;
    assert_eq!(tombstone.children_generation, 2);
    let command = restore(&rig, &tombstone, root, "RestoredFolder", 8);
    let Outcome::Restored(restored) = rig.workspace.restore_node(command).await.unwrap().outcome
    else {
        panic!("restored folder")
    };
    assert_eq!(restored.children_generation, 2);
    assert_eq!(restored.content_rev, None);
    assert!(
        fs::read_dir(rig.dir.path().join("notes/RestoredFolder"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        rig.workspace
            .node_state(child.node_id)
            .await
            .unwrap()
            .deleted
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
async fn trash_is_a_live_uuid_keyset_and_inventory_excludes_tombstones() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let folder = create(&mut rig, root, "Folder", NodeKind::Folder, 1).await;
    let original = NodeSnapshot::from(&rig.initial);
    let first = deleted(&mut rig, &original, 2).await;
    let second = deleted(&mut rig, &folder, 3).await;
    let mut ids = [first.node_id, second.node_id];
    ids.sort_by_key(|id| id.to_string());
    let page = rig.workspace.trash(None, 1).await.unwrap();
    assert_eq!(page.workspace_epoch, rig.workspace.epoch());
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.nodes[0].node_id, ids[0]);
    assert_eq!(page.next_after_id, Some(ids[0]));
    let next = rig.workspace.trash(page.next_after_id, 1).await.unwrap();
    assert_eq!(next.nodes.len(), 1);
    assert_eq!(next.nodes[0].node_id, ids[1]);
    assert_eq!(next.next_after_id, None);
    assert!(page.nodes[0].restorable && next.nodes[0].restorable);
    assert_eq!(
        rig.workspace
            .inventory(None, 100)
            .await
            .unwrap()
            .nodes
            .len(),
        1
    );
    let after_end = rig
        .workspace
        .trash(
            Some("ffffffff-ffff-7fff-bfff-ffffffffffff".parse().unwrap()),
            100,
        )
        .await
        .unwrap();
    assert!(after_end.nodes.is_empty());
    for limit in [0, 101] {
        assert_eq!(
            rig.workspace.trash(None, limit).await,
            Err(Error::InvalidRequest)
        );
    }
    let command = restore(&rig, &first, root, "Live.md", 4);
    rig.workspace.restore_node(command).await.unwrap();
    let now = rig.workspace.trash(None, 100).await.unwrap();
    assert_eq!(now.nodes.len(), 1);
    assert_eq!(now.nodes[0].node_id, second.node_id);
}

#[tokio::test]
async fn deleted_parents_and_expired_history_do_not_block_startup_or_allow_new_children() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let parent = create(&mut rig, root, "Parent", NodeKind::Folder, 1).await;
    let child = create(&mut rig, parent.node_id, "Child.md", NodeKind::File, 2).await;
    let child_tombstone = deleted(&mut rig, &child, 3).await;
    let parent_now = rig
        .workspace
        .state
        .node(parent.node_id)
        .await
        .unwrap()
        .unwrap();
    let parent_tombstone = deleted(&mut rig, &parent_now, 4).await;
    fs::remove_file(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(child.content_hash.unwrap()),
    )
    .unwrap();
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert!(rig.workspace.ready());
    let page = rig.workspace.trash(None, 100).await.unwrap();
    assert!(
        !page
            .nodes
            .iter()
            .find(|node| node.node_id == child.node_id)
            .unwrap()
            .restorable
    );
    assert!(
        page.nodes
            .iter()
            .find(|node| node.node_id == parent.node_id)
            .unwrap()
            .restorable
    );
    let invalid_parent = restore(&rig, &child_tombstone, parent.node_id, "Child.md", 5);
    assert_eq!(
        rig.workspace
            .restore_node(invalid_parent)
            .await
            .unwrap()
            .outcome,
        Outcome::NodeDeleted
    );
    let create_request = CreateNode {
        upload_id: None,
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 6,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: parent.node_id,
        name: "New.md".into(),
        kind: NodeKind::File,
    };
    assert_eq!(
        rig.workspace
            .create_node(create_request)
            .await
            .unwrap()
            .outcome,
        Outcome::NodeDeleted
    );
    let move_request = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 7,
        operation_id: op_id(),
        node_id: rig.initial.node_id,
        expected_meta_rev: rig.initial.meta_rev.clone(),
        expected_parent_id: root,
        parent_id: parent.node_id,
        name: "Moved.md".into(),
    };
    assert_eq!(
        rig.workspace.move_node(move_request).await.unwrap().outcome,
        Outcome::NodeDeleted
    );
    let expired = restore(&rig, &child_tombstone, root, "Child.md", 8);
    assert_eq!(
        rig.workspace
            .restore_node(expired.clone())
            .await
            .unwrap()
            .outcome,
        Outcome::HistoryExpired
    );
    let parent_restore = restore(&rig, &parent_tombstone, root, "RecoveredParent", 9);
    assert!(matches!(
        rig.workspace
            .restore_node(parent_restore)
            .await
            .unwrap()
            .outcome,
        Outcome::Restored(_)
    ));
    assert_eq!(
        rig.workspace.restore_node(expired).await.unwrap().outcome,
        Outcome::HistoryExpired
    );
    assert!(
        rig.workspace
            .node_state(child.node_id)
            .await
            .unwrap()
            .deleted
    );
}

#[tokio::test]
async fn deleted_targets_reject_save_move_and_repeat_delete_as_new_operations() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let before = NodeSnapshot::from(&rig.initial);
    let tombstone = deleted(&mut rig, &before, 1).await;
    let save = SaveContent {
        operation_seq: 2,
        ..rig.command(b"must not return")
    };
    assert_eq!(
        rig.workspace.save(save).await.unwrap().outcome,
        Outcome::NodeDeleted
    );
    let command = MoveNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 3,
        operation_id: op_id(),
        node_id: before.node_id,
        expected_meta_rev: tombstone.meta_rev.clone(),
        expected_parent_id: root,
        parent_id: root,
        name: "Moved.md".into(),
    };
    assert_eq!(
        rig.workspace.move_node(command).await.unwrap().outcome,
        Outcome::NodeDeleted
    );
    let repeat = delete(&rig, &tombstone, 4);
    assert_eq!(
        rig.workspace.delete_node(repeat).await.unwrap().outcome,
        Outcome::NodeDeleted
    );
    let live = create(&mut rig, root, "Live", NodeKind::Folder, 5).await;
    let not_deleted = restore(&rig, &live, root, "Other", 6);
    assert_eq!(
        rig.workspace
            .restore_node(not_deleted)
            .await
            .unwrap()
            .outcome,
        Outcome::NotDeleted
    );
    assert!(!rig.dir.path().join("notes/Document.md").exists());
}

#[tokio::test]
async fn delete_and_restore_recover_only_durable_before_or_proposed_states() {
    for kind in [NodeKind::File, NodeKind::Folder] {
        for restoring in [false, true] {
            for applied in [false, true] {
                let mut rig = Rig::new().await;
                let root = rig.workspace.state.root_id;
                let (before, mut sequence) = if kind == NodeKind::Folder {
                    (create(&mut rig, root, "Folder", kind, 1).await, 2)
                } else {
                    (NodeSnapshot::from(&rig.initial), 1)
                };
                let tombstone = if restoring {
                    let node = deleted(&mut rig, &before, sequence).await;
                    sequence += 1;
                    Some(node)
                } else {
                    None
                };
                let deletion = delete(&rig, &before, sequence);
                let restoration = restore(
                    &rig,
                    tombstone.as_ref().unwrap_or(&before),
                    root,
                    "Recovered",
                    sequence,
                );
                let operation_id = if restoring {
                    restoration.operation_id
                } else {
                    deletion.operation_id
                };
                let boundary = if applied {
                    Phase::Storage(if restoring {
                        notryx_server_storage::StorageStage::AfterRename
                    } else {
                        notryx_server_storage::StorageStage::AfterUnlink
                    })
                } else {
                    Phase::AfterAdmission
                };
                rig.workspace.hook = Some(Box::new(move |phase| {
                    if phase == boundary {
                        Err(Error::Storage)
                    } else {
                        Ok(())
                    }
                }));
                let result = if restoring {
                    rig.workspace.restore_node(restoration.clone()).await
                } else {
                    rig.workspace.delete_node(deletion.clone()).await
                };
                assert!(result.is_err());
                assert!(!rig.workspace.ready());
                rig.workspace.close().await.unwrap();
                let mut reopened =
                    Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                        .await
                        .unwrap();
                assert!(reopened.ready());
                let record = reopened
                    .state
                    .operation(rig.stream, operation_id)
                    .await
                    .unwrap();
                match (&record.outcome, restoring, applied) {
                    (Outcome::Restored(node), true, true) => {
                        assert!(!node.deleted);
                        assert!(rig.dir.path().join("notes/Recovered").exists());
                    }
                    (Outcome::Deleted(node), false, true) => {
                        assert!(node.deleted);
                        assert!(!rig.dir.path().join("notes").join(&before.name).exists());
                    }
                    (Outcome::NotApplied, _, false) => {}
                    _ => panic!("unexpected recovery outcome"),
                }
                let replay = if restoring {
                    reopened.restore_node(restoration).await.unwrap()
                } else {
                    reopened.delete_node(deletion).await.unwrap()
                };
                assert_eq!(replay.outcome, record.outcome);
            }
        }
    }
}

#[tokio::test]
async fn lifecycle_sql_and_recovery_sync_failures_keep_pending_until_retry() {
    for restoring in [false, true] {
        let mut rig = Rig::new().await;
        let root = rig.workspace.state.root_id;
        let before = NodeSnapshot::from(&rig.initial);
        let tombstone = if restoring {
            Some(deleted(&mut rig, &before, 1).await)
        } else {
            None
        };
        let sequence = if restoring { 2 } else { 1 };
        let deletion = delete(&rig, &before, sequence);
        let restoration = restore(
            &rig,
            tombstone.as_ref().unwrap_or(&before),
            root,
            "Recovered.md",
            sequence,
        );
        let operation_id = if restoring {
            restoration.operation_id
        } else {
            deletion.operation_id
        };
        sqlx::raw_sql("CREATE TRIGGER injected BEFORE UPDATE OF deleted ON nodes BEGIN SELECT RAISE(ABORT,'synthetic failure'); END")
            .execute(&mut rig.workspace.state.connection).await.unwrap();
        let result = if restoring {
            rig.workspace.restore_node(restoration.clone()).await
        } else {
            rig.workspace.delete_node(deletion.clone()).await
        };
        assert_eq!(result, Err(Error::Storage));
        assert_eq!(
            rig.workspace
                .state
                .operation(rig.stream, operation_id)
                .await
                .unwrap()
                .outcome,
            Outcome::Pending
        );
        sqlx::raw_sql("DROP TRIGGER injected")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
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
                reopened
                    .state
                    .operation(rig.stream, operation_id)
                    .await
                    .unwrap()
                    .outcome,
                Outcome::Pending
            );
        }
        reopened.hook = None;
        reopened.recover().await.unwrap();
        assert!(reopened.ready());
        let final_receipt = if restoring {
            reopened.restore_node(restoration).await.unwrap()
        } else {
            reopened.delete_node(deletion).await.unwrap()
        };
        assert!(matches!(
            final_receipt.outcome,
            Outcome::Deleted(_) | Outcome::Restored(_)
        ));
    }
}

#[tokio::test]
async fn unexpected_lifecycle_entries_are_preserved_and_quarantined() {
    for restoring in [false, true] {
        let mut rig = Rig::new().await;
        let root = rig.workspace.state.root_id;
        let before = NodeSnapshot::from(&rig.initial);
        let tombstone = if restoring {
            Some(deleted(&mut rig, &before, 1).await)
        } else {
            None
        };
        let sequence = if restoring { 2 } else { 1 };
        let deletion = delete(&rig, &before, sequence);
        let restoration = restore(
            &rig,
            tombstone.as_ref().unwrap_or(&before),
            root,
            "Recovered.md",
            sequence,
        );
        let operation_id = if restoring {
            restoration.operation_id
        } else {
            deletion.operation_id
        };
        rig.workspace.hook = Some(Box::new(|phase| {
            if phase == Phase::AfterAdmission {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        if restoring {
            assert!(rig.workspace.restore_node(restoration).await.is_err());
        } else {
            assert!(rig.workspace.delete_node(deletion).await.is_err());
        }
        let path = rig.dir.path().join(if restoring {
            "notes/Recovered.md"
        } else {
            "notes/Document.md"
        });
        fs::write(&path, b"external evidence\r\n").unwrap();
        rig.workspace.close().await.unwrap();
        for _ in 0..2 {
            let mut reopened =
                Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                    .await
                    .unwrap();
            assert!(!reopened.ready());
            assert_eq!(
                reopened
                    .state
                    .operation(rig.stream, operation_id)
                    .await
                    .unwrap()
                    .outcome,
                Outcome::Pending
            );
            assert_eq!(fs::read(&path).unwrap(), b"external evidence\r\n");
            assert_eq!(
                reopened
                    .files
                    .read_object(&digest(b"external evidence\r\n"))
                    .unwrap(),
                b"external evidence\r\n"
            );
            let (quarantined, observed): (i64, Option<String>) = sqlx::query_as(
                "SELECT recovery_conflict,observed_hash FROM write_intents WHERE operation_id=?",
            )
            .bind(operation_id.to_string())
            .fetch_one(&mut reopened.state.connection)
            .await
            .unwrap();
            assert_eq!(quarantined, 1);
            assert_eq!(
                observed.as_deref(),
                Some(digest(b"external evidence\r\n").as_str())
            );
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn real_schema_six_migration_preserves_pending_operations_and_replaces_live_name_constraint()
{
    for pending_kind in ["save", "create", "move"] {
        let mut rig = Rig::new().await;
        let root = rig.workspace.state.root_id;
        let receipt = rig
            .workspace
            .save(rig.command(b"before migration\r\n"))
            .await
            .unwrap();
        let Outcome::Saved(document) = &receipt.outcome else {
            panic!("saved")
        };
        let saved_record = rig
            .workspace
            .state
            .operation(rig.stream, receipt.operation_id)
            .await
            .unwrap();
        let create_request = CreateNode {
            upload_id: None,
            epoch: rig.workspace.epoch(),
            stream_id: rig.stream,
            operation_seq: 2,
            operation_id: op_id(),
            node_id: node_id(),
            parent_id: root,
            name: "Created.md".into(),
            kind: NodeKind::File,
        };
        let move_request = MoveNode {
            epoch: rig.workspace.epoch(),
            stream_id: rig.stream,
            operation_seq: 2,
            operation_id: op_id(),
            node_id: document.node_id,
            expected_meta_rev: document.meta_rev.clone(),
            expected_parent_id: root,
            parent_id: root,
            name: "Moved.md".into(),
        };
        let save_request = SaveContent {
            operation_seq: 2,
            expected_content_rev: document.content_rev.clone(),
            ..rig.command(b"pending migration\r\n")
        };
        let pending_id = match pending_kind {
            "save" => save_request.operation_id,
            "create" => create_request.operation_id,
            _ => move_request.operation_id,
        };
        rig.workspace.hook = Some(Box::new(|phase| {
            if phase == Phase::BeforeFinish {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        let result = match pending_kind {
            "save" => rig.workspace.save(save_request).await,
            "create" => rig.workspace.create_node(create_request).await,
            _ => rig.workspace.move_node(move_request).await,
        };
        assert!(result.is_err());
        let pending: String = sqlx::query_scalar("SELECT intent FROM write_intents")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        legacy::nodes_before_deletion(&mut rig.workspace.state.connection).await;
        sqlx::query("PRAGMA user_version=6")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        let columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('nodes') WHERE name='deleted'",
        )
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
        assert_eq!(columns, 0);
        let schema: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name='nodes'")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert!(schema.contains("UNIQUE(parent_id, normalized_name_key)"));
        let epoch = rig.workspace.epoch();
        rig.workspace.close().await.unwrap();
        let notes = rig.dir.path().join("notes");
        let state = rig.dir.path().join("state");
        assert!(matches!(
            Workspace::open(&notes, &state).await,
            Err(Error::MigrationRequired)
        ));
        Workspace::migrate(&notes, &state).await.unwrap();
        rig.workspace = Workspace::load(&notes, &state).await.unwrap();
        assert_eq!(rig.workspace.epoch(), epoch);
        assert_eq!(
            rig.workspace
                .state
                .operation(rig.stream, receipt.operation_id)
                .await
                .unwrap(),
            saved_record
        );
        assert_eq!(
            rig.workspace
                .state
                .operation(rig.stream, pending_id)
                .await
                .unwrap()
                .outcome,
            Outcome::Pending
        );
        let retained: String = sqlx::query_scalar("SELECT intent FROM write_intents")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(retained, pending);
        rig.workspace.recover().await.unwrap();
        assert!(rig.workspace.ready());
        let terminal = rig
            .workspace
            .state
            .operation(rig.stream, pending_id)
            .await
            .unwrap();
        match (pending_kind, terminal.outcome) {
            ("save", Outcome::Saved(node)) => {
                assert_eq!(node.node_id, document.node_id);
                assert_eq!(
                    fs::read(notes.join("Document.md")).unwrap(),
                    b"pending migration\r\n"
                );
            }
            ("create", Outcome::Created(node)) => {
                assert_eq!(node.name, "Created.md");
                assert_eq!(fs::read(notes.join("Created.md")).unwrap(), b"");
                assert_eq!(
                    rig.workspace
                        .node_state(node.node_id)
                        .await
                        .unwrap()
                        .node_id,
                    node.node_id
                );
            }
            ("move", Outcome::Moved(node)) => {
                assert_eq!(node.node_id, document.node_id);
                assert_eq!(node.name, "Moved.md");
                assert!(!notes.join("Document.md").exists());
                assert_eq!(
                    fs::read(notes.join("Moved.md")).unwrap(),
                    b"before migration\r\n"
                );
            }
            (_, outcome) => panic!("incorrect pending terminal: {outcome:?}"),
        }
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(version, 9);
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&mut rig.workspace.state.connection)
                .await
                .unwrap()
                .is_empty()
        );
        let current = rig
            .workspace
            .state
            .node(document.node_id)
            .await
            .unwrap()
            .unwrap();
        deleted(&mut rig, &current, 3).await;
        let replacement = create(&mut rig, root, &current.name, NodeKind::File, 4).await;
        assert_ne!(replacement.node_id, current.node_id);
    }
}

#[tokio::test]
async fn schema_rebuild_failure_restores_original_tables_and_foreign_key_enforcement() {
    let mut rig = Rig::new().await;
    let receipt = rig
        .workspace
        .save(rig.command(b"migration evidence\r\n"))
        .await
        .unwrap();
    legacy::nodes_before_deletion(&mut rig.workspace.state.connection).await;
    sqlx::query("PRAGMA user_version=6")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let schema: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name='nodes'")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM operation_receipts WHERE operation_id=?")
            .bind(receipt.operation_id.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    sqlx::raw_sql("CREATE TABLE live_node_name(sentinel TEXT); INSERT INTO live_node_name VALUES('keep evidence');").execute(&mut rig.workspace.state.connection).await.unwrap();
    assert!(rig.workspace.state.migrate(6).await.is_err());
    let actual_schema: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name='nodes'")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(actual_schema, schema);
    let actual_outcome: String =
        sqlx::query_scalar("SELECT outcome FROM operation_receipts WHERE operation_id=?")
            .bind(receipt.operation_id.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(actual_outcome, outcome);
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let enforcement: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(version, 6);
    assert_eq!(enforcement, 1);
    let evidence: String = sqlx::query_scalar("SELECT sentinel FROM live_node_name")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(evidence, "keep evidence");
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"migration evidence\r\n"
    );
}

#[tokio::test]
async fn trash_caps_actual_json_without_skipping_or_repeating_nodes() {
    let mut rig = Rig::new().await;
    let root = rig.workspace.state.root_id;
    let folder = create(&mut rig, root, "Folder", NodeKind::Folder, 1).await;
    let before = NodeSnapshot::from(&rig.initial);
    let first = deleted(&mut rig, &before, 2).await;
    let second = deleted(&mut rig, &folder, 3).await;
    for node in [&first, &second] {
        sqlx::query("UPDATE nodes SET meta_rev=? WHERE node_id=?")
            .bind("m".repeat(600_000))
            .bind(node.node_id.to_string())
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    }
    let page = rig.workspace.trash(None, 100).await.unwrap();
    assert_eq!(page.nodes.len(), 1);
    assert!(serde_json::to_vec(&page).unwrap().len() <= 1024 * 1024);
    assert_eq!(page.next_after_id, Some(page.nodes[0].node_id));
    let next = rig.workspace.trash(page.next_after_id, 100).await.unwrap();
    assert_eq!(next.nodes.len(), 1);
    assert_ne!(next.nodes[0].node_id, page.nodes[0].node_id);
    assert_eq!(next.next_after_id, None);
    sqlx::query("UPDATE nodes SET meta_rev=? WHERE node_id=?")
        .bind("m".repeat(1_100_000))
        .bind(page.nodes[0].node_id.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert!(rig.workspace.trash(None, 100).await.is_err());
}

#[tokio::test]
async fn foreign_key_failure_rolls_back_schema_and_keeps_pending_objects() {
    let mut rig = Rig::new().await;
    let pending = rig.command(b"retained pending migration bytes\r\n");
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert!(rig.workspace.save(pending.clone()).await.is_err());
    legacy::nodes_before_deletion(&mut rig.workspace.state.connection).await;
    sqlx::query("PRAGMA user_version=6")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let schema: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name='nodes'")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let intent: String = sqlx::query_scalar("SELECT intent FROM write_intents")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let orphan = node_id();
    sqlx::query("CREATE TABLE migration_evidence(node_id TEXT REFERENCES nodes(node_id))")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO migration_evidence VALUES(?)")
        .bind(orphan.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.state.migrate(6).await,
        Err(Error::InvalidFormat)
    );
    let actual: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name='nodes'")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(actual, schema);
    let actual: String = sqlx::query_scalar("SELECT intent FROM write_intents")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(actual, intent);
    let parent: String = sqlx::query_scalar("SELECT node_id FROM migration_evidence")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(parent, orphan.to_string());
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let enforced: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!((version, enforced), (6, 1));
    assert_eq!(
        rig.workspace
            .state
            .operation(rig.stream, pending.operation_id)
            .await
            .unwrap()
            .outcome,
        Outcome::Pending
    );
    assert_eq!(
        rig.workspace
            .files
            .read_object(&pending.content_hash)
            .unwrap(),
        pending.bytes
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        rig.workspace
            .files
            .read_object(&rig.initial.content_hash)
            .unwrap()
    );
}
