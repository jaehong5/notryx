use super::*;
use notryx_server_storage::StorageStage;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::process::ExitStatusExt,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    time::Duration,
};

#[test]
#[ignore = "invoked only by the SIGKILL parent test"]
fn crash_worker() {
    let directory = std::path::PathBuf::from(std::env::var_os("NOTRYX_TEST_WORKSPACE").unwrap());
    let target = std::env::var("NOTRYX_TEST_CUT").unwrap();
    let recovery = std::env::var("NOTRYX_TEST_RECOVER").is_ok();
    let ack = std::env::var("NOTRYX_TEST_ACK").is_ok();
    let create = std::env::var("NOTRYX_TEST_CREATE").is_ok();
    let move_node = std::env::var("NOTRYX_TEST_MOVE").is_ok();
    let delete_node = std::env::var("NOTRYX_TEST_DELETE").is_ok();
    let restore_node = std::env::var("NOTRYX_TEST_RESTORE").is_ok();
    let resolve = std::env::var("NOTRYX_TEST_RESOLVE").is_ok();
    let upload = directory.join("upload-request.json").exists();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut workspace = Workspace::load(&directory.join("notes"), &directory.join("state"))
            .await
            .unwrap();
        if !recovery {
            workspace.recover().await.unwrap();
        }
        workspace.hook = Some(Box::new(move |phase| {
            if format!("{phase:?}") == target {
                println!("NOTRYX_CUT");
                std::io::stdout().flush().unwrap();
                loop {
                    std::thread::park();
                }
            }
            Ok(())
        }));
        if recovery {
            workspace.recover().await.unwrap();
        } else if upload {
            let (token, bytes): (String, Vec<u8>) =
                serde_json::from_slice(&fs::read(directory.join("upload-request.json")).unwrap())
                    .unwrap();
            let time = crate::upload::now().unwrap();
            let mut grant = workspace
                .begin_upload(&token, digest(&bytes), Some(bytes.len() as u64), time)
                .await
                .unwrap();
            grant.writer.write_chunk(&bytes).unwrap();
            workspace
                .finish_upload(&token, grant.upload_id, grant.writer.seal().unwrap(), time)
                .await
                .unwrap();
        } else if resolve {
            let id =
                serde_json::from_slice(&fs::read(directory.join("resolve-request.json")).unwrap())
                    .unwrap();
            workspace.resolve_recovery(id).await.unwrap();
        } else if ack {
            let (stream_id, ack_through) = serde_json::from_slice::<(Uuid, u64)>(
                &fs::read(directory.join("request.json")).unwrap(),
            )
            .unwrap();
            workspace
                .ack_operations(crate::AckOperations {
                    epoch: workspace.epoch(),
                    stream_id,
                    ack_through,
                })
                .await
                .unwrap();
        } else if delete_node {
            let request =
                serde_json::from_slice(&fs::read(directory.join("delete-request.json")).unwrap())
                    .unwrap();
            workspace.delete_node(request).await.unwrap();
        } else if restore_node {
            let request =
                serde_json::from_slice(&fs::read(directory.join("restore-request.json")).unwrap())
                    .unwrap();
            workspace.restore_node(request).await.unwrap();
        } else if move_node {
            let request =
                serde_json::from_slice(&fs::read(directory.join("move-request.json")).unwrap())
                    .unwrap();
            workspace.move_node(request).await.unwrap();
        } else if create {
            let request =
                serde_json::from_slice(&fs::read(directory.join("create-request.json")).unwrap())
                    .unwrap();
            let token = fs::read_to_string(directory.join("upload-token.txt")).ok();
            workspace
                .create_node_authorized(request, token.as_deref())
                .await
                .unwrap();
        } else {
            let request =
                serde_json::from_slice(&fs::read(directory.join("request.json")).unwrap()).unwrap();
            workspace.save(request).await.unwrap();
        }
        panic!("requested crash boundary was not reached");
    });
}

