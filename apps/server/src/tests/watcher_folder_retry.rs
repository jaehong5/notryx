use super::*;
use crate::config::WorkspaceConfig;
use crate::runtime::{WorkspaceHandle, WorkspaceRuntime};
use notryx_protocol::{AdminRequest, AdminResult, NodeKind, PairRequest, PairResponse};
use std::time::{Duration, Instant};

struct Fixture {
    dir: tempfile::TempDir,
    runtime: WorkspaceRuntime,
    paired: PairResponse,
    root: NodeId,
    initial_folders: Option<(NodeId, NodeId)>,
}

impl Fixture {
    async fn new(with_initial_folder: bool) -> Self {
        let mut rig = Rig::new().await;
        let root = rig.initial.parent_id;
        let initial_folders = if with_initial_folder {
            let mut folders = [node_id(), node_id()];
            folders.sort_by_key(|id| id.to_string());
            for (index, name) in ["Retry folder", "Healthy folder"].into_iter().enumerate() {
                let result = rig
                    .workspace
                    .create_node(CreateNode {
                        epoch: rig.workspace.epoch(),
                        stream_id: rig.stream,
                        operation_seq: index as u64 + 1,
                        operation_id: op_id(),
                        node_id: folders[index],
                        parent_id: root,
                        name: name.into(),
                        kind: NodeKind::Folder,
                        upload_id: None,
                    })
                    .await
                    .unwrap();
                assert!(matches!(result.outcome, Outcome::Created(_)));
            }
            Some((folders[0], folders[1]))
        } else {
            None
        };
        let Rig { dir, workspace, .. } = rig;
        workspace.close().await.unwrap();
        let mut runtime = WorkspaceRuntime::test_start_with_sweep_interval(
            WorkspaceConfig {
                notes_root: dir.path().join("notes"),
                state_root: dir.path().join("state"),
                uploads: Default::default(),
            },
            Duration::from_millis(500),
        )
        .await
        .unwrap();
        runtime.test_wait_startup_scan().await;
        let handle = runtime.handle();
        assert!(!handle.known_file_scan_incomplete());
        assert!(!handle.known_file_watch_degraded());
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
            panic!("expected pairing grant");
        };
        let paired = handle
            .pair(PairRequest {
                protocol: notryx_protocol::CONTRACT.into(),
                workspace_id,
                pairing_secret,
            })
            .await
            .unwrap();
        Self {
            dir,
            runtime,
            paired,
            root,
            initial_folders,
        }
    }

    fn folder(&self, name: &str, node_id: NodeId, operation_seq: u64) -> CreateNode {
        CreateNode {
            epoch: self.paired.workspace_epoch,
            stream_id: self.paired.device_stream_id.parse().unwrap(),
            operation_seq,
            operation_id: op_id(),
            node_id,
            parent_id: self.root,
            name: name.into(),
            kind: NodeKind::Folder,
            upload_id: None,
        }
    }
}

