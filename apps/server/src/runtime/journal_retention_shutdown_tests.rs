use super::{
    Command, WorkspaceRuntime,
    journal_retention_test_support::{
        RetentionPauseStage, RetentionTestControl, RetentionTestEvent, RetentionTestPause,
    },
};
use crate::{Workspace, config::WorkspaceConfig};
use notryx_domain::ChangeSeq;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    fs,
    sync::{Arc, atomic::AtomicI64},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

#[tokio::test]
async fn journal_retention_shutdown_does_not_start_a_new_tick_after_begin_shutdown() {
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
    let content = b"synthetic shutdown checkpoint\n";
    fs::write(config.notes_root.join("Synthetic.md"), content).unwrap();
    workspace.adopt_root_document("Synthetic.md").await.unwrap();
    let epoch = workspace.epoch();
    assert_eq!(
        workspace.server_info().await.unwrap().head_seq,
        ChangeSeq::new(1)
    );
    workspace.close().await.unwrap();

    let interval = Duration::from_millis(20);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let control = RetentionTestControl {
        clock: Arc::new(AtomicI64::new(42_000)),
        interval,
        retention_seconds: 86_400,
        budget_bytes: 512 * 1024 * 1024,
        events: events_tx,
        pause: Some(RetentionTestPause {
            stage: RetentionPauseStage::BeforeCommit,
            entered: entered_tx,
            release: release_rx,
        }),
    };
    let mut runtime = WorkspaceRuntime::test_start_with_retention_control(config.clone(), control)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), events_rx.recv())
            .await
            .unwrap(),
        Some(RetentionTestEvent::Started)
    );
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let deadline = tokio::time::Instant::now() + interval * 2;
    runtime.begin_shutdown();

    // The inert barrier enters the worker directly after admission has been closed.
    let (barrier_entered_tx, barrier_entered_rx) = oneshot::channel();
    let (barrier_release_tx, barrier_release_rx) = oneshot::channel();
    runtime
        .handle
        .commands
        .send(Command::Barrier {
            entered: barrier_entered_tx,
            release: barrier_release_rx,
        })
        .await
        .unwrap();
    tokio::time::sleep_until(deadline).await;
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), barrier_entered_rx)
        .await
        .unwrap()
        .unwrap();

    let mut observed = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        observed.push(event);
    }
    assert_eq!(
        observed,
        vec![RetentionTestEvent::Success],
        "only the tick already in progress may finish"
    );
    barrier_release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.state_root.join("state.db"))
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let timestamps: Vec<Option<i64>> = sqlx::query_scalar("SELECT committed_at FROM change_sets")
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert_eq!(timestamps, vec![Some(42_001)]);
    connection.close().await.unwrap();
    let mut reopened = Workspace::open(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    let info = reopened.server_info().await.unwrap();
    assert_eq!(info.workspace_epoch, epoch);
    assert_eq!(info.head_seq, ChangeSeq::new(1));
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(
        fs::read(config.notes_root.join("Synthetic.md")).unwrap(),
        content
    );
    reopened.close().await.unwrap();
}