pub(super) fn kill_at(directory: &Path, target: Phase, recovery: bool) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tests::crash::crash_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("NOTRYX_TEST_WORKSPACE", directory)
        .env("NOTRYX_TEST_CUT", format!("{target:?}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if recovery {
        command.env("NOTRYX_TEST_RECOVER", "1");
    } else {
        command.env_remove("NOTRYX_TEST_RECOVER");
    }
    if matches!(target, Phase::BeforeAckCommit | Phase::AfterAckCommit) {
        command.env("NOTRYX_TEST_ACK", "1");
    } else {
        command.env_remove("NOTRYX_TEST_ACK");
    }
    if directory.join("create-request.json").exists() {
        command.env("NOTRYX_TEST_CREATE", "1");
    } else {
        command.env_remove("NOTRYX_TEST_CREATE");
    }
    if directory.join("move-request.json").exists() {
        command.env("NOTRYX_TEST_MOVE", "1");
    } else {
        command.env_remove("NOTRYX_TEST_MOVE");
    }
    for (file, flag) in [
        ("resolve-request.json", "NOTRYX_TEST_RESOLVE"),
        ("delete-request.json", "NOTRYX_TEST_DELETE"),
        ("restore-request.json", "NOTRYX_TEST_RESTORE"),
    ] {
        if directory.join(file).exists() {
            command.env(flag, "1");
        } else {
            command.env_remove(flag);
        }
    }
    let child = command.spawn().unwrap();
    let (reached, status) = wait_for_cut_and_kill(child, Duration::from_secs(20));
    assert!(
        reached.is_ok(),
        "child did not reach {target:?}: {reached:?}, {status}"
    );
    assert_eq!(status.signal(), Some(9));
}

