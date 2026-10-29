use super::{
    RuntimeError, WorkspaceRuntime,
    journal_retention_test_support::{RetentionTestControl, RetentionTestEvent},
    now,
};
use crate::{Error, Outcome, SaveContent, Workspace, config::WorkspaceConfig};
use notryx_protocol::{AdminRequest, AdminResult, CONTRACT, PairRequest};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    fs,
    sync::{Arc, atomic::AtomicI64},
    time::Duration,
};
use tokio::sync::mpsc;
use uuid::Uuid;

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Copy)]
enum Detection {
    JournalStatus,
    ServerInfo,
}

#[tokio::test]
async fn journal_status_invalid_baseline_disables_writes_and_preserves_reads() {
    check_detected_corruption(Detection::JournalStatus).await;
}

#[tokio::test]
async fn server_info_invalid_position_disables_writes_and_preserves_reads() {
    check_detected_corruption(Detection::ServerInfo).await;
}

async fn check_detected_corruption(detection: Detection) {
    let directory = tempfile::tempdir().unwrap();
    let config = WorkspaceConfig {
        notes_root: directory.path().join("notes"),
        state_root: directory.path().join("state"),
        uploads: Default::default(),
    };
    fs::create_dir(&config.notes_root).unwrap();
    fs::create_dir(&config.state_root).unwrap();
    let path = config.notes_root.join("Synthetic.md");
    let saved = b"synthetic committed\n";
    let attempted = b"synthetic must not be written\n";
    let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    fs::write(&path, b"synthetic initial\n").unwrap();
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

    let (events, mut event_receiver) = mpsc::unbounded_channel();
    let runtime = WorkspaceRuntime::test_start_with_retention_control(
        config.clone(),
        RetentionTestControl {
            clock: Arc::new(AtomicI64::new(1_000_000)),
            interval: Duration::from_secs(3600),
            retention_seconds: 86_400,
            budget_bytes: 512 * 1024 * 1024,
            events,
            pause: None,
        },
    )
    .await
    .unwrap();
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
    assert_eq!(event, RetentionTestEvent::Success);
    let handle = runtime.handle();
    assert!(handle.health().await.unwrap().ready);
    assert!(matches!(
        handle.admin(AdminRequest::JournalStatus {}).await,
        Ok(AdminResult::JournalStatus { .. })
    ));
    let operation_id = Uuid::now_v7().to_string().parse().unwrap();
    let receipt = handle
        .save_content(
            device.device_token.clone(),
            SaveContent {
                epoch,
                stream_id,
                operation_seq: 1,
                operation_id,
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: hash(saved),
                bytes: saved.to_vec(),
            },
        )
        .await
        .unwrap();
    let Outcome::Saved(saved_document) = receipt.outcome else {
        panic!("synthetic initial save failed");
    };
    assert_eq!(fs::read(&path).unwrap(), saved);

    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.state_root.join("state.db"))
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(3)),
    )
    .await
    .unwrap();
    let corrupt = match detection {
        Detection::JournalStatus => "UPDATE journal_baseline SET post_state='{\"invalid\":true}'",
        Detection::ServerInfo => "UPDATE workspaces SET journal_floor_seq='3' WHERE singleton=1",
    };
    let changed = sqlx::query(corrupt)
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
    let before_receipts: Vec<String> = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    let before_streams: Vec<String> = sqlx::query_scalar("SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id")
        .fetch_all(&mut db).await.unwrap();
    assert_eq!(before_receipts.len(), 1);
    db.close().await.unwrap();

    match detection {
        Detection::JournalStatus => assert!(matches!(
            handle.admin(AdminRequest::JournalStatus {}).await,
            Err(RuntimeError::Workspace(Error::InvalidFormat))
        )),
        Detection::ServerInfo => assert!(matches!(
            handle.server_info(device.device_token.clone()).await,
            Err(RuntimeError::Workspace(Error::InvalidFormat))
        )),
    }
    let health = handle.health().await.unwrap();
    let write = handle
        .save_content(
            device.device_token.clone(),
            SaveContent {
                epoch,
                stream_id,
                operation_seq: 2,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: saved_document.node_id,
                expected_content_rev: saved_document.content_rev,
                content_hash: hash(attempted),
                bytes: attempted.to_vec(),
            },
        )
        .await;
    let (_, content) = handle
        .content(device.device_token.clone(), document.node_id, None)
        .await
        .unwrap();
    let recovered = handle
        .operation(device.device_token.clone(), operation_id)
        .await
        .unwrap();
    let diagnostic = handle.admin(AdminRequest::Status {}).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(config.state_root.join("state.db"))
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(3)),
    )
    .await
    .unwrap();
    let after_receipts: Vec<String> = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    let after_streams: Vec<String> = sqlx::query_scalar("SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(
        !health.ready
            && matches!(write, Err(RuntimeError::Workspace(Error::RecoveryRequired)))
            && content == saved
            && fs::read(&path).unwrap() == saved
            && matches!(recovered.outcome, Outcome::Saved(_))
            && matches!(diagnostic, Ok(AdminResult::Status { .. }))
            && before_receipts == after_receipts
            && before_streams == after_streams,
        "detected invalid journal did not protect writes and preserve reads"
    );
    runtime.shutdown().await.unwrap();
}
