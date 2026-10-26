use super::*;
use sha2::{Digest, Sha256};
use std::fs;

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn op_id() -> OperationId {
    Uuid::now_v7().to_string().parse().unwrap()
}
struct Rig {
    dir: tempfile::TempDir,
    workspace: Workspace,
    initial: Document,
    stream: Uuid,
}
impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let state = dir.path().join("state");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
        fs::write(
            notes.join("Document.md"),
            b"\xef\xbb\xbf# Original\r\n\r\n<!-- untouched -->\r\n",
        )
        .unwrap();
        let initial = workspace.adopt_root_document("Document.md").await.unwrap();
        let stream = workspace.register_stream().await.unwrap();
        Self {
            dir,
            workspace,
            initial,
            stream,
        }
    }
    fn command(&self, bytes: &[u8]) -> SaveContent {
        SaveContent {
            epoch: self.workspace.epoch(),
            stream_id: self.stream,
            operation_seq: 1,
            operation_id: op_id(),
            node_id: self.initial.node_id,
            expected_content_rev: self.initial.content_rev.clone(),
            content_hash: digest(bytes),
            bytes: bytes.to_vec(),
        }
    }
}

#[tokio::test]
async fn conditional_save_preserves_exact_bytes_and_commits_one_post_state() {
    let mut rig = Rig::new().await;
    let bytes = b"\xef\xbb\xbf# Changed\r\n\r\n<!-- untouched -->\r\n";
    let receipt = rig.workspace.save(rig.command(bytes)).await.unwrap();
    let Outcome::Saved(saved) = receipt.outcome else {
        panic!("expected saved receipt")
    };
    assert_ne!(saved.content_rev, rig.initial.content_rev);
    assert_eq!(saved.meta_rev, rig.initial.meta_rev);
    assert_eq!(
        saved.last_change_seq.get(),
        rig.initial.last_change_seq.get() + 1
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        bytes
    );
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert!(reopened.ready());
    let (metadata, content) = reopened.read(saved.node_id).await.unwrap();
    assert_eq!(metadata, saved);
    assert_eq!(content, bytes);
}

#[tokio::test]
async fn retry_returns_original_receipt_and_changed_payload_is_rejected() {
    let mut rig = Rig::new().await;
    let command = rig.command(b"replacement\n");
    let receipt = rig.workspace.save(command.clone()).await.unwrap();
    assert_eq!(rig.workspace.save(command.clone()).await.unwrap(), receipt);
    let mut changed = command;
    changed.bytes = b"another body\n".to_vec();
    changed.content_hash = digest(&changed.bytes);
    assert_eq!(
        rig.workspace.save(changed).await,
        Err(Error::IdempotencyMismatch)
    );
}

#[tokio::test]
async fn two_streams_with_same_base_commit_once_and_preserve_failed_receipt() {
    let mut rig = Rig::new().await;
    let mut second = rig.command(b"second writer\n");
    second.stream_id = rig.workspace.register_stream().await.unwrap();
    let first = rig
        .workspace
        .save(rig.command(b"first writer\n"))
        .await
        .unwrap();
    let Outcome::Saved(saved) = first.outcome else {
        panic!("expected saved")
    };
    let rejected = rig.workspace.save(second.clone()).await.unwrap();
    assert_eq!(
        rejected.outcome,
        Outcome::PreconditionFailed {
            current: saved.content_rev
        }
    );
    assert_eq!(rig.workspace.save(second).await.unwrap(), rejected);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"first writer\n"
    );
}

