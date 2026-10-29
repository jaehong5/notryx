use super::{
    RuntimeError, WorkspaceRuntime,
    journal_retention_test_support::{
        RetentionPauseStage as PauseStage, RetentionTestControl, RetentionTestEvent as TestEvent,
        RetentionTestPause as TestPause,
    },
};
use crate::{Document, Error, Outcome, SaveContent, Workspace, config::WorkspaceConfig};
use notryx_domain::ChangeSeq;
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const FIRST: &[u8] = b"synthetic original\r\n";
const SAVED: &[u8] = b"\xef\xbb\xbf# synthetic saved\n";
const RETENTION_SECONDS: u64 = 86_400;
const BUDGET_BYTES: u64 = 512 * 1024 * 1024;

async fn start_test_runtime(
    config: WorkspaceConfig,
    clock: Arc<AtomicI64>,
    retention_seconds: u64,
    budget_bytes: u64,
    interval: Duration,
    events: mpsc::UnboundedSender<TestEvent>,
    pause: Option<TestPause>,
) -> Result<WorkspaceRuntime, RuntimeError> {
    WorkspaceRuntime::test_start_with_retention_control(
        config,
        RetentionTestControl {
            clock,
            interval,
            retention_seconds,
            budget_bytes,
            events,
            pause,
        },
    )
    .await
}

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

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    workspace: Vec<String>,
    baseline: Vec<String>,
    changes: Vec<String>,
    node_changes: Vec<String>,
    nodes: Vec<String>,
    receipts: Vec<String>,
    streams: Vec<String>,
    body: Vec<u8>,
    objects: BTreeMap<PathBuf, Vec<u8>>,
}

fn object_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

async fn snapshot(config: &WorkspaceConfig) -> Snapshot {
    let mut connection = connect(&config.state_root).await;
    let mut tx = connection.begin().await.unwrap();
    let mut tables = Vec::new();
    for query in [
        "SELECT json_array(singleton,workspace_id,epoch,root_id,head_seq,journal_floor_seq,journal_baseline_count,journal_baseline_hash) FROM workspaces",
        "SELECT json_array(node_id,seq,post_state) FROM journal_baseline ORDER BY node_id",
        "SELECT json_array(seq,operation_id,committed_at) FROM change_sets ORDER BY seq",
        "SELECT json_array(seq,node_id,post_state) FROM node_changes ORDER BY seq,node_id",
        "SELECT json_array(node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,deleted) FROM nodes ORDER BY node_id",
        "SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id",
        "SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id",
    ] {
        tables.push(
            sqlx::query_scalar::<_, String>(query)
                .fetch_all(&mut *tx)
                .await
                .unwrap(),
        );
    }
    tx.commit().await.unwrap();
    connection.close().await.unwrap();
    let mut tables = tables.into_iter();
    Snapshot {
        workspace: tables.next().unwrap(),
        baseline: tables.next().unwrap(),
        changes: tables.next().unwrap(),
        node_changes: tables.next().unwrap(),
        nodes: tables.next().unwrap(),
        receipts: tables.next().unwrap(),
        streams: tables.next().unwrap(),
        body: fs::read(config.notes_root.join("Synthetic.md")).unwrap(),
        objects: object_bytes(&config.state_root.join("objects")),
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    config: WorkspaceConfig,
    epoch: notryx_domain::WorkspaceEpoch,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("state"),
            uploads: Default::default(),
        };
        fs::create_dir(&config.notes_root).unwrap();
        fs::create_dir(&config.state_root).unwrap();
        let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
            .await
            .unwrap();
        fs::write(config.notes_root.join("Synthetic.md"), FIRST).unwrap();
        let document: Document = workspace.adopt_root_document("Synthetic.md").await.unwrap();
        let stream = workspace.register_stream().await.unwrap();
        let receipt = workspace
            .save(SaveContent {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: Sha256::digest(SAVED)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                bytes: SAVED.to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(receipt.outcome, Outcome::Saved(_)));
        let epoch = workspace.epoch();
        assert_eq!(
            workspace.server_info().await.unwrap().head_seq,
            ChangeSeq::new(2)
        );
        workspace.close().await.unwrap();
        let mut connection = connect(&config.state_root).await;
        let times: Vec<Option<i64>> =
            sqlx::query_scalar("SELECT committed_at FROM change_sets ORDER BY seq")
                .fetch_all(&mut connection)
                .await
                .unwrap();
        assert_eq!(times, vec![None, None]);
        connection.close().await.unwrap();
        Self {
            _directory: directory,
            config,
            epoch,
        }
    }

    async fn mark_expired(&self) {
        let mut connection = connect(&self.config.state_root).await;
        sqlx::query("UPDATE change_sets SET committed_at=1")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
    }

    async fn assert_reopens(&self, expected_floor: u64) {
        let mut reopened = Workspace::open(&self.config.notes_root, &self.config.state_root)
            .await
            .unwrap();
        let info = reopened.server_info().await.unwrap();
        assert_eq!(info.workspace_epoch, self.epoch);
        assert_eq!(info.head_seq, ChangeSeq::new(2));
        assert_eq!(info.journal_floor_seq, ChangeSeq::new(expected_floor));
        reopened.close().await.unwrap();
    }
}