#[tokio::test]
async fn sigkill_at_move_boundaries_preserves_identity_and_one_terminal_result() {
    for kind in [
        notryx_protocol::NodeKind::File,
        notryx_protocol::NodeKind::Folder,
    ] {
        for phase in [
            Phase::AfterAdmission,
            Phase::Storage(StorageStage::AfterRename),
            Phase::Storage(StorageStage::AfterMoveSourceDirectorySync),
            Phase::Storage(StorageStage::AfterMoveDestinationDirectorySync),
            Phase::BeforeFinish,
            Phase::AfterFinish,
        ] {
            let mut rig = Rig::new().await;
            let destination_id = node_id();
            rig.workspace
                .create_node(CreateNode {
                    upload_id: None,
                    epoch: rig.workspace.epoch(),
                    stream_id: rig.stream,
                    operation_seq: 1,
                    operation_id: op_id(),
                    node_id: destination_id,
                    parent_id: rig.workspace.state.root_id,
                    name: "Destination".into(),
                    kind: notryx_protocol::NodeKind::Folder,
                })
                .await
                .unwrap();
            let (before, sequence, source_path, content_suffix) =
                if kind == notryx_protocol::NodeKind::Folder {
                    let receipt = rig
                        .workspace
                        .create_node(CreateNode {
                            upload_id: None,
                            epoch: rig.workspace.epoch(),
                            stream_id: rig.stream,
                            operation_seq: 2,
                            operation_id: op_id(),
                            node_id: node_id(),
                            parent_id: rig.workspace.state.root_id,
                            name: "Source".into(),
                            kind,
                        })
                        .await
                        .unwrap();
                    let Outcome::Created(node) = receipt.outcome else {
                        panic!("created folder")
                    };
                    fs::write(
                        rig.dir.path().join("notes/Source/Evidence.md"),
                        b"untouched descendant\r\n",
                    )
                    .unwrap();
                    (node, 3, "Source", "/Evidence.md")
                } else {
                    (NodeSnapshot::from(&rig.initial), 2, "Document.md", "")
                };
            let bytes = fs::read(
                rig.dir
                    .path()
                    .join(format!("notes/{source_path}{content_suffix}")),
            )
            .unwrap();
            let head_before = rig.workspace.server_info().await.unwrap().head_seq.get();
            let command = MoveNode {
                epoch: rig.workspace.epoch(),
                stream_id: rig.stream,
                operation_seq: sequence,
                operation_id: op_id(),
                node_id: before.node_id,
                expected_meta_rev: before.meta_rev.clone(),
                expected_parent_id: before.parent_id.unwrap(),
                parent_id: destination_id,
                name: "Moved".into(),
            };
            fs::write(
                rig.dir.path().join("move-request.json"),
                serde_json::to_vec(&command).unwrap(),
            )
            .unwrap();
            rig.workspace.close().await.unwrap();
            kill_at(rig.dir.path(), phase, false);
            let mut reopened =
                Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                    .await
                    .unwrap();
            assert!(reopened.ready());
            let first = reopened.move_node(command.clone()).await.unwrap();
            let applied = phase != Phase::AfterAdmission;
            if applied {
                let Outcome::Moved(after) = &first.outcome else {
                    panic!("moved")
                };
                assert_eq!(after.node_id, before.node_id);
                assert_eq!(after.content_rev, before.content_rev);
                assert_eq!(after.parent_id, Some(destination_id));
                assert!(!rig.dir.path().join(format!("notes/{source_path}")).exists());
                assert_eq!(
                    fs::read(
                        rig.dir
                            .path()
                            .join(format!("notes/Destination/Moved{content_suffix}"))
                    )
                    .unwrap(),
                    bytes
                );
            } else {
                assert_eq!(first.outcome, Outcome::NotApplied);
                assert_eq!(
                    fs::read(
                        rig.dir
                            .path()
                            .join(format!("notes/{source_path}{content_suffix}"))
                    )
                    .unwrap(),
                    bytes
                );
                assert!(!rig.dir.path().join("notes/Destination/Moved").exists());
            }
            assert_eq!(
                reopened.server_info().await.unwrap().head_seq.get(),
                head_before + u64::from(applied)
            );
            assert_eq!(reopened.move_node(command.clone()).await.unwrap(), first);
            reopened
                .ack_operations(AckOperations {
                    epoch: reopened.epoch(),
                    stream_id: rig.stream,
                    ack_through: sequence,
                })
                .await
                .unwrap();
            assert_eq!(
                reopened.move_node(command).await,
                Err(Error::OperationRetired)
            );
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn sigkill_at_create_boundaries_preserves_one_result_and_never_replays_creation() {
    for kind in [
        notryx_protocol::NodeKind::File,
        notryx_protocol::NodeKind::Folder,
    ] {
        for phase in [
            Phase::AfterAdmission,
            Phase::Storage(StorageStage::AfterRename),
            Phase::BeforeFinish,
            Phase::AfterFinish,
        ] {
            let rig = Rig::new().await;
            let request = CreateNode {
                upload_id: None,
                epoch: rig.workspace.epoch(),
                stream_id: rig.stream,
                operation_seq: 1,
                operation_id: op_id(),
                node_id: node_id(),
                parent_id: rig.workspace.state.root_id,
                name: "Created".into(),
                kind,
            };
            fs::write(
                rig.dir.path().join("create-request.json"),
                serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
            rig.workspace.close().await.unwrap();
            kill_at(rig.dir.path(), phase, false);
            let mut reopened =
                Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                    .await
                    .unwrap();
            assert!(reopened.ready());
            let first = reopened.create_node(request.clone()).await.unwrap();
            if phase == Phase::AfterAdmission {
                assert_eq!(first.outcome, Outcome::NotApplied);
                assert!(!rig.dir.path().join("notes/Created").exists());
                assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 1);
            } else {
                assert!(matches!(first.outcome, Outcome::Created(_)));
                assert!(rig.dir.path().join("notes/Created").exists());
                assert_eq!(reopened.server_info().await.unwrap().head_seq.get(), 2);
            }
            assert_eq!(reopened.create_node(request.clone()).await.unwrap(), first);
            reopened
                .ack_operations(AckOperations {
                    epoch: reopened.epoch(),
                    stream_id: rig.stream,
                    ack_through: 1,
                })
                .await
                .unwrap();
            assert_eq!(
                reopened.create_node(request).await,
                Err(Error::OperationRetired)
            );
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn sigkill_at_ack_commit_preserves_all_or_none_of_retirement() {
    for (phase, retired) in [
        (Phase::BeforeAckCommit, false),
        (Phase::AfterAckCommit, true),
    ] {
        let mut rig = Rig::new().await;
        let request = rig.command(b"acknowledged source stays unchanged\n");
        let saved = rig.workspace.save(request.clone()).await.unwrap();
        fs::write(
            rig.dir.path().join("request.json"),
            serde_json::to_vec(&(rig.stream, 1u64)).unwrap(),
        )
        .unwrap();
        rig.workspace.close().await.unwrap();
        kill_at(rig.dir.path(), phase, false);
        let mut reopened =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        let watermark: String =
            sqlx::query_scalar("SELECT retired_through FROM operation_streams WHERE stream_id=?")
                .bind(rig.stream.to_string())
                .fetch_one(&mut reopened.state.connection)
                .await
                .unwrap();
        assert_eq!(watermark, if retired { "1" } else { "0" });
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
            .fetch_one(&mut reopened.state.connection)
            .await
            .unwrap();
        let retired_ids: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retired_operations")
            .fetch_one(&mut reopened.state.connection)
            .await
            .unwrap();
        assert_eq!(
            (receipts, retired_ids),
            if retired { (0, 1) } else { (1, 0) }
        );
        if retired {
            assert_eq!(reopened.save(request).await, Err(Error::OperationRetired));
        } else {
            assert_eq!(reopened.save(request).await.unwrap(), saved);
        }
        let state = reopened
            .ack_operations(crate::AckOperations {
                epoch: reopened.epoch(),
                stream_id: rig.stream,
                ack_through: 1,
            })
            .await
            .unwrap();
        assert_eq!(state.retired_through, 1);
        assert_eq!(
            reopened.read(rig.initial.node_id).await.unwrap().1,
            b"acknowledged source stays unchanged\n"
        );
        reopened.close().await.unwrap();
    }
}

fn wait_for_cut_and_kill(
    mut child: Child,
    timeout: Duration,
) -> (Result<(), mpsc::RecvTimeoutError>, ExitStatus) {
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.trim() == "NOTRYX_CUT" {
                let _ = sender.send(());
                return;
            }
        }
    });
    let reached = receiver.recv_timeout(timeout);
    let _ = child.kill();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    (reached, status)
}

#[test]
#[ignore = "invoked only by the timeout parent test"]
fn silent_crash_worker() {
    loop {
        std::thread::park();
    }
}

#[test]
fn missing_cut_times_out_and_reaps_child() {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::crash::silent_crash_worker",
            "--ignored",
            "--nocapture",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let (reached, status) = wait_for_cut_and_kill(child, Duration::from_millis(100));
    assert_eq!(reached, Err(mpsc::RecvTimeoutError::Timeout));
    assert_eq!(status.signal(), Some(9));
}

#[tokio::test]
async fn sigkill_at_save_boundaries_replays_exactly_one_terminal_result() {
    for (phase, applied) in [
        (Phase::AfterAdmission, false),
        (Phase::Storage(StorageStage::AfterTemporaryFileSync), false),
        (Phase::Storage(StorageStage::AfterRename), true),
        (Phase::Storage(StorageStage::AfterParentDirectorySync), true),
        (Phase::BeforeFinish, true),
        (Phase::AfterFinish, true),
    ] {
        let rig = Rig::new().await;
        let request = rig.command(b"crash-safe bytes\n");
        fs::write(
            rig.dir.path().join("request.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        rig.workspace.close().await.unwrap();
        kill_at(rig.dir.path(), phase, false);
        let mut recovered =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(recovered.ready(), "{phase:?}");
        let receipt = recovered.save(request.clone()).await.unwrap();
        if applied {
            assert!(matches!(receipt.outcome, Outcome::Saved(_)), "{phase:?}");
            assert_eq!(
                recovered.read(rig.initial.node_id).await.unwrap().1,
                b"crash-safe bytes\n"
            );
        } else {
            assert_eq!(receipt.outcome, Outcome::NotApplied, "{phase:?}");
            assert_eq!(
                recovered.read(rig.initial.node_id).await.unwrap().0,
                rig.initial
            );
        }
        assert_eq!(recovered.save(request).await.unwrap(), receipt);
        let effects: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM change_sets WHERE operation_id IS NOT NULL")
                .fetch_one(&mut recovered.state.connection)
                .await
                .unwrap();
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM write_intents")
            .fetch_one(&mut recovered.state.connection)
            .await
            .unwrap();
        assert_eq!(effects, i64::from(applied));
        assert_eq!(pending, 0);
        recovered.close().await.unwrap();
    }
}

#[tokio::test]
async fn sigkill_during_recovery_can_repeat_fsync_and_finalize() {
    for (cut, expected_applied) in [
        (StorageStage::AfterRecoveryFileSync, false),
        (StorageStage::AfterRecoveryDirectorySync, true),
    ] {
        let mut rig = Rig::new().await;
        let interrupted = if expected_applied {
            Phase::BeforeFinish
        } else {
            Phase::AfterAdmission
        };
        rig.workspace.hook = Some(Box::new(move |phase| {
            if phase == interrupted {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        let request = rig.command(b"recovery bytes\n");
        assert_eq!(
            rig.workspace.save(request.clone()).await,
            Err(Error::Storage)
        );
        rig.workspace.close().await.unwrap();
        kill_at(rig.dir.path(), Phase::Storage(cut), true);
        let mut recovered =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(recovered.ready());
        let receipt = recovered.save(request).await.unwrap();
        assert_eq!(
            matches!(receipt.outcome, Outcome::Saved(_)),
            expected_applied
        );
        recovered.close().await.unwrap();
    }
}

#[tokio::test]
async fn sigkill_at_lifecycle_boundaries_and_recovery_preserves_one_terminal_result() {
    use crate::{DeleteNode, RestoreNode};
    use notryx_protocol::NodeKind;
    for kind in [NodeKind::File, NodeKind::Folder] {
        for restoring in [false, true] {
            let effect = if restoring {
                StorageStage::AfterRename
            } else {
                StorageStage::AfterUnlink
            };
            for phase in [
                Phase::AfterAdmission,
                Phase::Storage(effect),
                Phase::Storage(StorageStage::AfterParentDirectorySync),
                Phase::BeforeFinish,
                Phase::AfterFinish,
            ] {
                let mut rig = Rig::new().await;
                let root = rig.workspace.state.root_id;
                let (before, mut sequence) = if kind == NodeKind::Folder {
                    let receipt = rig
                        .workspace
                        .create_node(CreateNode {
                            upload_id: None,
                            epoch: rig.workspace.epoch(),
                            stream_id: rig.stream,
                            operation_seq: 1,
                            operation_id: op_id(),
                            node_id: node_id(),
                            parent_id: root,
                            name: "Folder".into(),
                            kind,
                        })
                        .await
                        .unwrap();
                    let Outcome::Created(node) = receipt.outcome else {
                        panic!("folder")
                    };
                    (node, 2)
                } else {
                    (NodeSnapshot::from(&rig.initial), 1)
                };
                let bytes = before
                    .content_hash
                    .as_ref()
                    .map(|hash| rig.workspace.files.read_object(hash).unwrap());
                let tombstone = if restoring {
                    let receipt = rig
                        .workspace
                        .delete_node(DeleteNode {
                            epoch: rig.workspace.epoch(),
                            stream_id: rig.stream,
                            operation_seq: sequence,
                            operation_id: op_id(),
                            node_id: before.node_id,
                            expected_node_etag: before.etag().unwrap(),
                        })
                        .await
                        .unwrap();
                    sequence += 1;
                    let Outcome::Deleted(node) = receipt.outcome else {
                        panic!("deleted")
                    };
                    Some(node)
                } else {
                    None
                };
                let deletion = DeleteNode {
                    epoch: rig.workspace.epoch(),
                    stream_id: rig.stream,
                    operation_seq: sequence,
                    operation_id: op_id(),
                    node_id: before.node_id,
                    expected_node_etag: before.etag().unwrap(),
                };
                let restoration = RestoreNode {
                    epoch: rig.workspace.epoch(),
                    stream_id: rig.stream,
                    operation_seq: sequence,
                    operation_id: op_id(),
                    node_id: before.node_id,
                    expected_meta_rev: tombstone.as_ref().unwrap_or(&before).meta_rev.clone(),
                    parent_id: root,
                    name: "Restored".into(),
                };
                let head_before = rig.workspace.server_info().await.unwrap().head_seq.get();
                let (filename, encoded) = if restoring {
                    (
                        "restore-request.json",
                        serde_json::to_vec(&restoration).unwrap(),
                    )
                } else {
                    (
                        "delete-request.json",
                        serde_json::to_vec(&deletion).unwrap(),
                    )
                };
                fs::write(rig.dir.path().join(filename), encoded).unwrap();
                rig.workspace.close().await.unwrap();
                kill_at(rig.dir.path(), phase, false);
                if phase == Phase::Storage(effect) {
                    kill_at(
                        rig.dir.path(),
                        Phase::Storage(StorageStage::AfterRecoveryDirectorySync),
                        true,
                    );
                    kill_at(rig.dir.path(), Phase::BeforeRecoveryFinish, true);
                }
                let mut reopened =
                    Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                        .await
                        .unwrap();
                assert!(reopened.ready());
                let receipt = if restoring {
                    reopened.restore_node(restoration.clone()).await.unwrap()
                } else {
                    reopened.delete_node(deletion.clone()).await.unwrap()
                };
                let applied = phase != Phase::AfterAdmission;
                match (&receipt.outcome, restoring, applied) {
                    (Outcome::NotApplied, _, false)
                    | (Outcome::Deleted(_), false, true)
                    | (Outcome::Restored(_), true, true) => {}
                    other => panic!("unexpected lifecycle result: {other:?}"),
                }
                let node = reopened.state.node(before.node_id).await.unwrap().unwrap();
                assert_eq!(node.deleted, if restoring { !applied } else { applied });
                assert_eq!(node.kind, before.kind);
                assert_eq!(node.content_hash, before.content_hash);
                assert_eq!(
                    reopened.server_info().await.unwrap().head_seq.get(),
                    head_before + u64::from(applied)
                );
                if !node.deleted {
                    let path = reopened.state.node_path(node.node_id).await.unwrap();
                    assert!(rig.dir.path().join("notes").join(&path).exists());
                    if let Some(bytes) = &bytes {
                        assert_eq!(
                            fs::read(rig.dir.path().join("notes").join(path)).unwrap(),
                            *bytes
                        );
                    }
                } else {
                    assert!(
                        !rig.dir
                            .path()
                            .join("notes")
                            .join(if restoring { "Restored" } else { &before.name })
                            .exists()
                    );
                }
                let replay = if restoring {
                    reopened.restore_node(restoration.clone()).await.unwrap()
                } else {
                    reopened.delete_node(deletion.clone()).await.unwrap()
                };
                assert_eq!(replay, receipt);
                reopened
                    .ack_operations(AckOperations {
                        epoch: reopened.epoch(),
                        stream_id: rig.stream,
                        ack_through: sequence,
                    })
                    .await
                    .unwrap();
                let retired = if restoring {
                    reopened.restore_node(restoration).await
                } else {
                    reopened.delete_node(deletion).await
                };
                assert_eq!(retired, Err(Error::OperationRetired));
                reopened.close().await.unwrap();
            }
        }
    }
}