#[tokio::test]
async fn invalid_digest_epoch_or_sequence_does_not_consume_admission() {
    let mut rig = Rig::new().await;
    let good = rig.command(b"valid\n");
    let mut request = good.clone();
    request.content_hash = digest(b"wrong");
    assert_eq!(
        rig.workspace.save(request).await,
        Err(Error::DigestMismatch)
    );
    let mut request = good.clone();
    request.epoch = Uuid::new_v4().to_string().parse().unwrap();
    assert_eq!(
        rig.workspace.save(request).await,
        Err(Error::WorkspaceMismatch)
    );
    let mut request = good.clone();
    request.operation_seq = 2;
    assert_eq!(rig.workspace.save(request).await, Err(Error::OperationGap));
    let mut request = good.clone();
    request.operation_seq = 0;
    assert_eq!(
        rig.workspace.save(request).await,
        Err(Error::InvalidRequest)
    );
    assert!(matches!(
        rig.workspace.save(good).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
}

#[tokio::test]
async fn old_receipt_survives_later_save_and_object_collection() {
    let mut rig = Rig::new().await;
    let first = rig.command(b"first\n");
    let receipt = rig.workspace.save(first.clone()).await.unwrap();
    let Outcome::Saved(saved) = &receipt.outcome else {
        panic!("expected saved")
    };
    let mut second = rig.command(b"second\n");
    second.operation_seq = 2;
    second.expected_content_rev = saved.content_rev.clone();
    rig.workspace.save(second).await.unwrap();
    fs::remove_file(
        rig.dir
            .path()
            .join("state/objects/sha256")
            .join(&first.content_hash),
    )
    .unwrap();
    assert_eq!(rig.workspace.save(first).await.unwrap(), receipt);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"second\n"
    );
}

#[tokio::test]
async fn returning_to_previous_bytes_issues_a_new_revision() {
    let mut rig = Rig::new().await;
    let original = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let first = rig.workspace.save(rig.command(b"changed\n")).await.unwrap();
    let Outcome::Saved(first) = first.outcome else {
        panic!("expected saved")
    };
    let mut back = rig.command(&original);
    back.operation_seq = 2;
    back.expected_content_rev = first.content_rev.clone();
    let back = rig.workspace.save(back).await.unwrap();
    let Outcome::Saved(back) = back.outcome else {
        panic!("expected saved")
    };
    assert_ne!(back.content_rev, first.content_rev);
    assert_ne!(back.content_rev, rig.initial.content_rev);
    assert_eq!(back.meta_rev, rig.initial.meta_rev);
}

#[tokio::test]
async fn external_edit_is_preserved_without_admission() {
    let mut rig = Rig::new().await;
    fs::write(rig.dir.path().join("notes/Document.md"), b"external\n").unwrap();
    assert_eq!(
        rig.workspace.save(rig.command(b"api\n")).await,
        Err(Error::ExternalChange)
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"external\n"
    );
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(receipts, 0);
}

#[tokio::test]
async fn database_failure_after_replace_never_acknowledges_and_recovers() {
    let mut rig = Rig::new().await;
    sqlx::raw_sql("CREATE TRIGGER fail_final BEFORE INSERT ON node_changes BEGIN SELECT RAISE(FAIL,'injected failure'); END;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    let request = rig.command(b"durable replacement\n");
    assert_eq!(
        rig.workspace.save(request.clone()).await,
        Err(Error::Storage)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.save(request.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    let mut next = request.clone();
    next.operation_seq = 2;
    next.operation_id = op_id();
    assert_eq!(rig.workspace.save(next).await, Err(Error::RecoveryRequired));
    let (metadata, bytes) = rig.workspace.read(rig.initial.node_id).await.unwrap();
    assert_eq!(metadata, rig.initial);
    assert_eq!(digest(&bytes), rig.initial.content_hash);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"durable replacement\n"
    );
    sqlx::raw_sql("DROP TRIGGER fail_final")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert!(reopened.ready());
    assert!(matches!(
        reopened.save(request).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
}

#[tokio::test]
async fn error_after_admission_preserves_pending_and_recovers_before_as_not_applied() {
    let mut rig = Rig::new().await;
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    let command = rig.command(b"never replaced\n");
    assert_eq!(
        rig.workspace.save(command.clone()).await,
        Err(Error::Storage)
    );
    assert!(!rig.workspace.ready());
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert!(reopened.ready());
    assert_eq!(
        reopened.save(command).await.unwrap().outcome,
        Outcome::NotApplied
    );
    assert_eq!(
        reopened.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
}

#[tokio::test]
async fn third_content_is_quarantined_and_never_automatically_replaced() {
    let mut rig = Rig::new().await;
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    let command = rig.command(b"proposed\n");
    assert_eq!(
        rig.workspace.save(command.clone()).await,
        Err(Error::Storage)
    );
    rig.workspace.close().await.unwrap();
    fs::write(rig.dir.path().join("notes/Document.md"), b"third content\n").unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert!(!reopened.ready());
    assert_eq!(
        reopened.save(command.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    let preserved: String = sqlx::query_scalar("SELECT observed_hash FROM write_intents")
        .fetch_one(&mut reopened.state.connection)
        .await
        .unwrap();
    assert_eq!(
        reopened.files.read_object(&preserved).unwrap(),
        b"third content\n"
    );
    reopened.close().await.unwrap();
    // Matching proposed bytes later must not silently settle an already quarantined intent.
    fs::write(rig.dir.path().join("notes/Document.md"), b"proposed\n").unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    assert!(!reopened.ready());
    assert_eq!(
        reopened.save(command).await.unwrap().outcome,
        Outcome::Pending
    );
    assert_eq!(
        reopened.files.read_object(&preserved).unwrap(),
        b"third content\n"
    );
}

#[tokio::test]
async fn recovery_file_sync_failure_keeps_pending_until_a_successful_retry() {
    let mut rig = Rig::new().await;
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    let command = rig.command(b"pending\n");
    assert_eq!(
        rig.workspace.save(command.clone()).await,
        Err(Error::Storage)
    );
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::load(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    reopened.hook = Some(Box::new(|phase| {
        if phase == Phase::Storage(notryx_server_storage::StorageStage::BeforeRecoveryFileSync) {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(reopened.recover().await, Err(Error::Storage));
    assert!(!reopened.ready());
    assert_eq!(
        reopened.save(command.clone()).await.unwrap().outcome,
        Outcome::Pending
    );
    reopened.hook = None;
    reopened.recover().await.unwrap();
    assert_eq!(
        reopened.save(command).await.unwrap().outcome,
        Outcome::NotApplied
    );
}

#[tokio::test]
async fn inconsistent_head_or_missing_root_fails_before_any_file_write() {
    for corrupt in [
        "UPDATE workspaces SET head_seq='0'",
        "UPDATE workspaces SET root_id='019c2ca2-8000-7000-8000-000000000001'",
    ] {
        let mut rig = Rig::new().await;
        let original = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
        sqlx::raw_sql(corrupt)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        rig.workspace.close().await.unwrap();
        let opened =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await;
        assert!(
            matches!(opened, Err(Error::InvalidFormat)),
            "corrupt logical state became ready"
        );
        assert_eq!(
            fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
            original
        );
    }
}

mod crash;

#[tokio::test]
async fn admission_transaction_failure_rolls_back_sequence_receipt_and_intent() {
    for terminal in [false, true] {
        let mut rig = Rig::new().await;
        let mut request = rig.command(b"admission must be atomic\n");
        if terminal {
            request.expected_content_rev = ContentRevision::new("stale-base").unwrap();
        }
        let trigger = if terminal {
            "CREATE TRIGGER fail_admission BEFORE INSERT ON operation_receipts BEGIN SELECT RAISE(FAIL,'injected'); END;"
        } else {
            "CREATE TRIGGER fail_admission BEFORE INSERT ON write_intents BEGIN SELECT RAISE(FAIL,'injected'); END;"
        };
        sqlx::raw_sql(trigger)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(
            rig.workspace.save(request.clone()).await,
            Err(Error::Storage)
        );
        assert!(!rig.workspace.ready());
        let last: String =
            sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
                .bind(rig.stream.to_string())
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        assert_eq!(last, "0");
        for query in [
            "SELECT COUNT(*) FROM operation_receipts",
            "SELECT COUNT(*) FROM write_intents",
        ] {
            let count: i64 = sqlx::query_scalar(query)
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
        sqlx::raw_sql("DROP TRIGGER fail_admission")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        rig.workspace.close().await.unwrap();
        let mut reopened =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(reopened.ready());
        assert_eq!(
            matches!(
                reopened.save(request).await.unwrap().outcome,
                Outcome::PreconditionFailed { .. }
            ),
            terminal
        );
    }
}

#[tokio::test]
async fn object_sync_failure_is_before_admission_and_can_retry_same_sequence() {
    let mut rig = Rig::new().await;
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::Storage(notryx_server_storage::StorageStage::BeforeObjectFileSync) {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    let command = rig.command(b"object failure\n");
    assert_eq!(
        rig.workspace.save(command.clone()).await,
        Err(Error::Storage)
    );
    assert!(rig.workspace.ready());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(count, 0);
    rig.workspace.hook = None;
    assert!(matches!(
        rig.workspace.save(command).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
}

#[tokio::test]
async fn missing_or_empty_database_and_wrong_marker_never_initialize_on_open() {
    for fault in ["missing", "empty", "marker"] {
        let rig = Rig::new().await;
        let original = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
        rig.workspace.close().await.unwrap();
        match fault {
            "missing" => fs::remove_file(rig.dir.path().join("state/state.db")).unwrap(),
            "empty" => fs::write(rig.dir.path().join("state/state.db"), []).unwrap(),
            _ => {
                let path = rig.dir.path().join("notes/.mdw-workspace.json");
                let mut marker: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                marker["workspace_id"] = Uuid::new_v4().to_string().into();
                fs::write(path, serde_json::to_vec(&marker).unwrap()).unwrap();
            }
        }
        assert!(
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
            original
        );
        if fault == "missing" {
            assert!(!rig.dir.path().join("state/state.db").exists());
        }
        if fault == "empty" {
            assert_eq!(
                fs::metadata(rig.dir.path().join("state/state.db"))
                    .unwrap()
                    .len(),
                0
            );
        }
    }
}

#[tokio::test]
async fn corrupt_pending_object_fails_closed_and_preserves_both_files() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new().await;
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    let command = rig.command(b"protected proposed\n");
    assert_eq!(
        rig.workspace.save(command.clone()).await,
        Err(Error::Storage)
    );
    rig.workspace.close().await.unwrap();
    let object = rig
        .dir
        .path()
        .join("state/objects/sha256")
        .join(&command.content_hash);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&object, b"corrupt").unwrap();
    assert!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .is_err()
    );
    assert_eq!(fs::read(&object).unwrap(), b"corrupt");
    assert_eq!(
        digest(&fs::read(rig.dir.path().join("notes/Document.md")).unwrap()),
        rig.initial.content_hash
    );
}

#[tokio::test]
async fn writer_uses_full_wal_and_foreign_keys() {
    let mut rig = Rig::new().await;
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(sync, 2);
    assert_eq!(foreign_keys, 1);
}

#[tokio::test]
async fn recovery_directory_sync_and_database_failure_keep_pending() {
    for database_failure in [false, true] {
        let mut rig = Rig::new().await;
        rig.workspace.hook = Some(Box::new(|phase| {
            if phase == Phase::BeforeFinish {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        let request = rig.command(b"already replaced\n");
        assert_eq!(
            rig.workspace.save(request.clone()).await,
            Err(Error::Storage)
        );
        rig.workspace.close().await.unwrap();
        let mut reopened =
            Workspace::load(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        if database_failure {
            sqlx::raw_sql("CREATE TRIGGER recovery_failure BEFORE INSERT ON node_changes BEGIN SELECT RAISE(FAIL,'injected'); END;")
                .execute(&mut reopened.state.connection).await.unwrap();
        } else {
            reopened.hook = Some(Box::new(|phase| {
                if phase
                    == Phase::Storage(
                        notryx_server_storage::StorageStage::BeforeRecoveryDirectorySync,
                    )
                {
                    Err(Error::Storage)
                } else {
                    Ok(())
                }
            }));
        }
        assert_eq!(reopened.recover().await, Err(Error::Storage));
        assert!(!reopened.ready());
        assert_eq!(
            reopened.save(request.clone()).await.unwrap().outcome,
            Outcome::Pending
        );
        assert_eq!(
            reopened.read(rig.initial.node_id).await.unwrap().0,
            rig.initial
        );
        if database_failure {
            sqlx::raw_sql("DROP TRIGGER recovery_failure")
                .execute(&mut reopened.state.connection)
                .await
                .unwrap();
        }
        reopened.hook = None;
        reopened.recover().await.unwrap();
        assert!(reopened.ready());
        assert!(matches!(
            reopened.save(request).await.unwrap().outcome,
            Outcome::Saved(_)
        ));
    }
}
