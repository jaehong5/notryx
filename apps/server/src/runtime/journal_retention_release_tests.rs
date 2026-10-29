use super::{
    Command, RuntimeError, WorkspaceRuntime,
    journal_retention_test_support::{
        RetentionPauseStage, RetentionTestControl, RetentionTestEvent, RetentionTestPause,
    },
    now,
};
use crate::{AckOperations, Error, Outcome, SaveContent, Workspace, config::WorkspaceConfig};
use notryx_protocol::{AdminRequest, CONTRACT, PairRequest};
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

#[derive(Clone, Copy)]
enum Action {
    Ack,
    Release,
    Abort,
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

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn exercise(action: Action, corrupt: bool) {
    let directory = tempfile::tempdir().unwrap();
    let config = WorkspaceConfig {
        notes_root: directory.path().join("notes"),
        state_root: directory.path().join("state"),
        uploads: Default::default(),
    };
    fs::create_dir(&config.notes_root).unwrap();
    fs::create_dir(&config.state_root).unwrap();
    let path = config.notes_root.join("Synthetic.md");
    let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
        .await
        .unwrap();
    let original = b"synthetic original\n";
    let saved = b"synthetic saved\n";
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
    let receipt = workspace
        .save(SaveContent {
            epoch,
            stream_id,
            operation_seq: 1,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_content_rev: document.content_rev,
            content_hash: digest(saved),
            bytes: saved.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, Outcome::Saved(_)));
    workspace.close().await.unwrap();

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
            pause: if corrupt {
                Some(RetentionTestPause {
                    stage: RetentionPauseStage::BeforeClock,
                    entered,
                    release: release_receiver,
                })
            } else {
                None
            },
        },
    )
    .await
    .unwrap();
    if corrupt {
        tokio::time::timeout(Duration::from_secs(10), entered_receiver)
            .await
            .unwrap()
            .unwrap();
    }
    let recovery_id: crate::OperationId = Uuid::now_v7().to_string().parse().unwrap();
    let upload_id = Uuid::new_v4();
    let mut db = connect(&config.state_root).await;
    match action {
        Action::Release => {
            sqlx::query("INSERT INTO recovery_cases(operation_id,intent,paths,resolved,resolved_head) VALUES(?, '{}', '[]', 1, '2')")
                .bind(recovery_id.to_string()).execute(&mut db).await.unwrap();
            sqlx::query("INSERT INTO recovery_objects(operation_id,hash) VALUES(?,?)")
                .bind(recovery_id.to_string())
                .bind(digest(original))
                .execute(&mut db)
                .await
                .unwrap();
        }
        Action::Abort => {
            sqlx::query("INSERT INTO uploads(upload_id,device_id,status,expected_hash,declared_size,reserved_bytes,started_at,expires_at) VALUES(?,?,'receiving',?,3,3,?,?)")
                .bind(upload_id.to_string()).bind(&device.device_id).bind(digest(b"abc"))
                .bind(timestamp).bind(timestamp + crate::upload::RECEIVE_SECONDS)
                .execute(&mut db).await.unwrap();
            fs::write(
                config
                    .state_root
                    .join("staging")
                    .join(format!("upload-{upload_id}.part")),
                b"abc",
            )
            .unwrap();
        }
        Action::Ack => {}
    }
    if corrupt {
        let changed = sqlx::query(
            "UPDATE node_changes SET post_state='{\"invalid\":true}' WHERE seq='1' AND node_id=?",
        )
        .bind(document.node_id.to_string())
        .execute(&mut db)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(changed, 1);
    }
    db.close().await.unwrap();
    if corrupt {
        release.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = event_receiver.recv().await {
                if event != RetentionTestEvent::Started {
                    return event;
                }
            }
            panic!("retention event stream closed");
        })
        .await
        .unwrap();
        assert_eq!(result, RetentionTestEvent::Failure(Error::InvalidFormat));
    }
    let handle = runtime.handle();
    let result = match action {
        Action::Ack => handle
            .ack_operations(
                device.device_token.clone(),
                AckOperations {
                    epoch,
                    stream_id,
                    ack_through: 1,
                },
            )
            .await
            .map(|_| ()),
        Action::Release => handle
            .admin(AdminRequest::RecoveryRelease {
                operation_id: recovery_id,
            })
            .await
            .map(|_| ()),
        Action::Abort => {
            let (reply, response) = oneshot::channel();
            handle
                .enqueue(Command::AbortUpload {
                    id: upload_id,
                    reply,
                })
                .unwrap();
            response.await.unwrap()
        }
    };
    if corrupt {
        assert!(
            matches!(
                result,
                Err(RuntimeError::Workspace(Error::RecoveryRequired))
            ),
            "{result:?}"
        );
    } else {
        assert!(result.is_ok(), "{result:?}");
    }
    let mut db = connect(&config.state_root).await;
    match action {
        Action::Ack => {
            let retired: String = sqlx::query_scalar(
                "SELECT retired_through FROM operation_streams WHERE stream_id=?",
            )
            .bind(stream_id.to_string())
            .fetch_one(&mut db)
            .await
            .unwrap();
            let receipts: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts WHERE stream_id=?")
                    .bind(stream_id.to_string())
                    .fetch_one(&mut db)
                    .await
                    .unwrap();
            assert_eq!(
                (retired.as_str(), receipts),
                if corrupt { ("0", 1) } else { ("1", 0) }
            );
        }
        Action::Release => {
            let refs: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM recovery_objects WHERE operation_id=?")
                    .bind(recovery_id.to_string())
                    .fetch_one(&mut db)
                    .await
                    .unwrap();
            let cases: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM recovery_cases WHERE operation_id=?")
                    .bind(recovery_id.to_string())
                    .fetch_one(&mut db)
                    .await
                    .unwrap();
            assert_eq!((cases, refs), if corrupt { (1, 1) } else { (0, 0) });
        }
        Action::Abort => {
            let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM uploads WHERE upload_id=?")
                .bind(upload_id.to_string())
                .fetch_one(&mut db)
                .await
                .unwrap();
            let stage = config
                .state_root
                .join("staging")
                .join(format!("upload-{upload_id}.part"));
            assert_eq!(rows, if corrupt { 1 } else { 0 });
            if corrupt {
                assert_eq!(fs::read(stage).unwrap(), b"abc");
            } else {
                assert!(!stage.exists());
            }
        }
    }
    db.close().await.unwrap();
    assert_eq!(fs::read(path).unwrap(), saved);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn retention_invalid_blocks_ack_and_preserves_receipt() {
    exercise(Action::Ack, false).await;
    exercise(Action::Ack, true).await;
}

#[tokio::test]
async fn retention_invalid_blocks_recovery_object_release() {
    exercise(Action::Release, false).await;
    exercise(Action::Release, true).await;
}

#[tokio::test]
async fn retention_invalid_blocks_upload_abort_and_preserves_staging() {
    exercise(Action::Abort, false).await;
    exercise(Action::Abort, true).await;
}