async fn expect_event(events: &mut mpsc::UnboundedReceiver<TestEvent>, expected: TestEvent) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if event == expected {
                return;
            }
        }
        panic!("retention event stream closed");
    })
    .await
    .expect("automatic retention did not report expected event");
}

async fn expect_success_without_failure(events: &mut mpsc::UnboundedReceiver<TestEvent>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            match event {
                TestEvent::Started => continue,
                TestEvent::Success => return,
                TestEvent::Failure(error) => panic!("retention failed before success: {error}"),
            }
        }
        panic!("retention event stream closed");
    })
    .await
    .expect("automatic retention did not report success");
}

#[tokio::test]
async fn journal_retention_timer_samples_clock_inside_coordinator_after_existing_commit() {
    let fixture = Fixture::new().await;
    let clock = Arc::new(AtomicI64::new(10_000));
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let runtime = start_test_runtime(
        fixture.config.clone(),
        Arc::clone(&clock),
        RETENTION_SECONDS,
        BUDGET_BYTES,
        Duration::from_secs(60),
        events_tx,
        Some(TestPause {
            stage: PauseStage::BeforeClock,
            entered: entered_tx,
            release: release_rx,
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    clock.store(42_000, Ordering::SeqCst);
    release_tx.send(()).unwrap();
    expect_success_without_failure(&mut events_rx).await;
    let mut connection = connect(&fixture.config.state_root).await;
    let times: Vec<Option<i64>> =
        sqlx::query_scalar("SELECT committed_at FROM change_sets ORDER BY seq")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(times, vec![Some(42_001), Some(42_001)]);
    let floor: String = sqlx::query_scalar("SELECT journal_floor_seq FROM workspaces")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(floor, "0");
    connection.close().await.unwrap();
    runtime.shutdown().await.unwrap();
    fixture.assert_reopens(0).await;
    assert_eq!(
        fs::read(fixture.config.notes_root.join("Synthetic.md")).unwrap(),
        SAVED
    );
}

#[tokio::test]
async fn journal_retention_timer_failure_rolls_back_and_later_tick_recovers() {
    let fixture = Fixture::new().await;
    fixture.mark_expired().await;
    let mut connection = connect(&fixture.config.state_root).await;
    sqlx::raw_sql("CREATE TRIGGER fail_retention BEFORE UPDATE OF journal_floor_seq ON workspaces BEGIN SELECT RAISE(FAIL,'synthetic retention failure'); END")
        .execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    let before = snapshot(&fixture.config).await;
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let runtime = start_test_runtime(
        fixture.config.clone(),
        clock,
        RETENTION_SECONDS,
        BUDGET_BYTES,
        Duration::from_millis(100),
        events_tx,
        None,
    )
    .await
    .unwrap();
    expect_event(&mut events_rx, TestEvent::Failure(Error::Storage)).await;
    assert_eq!(snapshot(&fixture.config).await, before);
    let mut connection = connect(&fixture.config.state_root).await;
    sqlx::raw_sql("DROP TRIGGER fail_retention")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    expect_event(&mut events_rx, TestEvent::Success).await;
    let after = snapshot(&fixture.config).await;
    assert_eq!(after.body, before.body);
    assert_eq!(after.objects, before.objects);
    assert_eq!(after.receipts, before.receipts);
    assert_eq!(after.streams, before.streams);
    runtime.shutdown().await.unwrap();
    fixture.assert_reopens(2).await;
}

#[tokio::test]
async fn journal_retention_timer_shutdown_waits_for_inflight_transaction_and_reopens() {
    let fixture = Fixture::new().await;
    fixture.mark_expired().await;
    let before = snapshot(&fixture.config).await;
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let runtime = start_test_runtime(
        fixture.config.clone(),
        clock,
        RETENTION_SECONDS,
        BUDGET_BYTES,
        Duration::from_secs(60),
        events_tx,
        Some(TestPause {
            stage: PauseStage::BeforeCommit,
            entered: entered_tx,
            release: release_rx,
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot(&fixture.config).await, before);
    let mut shutdown = tokio::spawn(runtime.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut shutdown)
            .await
            .is_err()
    );
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fixture.assert_reopens(2).await;
    let after = snapshot(&fixture.config).await;
    assert_eq!(after.body, before.body);
    assert_eq!(after.objects, before.objects);
    assert_eq!(after.receipts, before.receipts);
    assert_eq!(after.streams, before.streams);
}
