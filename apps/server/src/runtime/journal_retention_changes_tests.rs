use super::{
    RuntimeError, WorkspaceRuntime,
    journal_retention_test_support::{
        RetentionPauseStage, RetentionTestControl, RetentionTestEvent, RetentionTestPause,
    },
    now,
};
use crate::{Error, Outcome, SaveContent, Workspace, config::WorkspaceConfig};
use notryx_domain::ChangeSeq;
use notryx_protocol::{AdminRequest, AdminResult, CONTRACT, PairRequest};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    fs,
    sync::{Arc, atomic::AtomicI64},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[tokio::test]
async fn detected_invalid_journal_blocks_every_changes_range_but_keeps_other_reads() {
    let directory = tempfile::tempdir().unwrap();
    let config = WorkspaceConfig {
        notes_root: directory.path().join("notes"),
        state_root: directory.path().join("state"),
        uploads: Default::default(),
    };
    fs::create_dir(&config.notes_root).unwrap();
    fs::create_dir(&config.state_root).unwrap();
    let path = config.notes_root.join("Synthetic.md");
    let original = b"synthetic original\n";
    let saved = b"synthetic saved\n";
    let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    fs::write(&path, original).unwrap();
    let document = workspace.adopt_root_document("Synthetic.md").await.unwrap();
    let principal = workspace.auth_status().await.unwrap().owner_principal_id;
    let timestamp = now().unwrap();
    let grant = workspace.issue_pairing(principal, timestamp).await.unwrap();
    let device = workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            timestamp,
        )
        .await
        .unwrap();
    let stream_id = Uuid::parse_str(&device.device_stream_id).unwrap();
    let operation_id = Uuid::now_v7().to_string().parse().unwrap();
    let receipt = workspace
        .save(SaveContent {
            epoch: workspace.epoch(),
            stream_id,
            operation_seq: 1,
            operation_id,
            node_id: document.node_id,
            expected_content_rev: document.content_rev,
            content_hash: Sha256::digest(saved)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: saved.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, Outcome::Saved(_)));
    workspace.close().await.unwrap();

    let valid_runtime = WorkspaceRuntime::start(config.clone()).await.unwrap();
    let valid_handle = valid_runtime.handle();
    let page = valid_handle
        .changes(
            device.device_token.clone(),
            ChangeSeq::new(0),
            Some(ChangeSeq::new(1)),
        )
        .await
        .unwrap();
    assert_eq!(page.covered_through, ChangeSeq::new(1));
    assert_eq!(page.through_seq, ChangeSeq::new(1));
    assert!(!page.nodes.is_empty());
    assert!(
        valid_handle
            .server_info(device.device_token.clone())
            .await
            .unwrap()
            .ready
    );
    valid_runtime.shutdown().await.unwrap();

    let (events, mut event_receiver) = mpsc::unbounded_channel();
    let (entered, entered_receiver) = oneshot::channel();
    let (release, release_receiver) = oneshot::channel();
    let runtime = WorkspaceRuntime::test_start_with_retention_control(
        config.clone(),
        RetentionTestControl {
            clock: Arc::new(AtomicI64::new(1_000_000)),
            interval: Duration::from_secs(60),
            retention_seconds: 86_400,
            budget_bytes: 512 * 1024 * 1024,
            events,
            pause: Some(RetentionTestPause {
                stage: RetentionPauseStage::BeforeClock,
                entered,
                release: release_receiver,
            }),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered_receiver)
        .await
        .unwrap()
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.state_root.join("state.db"))
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(3)),
    )
    .await
    .unwrap();
    let changed = sqlx::query(
        "UPDATE node_changes SET post_state='{\"invalid\":true}' WHERE seq='2' AND node_id=?",
    )
    .bind(document.node_id.to_string())
    .execute(&mut db)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(changed, 1);
    db.close().await.unwrap();
    release.send(()).unwrap();
    let event = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = event_receiver.recv().await {
            if event != RetentionTestEvent::Started {
                return event;
            }
        }
        panic!("retention event stream closed");
    })
    .await
    .unwrap();
    assert_eq!(event, RetentionTestEvent::Failure(Error::InvalidFormat));

    let handle = runtime.handle();
    let first_range = handle
        .changes(
            device.device_token.clone(),
            ChangeSeq::new(0),
            Some(ChangeSeq::new(1)),
        )
        .await;
    let all = handle
        .changes(device.device_token.clone(), ChangeSeq::new(0), None)
        .await;
    let corrupt_range = handle
        .changes(
            device.device_token.clone(),
            ChangeSeq::new(1),
            Some(ChangeSeq::new(2)),
        )
        .await;
    let empty_range = handle
        .changes(device.device_token.clone(), ChangeSeq::new(2), None)
        .await;
    let info = handle
        .server_info(device.device_token.clone())
        .await
        .unwrap();
    let (_, bytes) = handle
        .content(device.device_token.clone(), document.node_id, None)
        .await
        .unwrap();
    let recovered_receipt = handle
        .operation(device.device_token.clone(), operation_id)
        .await
        .unwrap();
    let diagnostic = handle.admin(AdminRequest::JournalStatus {}).await;
    for result in [first_range, all, corrupt_range, empty_range] {
        assert!(
            matches!(
                result,
                Err(RuntimeError::Workspace(Error::RecoveryRequired))
            ),
            "{result:?}"
        );
    }
    assert!(!info.ready);
    assert_eq!(bytes, saved);
    assert_eq!(fs::read(&path).unwrap(), saved);
    assert!(matches!(recovered_receipt.outcome, Outcome::Saved(_)));
    assert!(matches!(diagnostic, Ok(AdminResult::JournalStatus { .. })));
    runtime.shutdown().await.unwrap();
}