async fn find_child(
    handle: &WorkspaceHandle,
    token: &str,
    parent: NodeId,
    name: &str,
    after: notryx_domain::ChangeSeq,
) -> Option<notryx_protocol::NodeState> {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let head = handle.server_info(token.to_owned()).await.unwrap().head_seq;
            if head > after {
                let changes = handle
                    .changes(token.to_owned(), after, Some(head))
                    .await
                    .unwrap();
                if let Some(node) = changes.nodes.into_iter().find(|node| {
                    node.parent_id == Some(parent)
                        && node.name == name
                        && node.kind == NodeKind::File
                        && !node.deleted
                }) {
                    break node;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok()
}

#[tokio::test]
async fn repeated_folder_registration_failures_are_bounded_and_other_folders_progress() {
    let fixture = Fixture::new(true).await;
    let handle = fixture.runtime.handle();
    let (failed, good) = fixture.initial_folders.unwrap();
    assert!(
        failed.to_string() < good.to_string(),
        "failed folder must be visited first by the ordered scan"
    );
    handle.test_fail_watch_folder(failed, 500);
    let retry_path = fixture.dir.path().join("notes/Retry folder");
    let good_path = fixture.dir.path().join("notes/Healthy folder");
    let removed_at = Instant::now();
    fs::remove_dir(&retry_path).unwrap();
    fs::remove_dir(&good_path).unwrap();
    tokio::time::timeout(Duration::from_millis(800), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not observe the removed folder");
    assert!(
        removed_at.elapsed() < Duration::from_secs(1),
        "test could not recreate the folder within the grace period"
    );
    fs::create_dir(&retry_path).unwrap();
    fs::create_dir(&good_path).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_watch_folder_failures() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("targeted registration failure was not observed");
    let started = Instant::now();
    let failures_before = handle.test_watch_folder_failures();
    let before = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let first_path = good_path.join("First.md");
    fs::write(&first_path, b"first watched source\n").unwrap();
    let first = find_child(
        &handle,
        &fixture.paired.device_token,
        good,
        "First.md",
        before,
    )
    .await;
    let first_content = if let Some(node) = &first {
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
    let second_path = good_path.join("Second.md");
    let second = if first.is_some() {
        let second_after = handle
            .server_info(fixture.paired.device_token.clone())
            .await
            .unwrap()
            .head_seq;
        fs::write(&second_path, b"second watched source\n").unwrap();
        find_child(
            &handle,
            &fixture.paired.device_token,
            good,
            "Second.md",
            second_after,
        )
        .await
    } else {
        None
    };
    let second_content = if let Some(node) = &second {
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
    let retried = tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_watch_folder_failures() < failures_before + 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let elapsed_ms = started.elapsed().as_millis();
    let failures_after = handle.test_watch_folder_failures();
    let max_failures = elapsed_ms.div_ceil(500) + 1;
    let incomplete = handle.known_file_scan_incomplete();
    let first_bytes = fs::read(first_path).unwrap();
    let second_bytes = fs::read(second_path).ok();
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .expect("shutdown did not drain the watcher")
        .unwrap();
    let events = handle.test_known_file_scan_events();
    let mut open_scans = 0usize;
    for event in events {
        match event {
            "known_file_scan_started" => open_scans += 1,
            "known_file_scan_complete"
            | "known_file_scan_incomplete"
            | "known_file_scan_stopped"
                if open_scans > 0 =>
            {
                open_scans -= 1
            }
            _ => {}
        }
    }
    assert!(retried, "failed folder was not retried");
    assert!(failures_after >= failures_before);
    assert!(
        (failures_after - failures_before) as u128 <= max_failures,
        "folder retry count exceeded the 500 ms rate bound for the observed interval"
    );
    assert!(incomplete, "failed folder was hidden from scan status");
    assert!(
        first.is_some(),
        "healthy folder registration did not discover its existing file"
    );
    assert!(
        second.is_some(),
        "healthy folder did not discover a later file"
    );
    assert_eq!(
        first_content.as_deref(),
        Some(b"first watched source\n".as_slice())
    );
    assert_eq!(
        second_content.as_deref(),
        Some(b"second watched source\n".as_slice())
    );
    assert_eq!(first_bytes, b"first watched source\n");
    assert_eq!(
        second_bytes.as_deref(),
        Some(b"second watched source\n".as_slice())
    );
    assert!(
        open_scans == 0,
        "shutdown left a started scan without a completion or stop result"
    );
}

#[tokio::test]
async fn folder_registration_overflow_recovers_after_a_failed_retry() {
    let fixture = Fixture::new(false).await;
    let handle = fixture.runtime.handle();
    let (entered, release) = handle.test_pause_watch_folder();
    let blocker = fixture.folder("Registration blocker", node_id(), 1);
    handle
        .create_node(fixture.paired.device_token.clone(), blocker)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handle.test_fill_watch_folder_queue(fixture.root), 256);
    let failed = node_id();
    handle.test_fail_watch_folder(failed, 1);
    let target = fixture.folder("Overflow folder", failed, 2);
    let receipt = handle
        .create_node(fixture.paired.device_token.clone(), target.clone())
        .await
        .unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Created(_)));
    assert!(handle.known_file_scan_incomplete());
    let before = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let child_path = fixture
        .dir
        .path()
        .join("notes/Overflow folder/Recovered.md");
    fs::write(&child_path, b"overflow recovered source\n").unwrap();
    release.send(()).unwrap();
    let failure = tokio::time::timeout(Duration::from_secs(20), async {
        while handle.test_watch_folder_failures() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let found = find_child(
        &handle,
        &fixture.paired.device_token,
        failed,
        "Recovered.md",
        before,
    )
    .await;
    let complete = tokio::time::timeout(Duration::from_secs(20), async {
        while handle.known_file_scan_incomplete() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let receipt_after = handle
        .operation(fixture.paired.device_token.clone(), target.operation_id)
        .await
        .unwrap();
    let bytes = fs::read(child_path).unwrap();
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .expect("shutdown did not drain the watcher")
        .unwrap();
    assert!(
        failure,
        "overflow recovery did not exercise the targeted failure"
    );
    assert_eq!(handle.test_watch_folder_failures(), 1);
    assert!(
        found.is_some(),
        "overflow recovery did not register and scan the folder after retry"
    );
    assert!(
        complete,
        "overflow recovery stayed incomplete after the retry succeeded"
    );
    assert_eq!(receipt_after, receipt);
    assert_eq!(bytes, b"overflow recovered source\n");
}

async fn deleted_folder_does_not_remain_in_registration_retry(api_delete: bool) {
    let fixture = Fixture::new(true).await;
    let handle = fixture.runtime.handle();
    let (failed, good) = fixture.initial_folders.unwrap();
    let retry_path = fixture.dir.path().join("notes/Retry folder");
    let original_path = fixture.dir.path().join("notes/Document.md");
    let original_bytes = fs::read(&original_path).unwrap();
    handle.test_set_watch_scan_retry_delay(Duration::from_secs(10));
    handle.test_fail_watch_folder(failed, 500);
    let jobs_before = handle.test_watch_folder_jobs_done();

    let removed_at = Instant::now();
    fs::remove_dir(&retry_path).unwrap();
    tokio::time::timeout(Duration::from_millis(800), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not observe the removed folder");
    assert!(removed_at.elapsed() < Duration::from_secs(1));
    fs::create_dir(&retry_path).unwrap();
    let retry_observed_at = Instant::now();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_watch_folder_failures() == 0
            || handle.test_watch_folder_jobs_done() <= jobs_before
            || !handle.test_watch_scan_pending(failed)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("target folder did not enter registration retry");

    let before = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    if api_delete {
        let folder = handle
            .node_state(fixture.paired.device_token.clone(), failed)
            .await
            .unwrap();
        assert!(!folder.deleted);
        let receipt = handle
            .delete_node(
                fixture.paired.device_token.clone(),
                DeleteNode {
                    epoch: fixture.paired.workspace_epoch,
                    stream_id: fixture.paired.device_stream_id.parse().unwrap(),
                    operation_seq: 1,
                    operation_id: op_id(),
                    node_id: failed,
                    expected_node_etag: folder.node_etag,
                },
            )
            .await
            .unwrap();
        assert!(matches!(receipt.outcome, Outcome::Deleted(_)));
    } else {
        fs::remove_dir(&retry_path).unwrap();
    }
    assert!(
        retry_observed_at.elapsed() < Duration::from_secs(10),
        "folder deletion did not finish before the next registration retry"
    );

    let tombstoned = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let folder = handle
                .node_state(fixture.paired.device_token.clone(), failed)
                .await
                .unwrap();
            if folder.deleted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let retry_cleared = tokio::time::timeout(Duration::from_secs(5), async {
        while handle.test_watch_scan_pending(failed) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let scan_complete = tokio::time::timeout(Duration::from_secs(5), async {
        while handle.known_file_scan_incomplete() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();

    let baseline_head = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let failures_before = handle.test_watch_folder_failures();
    let events_before = handle.test_known_file_scan_events().len();
    let observed_at = Instant::now();
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let observed_ms = observed_at.elapsed().as_millis();
    let starts = handle
        .test_known_file_scan_events()
        .iter()
        .skip(events_before)
        .filter(|event| **event == "known_file_scan_started")
        .count();
    let failures_after = handle.test_watch_folder_failures();
    let after = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let folder = handle
        .node_state(fixture.paired.device_token.clone(), failed)
        .await
        .unwrap();
    let other = handle
        .node_state(fixture.paired.device_token.clone(), good)
        .await
        .unwrap();
    let changes = handle
        .changes(fixture.paired.device_token.clone(), before, Some(after))
        .await
        .unwrap();
    let pending = handle.test_watch_scan_pending(failed);
    let bytes_after = fs::read(original_path).unwrap();
    let path_exists = retry_path.exists();
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .expect("shutdown did not drain the watcher")
        .unwrap();

    assert!(tombstoned, "folder was not tombstoned");
    assert!(
        retry_cleared,
        "deleted folder remained in registration retry"
    );
    assert!(
        scan_complete,
        "scan remained incomplete after folder deletion"
    );
    assert!(!pending);
    assert_eq!(failures_after, failures_before);
    assert!(starts > 0, "periodic sweep did not run during observation");
    assert!(
        starts as u128 <= observed_ms.div_ceil(500) + 2,
        "scan ran more often than the 500 ms sweep permits"
    );
    assert_eq!(
        after, baseline_head,
        "journal changed after deletion settled"
    );
    assert!(folder.deleted);
    assert!(!other.deleted);
    assert!(!path_exists);
    assert_eq!(bytes_after, original_bytes);
    assert!(
        changes
            .nodes
            .iter()
            .any(|node| node.node_id == failed && node.deleted)
    );
    assert!(
        !changes
            .nodes
            .iter()
            .any(|node| node.node_id == failed && !node.deleted)
    );
}

#[tokio::test]
async fn externally_deleted_folder_is_removed_from_registration_retry() {
    deleted_folder_does_not_remain_in_registration_retry(false).await;
}

#[tokio::test]
async fn api_deleted_folder_is_removed_from_registration_retry() {
    deleted_folder_does_not_remain_in_registration_retry(true).await;
}

#[tokio::test]
async fn restored_folder_keeps_retry_when_restored_after_scan_page() {
    let fixture = Fixture::new(true).await;
    let handle = fixture.runtime.handle();
    let (failed, good) = fixture.initial_folders.unwrap();
    let retry_path = fixture.dir.path().join("notes/Retry folder");
    let original_path = fixture.dir.path().join("notes/Document.md");
    let original_bytes = fs::read(&original_path).unwrap();
    handle.test_set_watch_scan_retry_delay(Duration::from_secs(10));
    handle.test_fail_watch_folder(failed, 500);
    let jobs_before = handle.test_watch_folder_jobs_done();

    let removed_at = Instant::now();
    fs::remove_dir(&retry_path).unwrap();
    tokio::time::timeout(Duration::from_millis(800), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not observe the removed folder");
    assert!(removed_at.elapsed() < Duration::from_secs(1));
    fs::create_dir(&retry_path).unwrap();
    let retry_observed_at = Instant::now();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.test_watch_folder_failures() == 0
            || handle.test_watch_folder_jobs_done() <= jobs_before
            || !handle.test_watch_scan_pending(failed)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("target folder did not enter registration retry");

    let (entered, release, checked) = handle.test_pause_stale_watch_retry_cleanup(failed);
    let folder = handle
        .node_state(fixture.paired.device_token.clone(), failed)
        .await
        .unwrap();
    assert!(!folder.deleted);
    let deleted = handle
        .delete_node(
            fixture.paired.device_token.clone(),
            DeleteNode {
                epoch: fixture.paired.workspace_epoch,
                stream_id: fixture.paired.device_stream_id.parse().unwrap(),
                operation_seq: 1,
                operation_id: op_id(),
                node_id: failed,
                expected_node_etag: folder.node_etag,
            },
        )
        .await
        .unwrap();
    assert!(matches!(deleted.outcome, Outcome::Deleted(_)));
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .expect("scan did not reach stale retry cleanup")
        .expect("scan stopped before stale retry cleanup");
    let tombstone = handle
        .node_state(fixture.paired.device_token.clone(), failed)
        .await
        .unwrap();
    assert!(tombstone.deleted);
    assert!(handle.test_watch_scan_pending(failed));
    let restored = handle
        .restore_node(
            fixture.paired.device_token.clone(),
            RestoreNode {
                epoch: fixture.paired.workspace_epoch,
                stream_id: fixture.paired.device_stream_id.parse().unwrap(),
                operation_seq: 2,
                operation_id: op_id(),
                node_id: failed,
                expected_meta_rev: tombstone.meta_rev,
                parent_id: fixture.root,
                name: "Retry folder".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(restored.outcome, Outcome::Restored(_)));
    assert!(
        retry_observed_at.elapsed() < Duration::from_secs(10),
        "restore did not finish before the next registration retry"
    );
    release.send(()).unwrap();
    let retry_kept = tokio::time::timeout(Duration::from_secs(5), checked)
        .await
        .expect("scan did not complete stale retry cleanup")
        .expect("scan stopped before reporting stale retry cleanup");
    let folder_after = handle
        .node_state(fixture.paired.device_token.clone(), failed)
        .await
        .unwrap();
    let other = handle
        .node_state(fixture.paired.device_token.clone(), good)
        .await
        .unwrap();
    let bytes_after = fs::read(original_path).unwrap();
    let path_exists = retry_path.is_dir();
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .expect("shutdown did not drain the watcher")
        .unwrap();

    assert!(retry_kept, "stale page removed the restored folder retry");
    assert!(!folder_after.deleted);
    assert!(!other.deleted);
    assert!(path_exists);
    assert_eq!(bytes_after, original_bytes);
}

#[tokio::test]
async fn old_deleted_registration_result_does_not_remove_new_retry() {
    let fixture = Fixture::new(true).await;
    let handle = fixture.runtime.handle();
    let (folder_id, good) = fixture.initial_folders.unwrap();
    let folder_path = fixture.dir.path().join("notes/Retry folder");
    let original_path = fixture.dir.path().join("notes/Document.md");
    let original_bytes = fs::read(&original_path).unwrap();
    handle.test_set_watch_scan_retry_delay(Duration::from_secs(10));
    let (registration_entered, registration_release) = handle.test_pause_watch_folder();
    let (deleted_entered, deleted_release, checked) =
        handle.test_pause_deleted_folder_registration_result(folder_id);
    handle.test_request_watch_folder_from_scan(folder_id);
    let previous_retry = handle
        .test_watch_scan_pending_at(folder_id)
        .expect("initial registration request was not tracked");
    tokio::time::timeout(Duration::from_secs(5), registration_entered)
        .await
        .expect("initial registration did not reach the pause")
        .expect("initial registration stopped before the pause");

    let folder = handle
        .node_state(fixture.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    assert!(!folder.deleted);
    let deleted = handle
        .delete_node(
            fixture.paired.device_token.clone(),
            DeleteNode {
                epoch: fixture.paired.workspace_epoch,
                stream_id: fixture.paired.device_stream_id.parse().unwrap(),
                operation_seq: 1,
                operation_id: op_id(),
                node_id: folder_id,
                expected_node_etag: folder.node_etag,
            },
        )
        .await
        .unwrap();
    assert!(matches!(deleted.outcome, Outcome::Deleted(_)));
    registration_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), deleted_entered)
        .await
        .expect("registration did not confirm the folder deletion")
        .expect("registration stopped before confirming deletion");

    let tombstone = handle
        .node_state(fixture.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    assert!(tombstone.deleted);
    let restored = handle
        .restore_node(
            fixture.paired.device_token.clone(),
            RestoreNode {
                epoch: fixture.paired.workspace_epoch,
                stream_id: fixture.paired.device_stream_id.parse().unwrap(),
                operation_seq: 2,
                operation_id: op_id(),
                node_id: folder_id,
                expected_meta_rev: tombstone.meta_rev,
                parent_id: fixture.root,
                name: "Retry folder".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(restored.outcome, Outcome::Restored(_)));
    handle.test_set_watch_scan_retry_delay(Duration::ZERO);
    handle.test_request_watch_folder_from_scan(folder_id);
    let new_retry = handle
        .test_watch_scan_pending_at(folder_id)
        .expect("restored folder registration request was not tracked");
    assert!(new_retry > previous_retry, "retry was not replaced");
    deleted_release.send(()).unwrap();
    let observed_retry = tokio::time::timeout(Duration::from_secs(5), checked)
        .await
        .expect("old registration result was not processed")
        .expect("watcher stopped before reporting registration cleanup");
    let folder_after = handle
        .node_state(fixture.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let other = handle
        .node_state(fixture.paired.device_token.clone(), good)
        .await
        .unwrap();
    let bytes_after = fs::read(original_path).unwrap();
    let path_exists = folder_path.is_dir();
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .expect("shutdown did not drain the watcher")
        .unwrap();

    assert_eq!(
        observed_retry,
        Some(new_retry),
        "old deleted registration changed a newer retry"
    );
    assert!(!folder_after.deleted);
    assert!(!other.deleted);
    assert!(path_exists);
    assert_eq!(bytes_after, original_bytes);
}

#[tokio::test]
async fn replaced_folder_after_registration_is_watched_after_self_event() {
    let fixture = Fixture::new(true).await;
    let handle = fixture.runtime.handle();
    let (folder_id, good) = fixture.initial_folders.unwrap();
    let folder_path = fixture.dir.path().join("notes/Retry folder");
    let original_path = fixture.dir.path().join("notes/Document.md");
    let original_bytes = fs::read(&original_path).unwrap();
    let (registered, release_registration) =
        handle.test_pause_successful_folder_registration(folder_id);

    let first_move = Instant::now();
    fs::rename(&folder_path, fixture.dir.path().join("old-folder-one")).unwrap();
    tokio::time::timeout(Duration::from_millis(800), async {
        while !handle.known_file_watch_degraded() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not observe the first folder move");
    assert!(first_move.elapsed() < Duration::from_secs(1));
    fs::create_dir(&folder_path).unwrap();
    let wd = tokio::time::timeout(Duration::from_secs(10), registered)
        .await
        .expect("replacement folder was not registered")
        .expect("registration stopped before reporting success");

    let self_event = handle.test_watch_self_event(wd);
    let second_move = Instant::now();
    fs::rename(&folder_path, fixture.dir.path().join("old-folder-two")).unwrap();
    fs::create_dir(&folder_path).unwrap();
    assert!(second_move.elapsed() < Duration::from_secs(1));
    tokio::time::timeout(Duration::from_secs(5), self_event)
        .await
        .expect("registered watch self event was not consumed")
        .expect("watcher stopped before consuming the self event");
    let events_before = handle.test_known_file_scan_events().len();
    release_registration.send(()).unwrap();
    let scan_complete = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let events = handle.test_known_file_scan_events();
            let mut started = false;
            let mut completed = false;
            for event in events.iter().skip(events_before) {
                match *event {
                    "known_file_scan_started" => started = true,
                    "known_file_scan_complete" if started => completed = true,
                    _ => {}
                }
            }
            if completed && !handle.known_file_scan_incomplete() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();

    let before = handle
        .server_info(fixture.paired.device_token.clone())
        .await
        .unwrap()
        .head_seq;
    let name = "Watch-after-replacement.md";
    let file_event = handle.test_signal_watch_name(name);
    let file_path = folder_path.join(name);
    fs::write(&file_path, b"replacement watch source\n").unwrap();
    let event_received = tokio::time::timeout(Duration::from_secs(5), file_event)
        .await
        .is_ok_and(|result| result.is_ok());
    let found = find_child(
        &handle,
        &fixture.paired.device_token,
        folder_id,
        name,
        before,
    )
    .await;
    let content = if let Some(node) = &found {
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
    let folder = handle
        .node_state(fixture.paired.device_token.clone(), folder_id)
        .await
        .unwrap();
    let other = handle
        .node_state(fixture.paired.device_token.clone(), good)
        .await
        .unwrap();
    let bytes_after = fs::read(original_path).unwrap();
    let file_bytes = fs::read(file_path).unwrap();
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .expect("shutdown did not drain the watcher")
        .unwrap();

    assert!(scan_complete, "watcher did not finish recovery scanning");
    assert!(event_received, "replacement folder has no active watch");
    assert!(
        found.is_some(),
        "replacement folder file was not discovered"
    );
    assert_eq!(
        content.as_deref(),
        Some(b"replacement watch source\n".as_slice())
    );
    assert_eq!(file_bytes, b"replacement watch source\n");
    assert!(!folder.deleted);
    assert!(!other.deleted);
    assert_eq!(bytes_after, original_bytes);
}
