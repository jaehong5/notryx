use super::{
    RuntimeError, WorkspaceRuntime,
    journal_retention_test_support::{
        RetentionPauseStage, RetentionTestControl, RetentionTestEvent, RetentionTestPause,
    },
    now,
};
use crate::{Error, Outcome, SaveContent, Workspace, config::WorkspaceConfig};
use notryx_domain::ChangeSeq;
use notryx_protocol::{CONTRACT, PairRequest};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicI64},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

async fn connect(state: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(state.join("state.db"))
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(3)),
    )
    .await
    .unwrap()
}

async fn committed_rows(state: &Path) -> (String, Vec<String>, Vec<String>) {
    let mut db = connect(state).await;
    let mut tx = db.begin().await.unwrap();
    let head = sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton=1")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let receipts = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut *tx).await.unwrap();
    let streams = sqlx::query_scalar("SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id")
        .fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    (head, receipts, streams)
}

fn content_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn journal_retention_invalid_detected_corruption_blocks_health_and_new_write() {
    let directory = tempfile::tempdir().unwrap();
    let config = WorkspaceConfig {
        notes_root: directory.path().join("notes"),
        state_root: directory.path().join("state"),
        uploads: Default::default(),
    };
    fs::create_dir(&config.notes_root).unwrap();
    fs::create_dir(&config.state_root).unwrap();
    let original = b"synthetic original\n";
    let saved = b"synthetic saved and committed\n";
    let attempted = b"synthetic must not be written\n";
    let path = config.notes_root.join("Synthetic.md");
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
    let epoch = workspace.epoch();
    workspace.close().await.unwrap();
    let initial_runtime = WorkspaceRuntime::start(config.clone()).await.unwrap();
    let initial_handle = initial_runtime.handle();
    assert!(initial_handle.health().await.unwrap().ready);
    let first = initial_handle
        .save_content(
            device.device_token.clone(),
            SaveContent {
                epoch,
                stream_id,
                operation_seq: 1,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: content_hash(saved),
                bytes: saved.to_vec(),
            },
        )
        .await
        .unwrap();
    let Outcome::Saved(saved_document) = first.outcome else {
        panic!("synthetic initial save failed");
    };
    assert_eq!(
        initial_handle
            .server_info(device.device_token.clone())
            .await
            .unwrap()
            .head_seq,
        ChangeSeq::new(2)
    );
    initial_runtime.shutdown().await.unwrap();
    let mut valid = Workspace::open(&config.notes_root, &config.state_root)
        .await
        .expect("uncorrupted synthetic workspace must reopen");
    assert!(valid.ready());
    assert_eq!(valid.read(saved_document.node_id).await.unwrap().1, saved);
    valid.close().await.unwrap();

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
    let mut db = connect(&config.state_root).await;
    let changed = sqlx::query(
        "UPDATE node_changes SET post_state='{\"invalid\":true}' WHERE seq='1' AND node_id=?",
    )
    .bind(saved_document.node_id.to_string())
    .execute(&mut db)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(changed, 1);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut db)
            .await
            .unwrap()
            .is_empty()
    );
    db.close().await.unwrap();
    let before = committed_rows(&config.state_root).await;
    assert_eq!(before.0, "2");
    assert_eq!(before.1.len(), 1);
    assert_eq!(fs::read(&path).unwrap(), saved);
    release.send(()).unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = event_receiver.recv().await {
            match event {
                RetentionTestEvent::Started => continue,
                other => return other,
            }
        }
        panic!("retention event stream closed");
    })
    .await
    .expect("automatic retention did not finish");
    assert_eq!(observed, RetentionTestEvent::Failure(Error::InvalidFormat));

    let handle = runtime.handle();
    let health = handle.health().await.unwrap();
    let attempt = handle
        .save_content(
            device.device_token.clone(),
            SaveContent {
                epoch,
                stream_id,
                operation_seq: 2,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: saved_document.node_id,
                expected_content_rev: saved_document.content_rev,
                content_hash: content_hash(attempted),
                bytes: attempted.to_vec(),
            },
        )
        .await;
    let blocked = matches!(
        attempt,
        Err(RuntimeError::Workspace(Error::RecoveryRequired))
    );
    let after = committed_rows(&config.state_root).await;
    let actual = fs::read(&path).unwrap();
    assert!(
        !health.ready && blocked && after == before && actual == saved,
        "detected invalid journal must disable readiness and new writes while preserving committed data"
    );
    runtime.shutdown().await.unwrap();
    assert!(matches!(
        Workspace::open(&config.notes_root, &config.state_root).await,
        Err(Error::InvalidFormat)
    ));
    assert_eq!(fs::read(&path).unwrap(), saved);
    assert_eq!(committed_rows(&config.state_root).await, before);
}
