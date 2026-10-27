use super::*;
use notryx_protocol::{CONTRACT, NodeKind, PairRequest};

async fn device(workspace: &mut Workspace, now: i64) -> notryx_protocol::PairResponse {
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let grant = workspace.issue_pairing(owner, now).await.unwrap();
    workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            now,
        )
        .await
        .unwrap()
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
async fn uploaded(workspace: &mut Workspace, token: &str, bytes: &[u8], time: i64) -> Uploaded {
    let mut grant = workspace
        .begin_upload(token, digest(bytes), Some(bytes.len() as u64), time)
        .await
        .unwrap();
    grant.writer.write_chunk(bytes).unwrap();
    let sealed = grant.writer.seal().unwrap();
    workspace
        .finish_upload(token, grant.upload_id, sealed, time)
        .await
        .unwrap()
}
fn create(rig: &Rig, device: &notryx_protocol::PairResponse, upload_id: Uuid) -> CreateNode {
    CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: device.device_stream_id.parse().unwrap(),
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.workspace.state.root_id,
        name: "Attachment.bin".into(),
        kind: NodeKind::File,
        upload_id: Some(upload_id),
    }
}

#[tokio::test]
async fn uploads_are_durable_without_advancing_stream_and_create_consumes_once() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let body = b"\0raw\xff\r\n";
    let upload = uploaded(&mut rig.workspace, &device.device_token, body, time).await;
    assert_eq!(upload.content_hash, digest(body));
    assert_eq!(upload.size, body.len() as u64);
    assert!((time + 86400..=now() + 86400).contains(&upload.expires_at));
    assert_eq!(rig.workspace.state.head_seq().await.unwrap().get(), 1);
    let seq: String =
        sqlx::query_scalar("SELECT last_seq FROM operation_streams WHERE stream_id=?")
            .bind(&device.device_stream_id)
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    assert_eq!(seq, "0");
    let request = create(&rig, &device, upload.upload_id);
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    let receipt = rig
        .workspace
        .create_node_authorized(request.clone(), Some(&device.device_token))
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, Outcome::Created(_)));
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Attachment.bin")).unwrap(),
        body
    );
    assert_eq!(
        rig.workspace
            .create_node_authorized(request.clone(), Some(&device.device_token))
            .await
            .unwrap(),
        receipt
    );
    let mut second = request;
    second.operation_id = op_id();
    second.operation_seq = 2;
    second.node_id = node_id();
    second.name = "Other.bin".into();
    assert_eq!(
        rig.workspace
            .create_node_authorized(second, Some(&device.device_token))
            .await,
        Err(Error::UploadUnavailable)
    );
}

#[tokio::test]
async fn upload_digest_and_declared_length_failure_release_reservation() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    for (expected, size, bytes) in [
        (digest(b"right"), Some(5), b"wrong".as_slice()),
        (digest(b"a"), Some(2), b"a".as_slice()),
    ] {
        let mut grant = rig
            .workspace
            .begin_upload(&device.device_token, expected, size, time)
            .await
            .unwrap();
        grant.writer.write_chunk(bytes).unwrap();
        let sealed = grant.writer.seal().unwrap();
        assert!(matches!(
            rig.workspace
                .finish_upload(&device.device_token, grant.upload_id, sealed, time)
                .await,
            Err(Error::DigestMismatch | Error::InvalidRequest)
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM uploads")
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn byte_and_empty_upload_count_quota_return_after_cancel_and_expiry() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    rig.workspace
        .set_upload_limits(UploadLimits {
            max_device_bytes: 4,
            max_workspace_bytes: 8,
            max_device_uploads: 1,
            max_workspace_uploads: 2,
            ..Default::default()
        })
        .unwrap();
    let grant = rig
        .workspace
        .begin_upload(&device.device_token, digest(b"1234"), Some(4), time)
        .await
        .unwrap();
    assert!(matches!(
        rig.workspace
            .begin_upload(&device.device_token, digest(b""), Some(0), time)
            .await,
        Err(Error::UploadQuota)
    ));
    drop(grant);
    let upload = uploaded(&mut rig.workspace, &device.device_token, b"", time).await;
    assert!(matches!(
        rig.workspace
            .begin_upload(&device.device_token, digest(b""), Some(0), time)
            .await,
        Err(Error::UploadQuota)
    ));
    let next = rig
        .workspace
        .begin_upload(
            &device.device_token,
            digest(b""),
            Some(0),
            upload.expires_at,
        )
        .await
        .unwrap();
    drop(next);
    assert!(matches!(
        rig.workspace
            .begin_upload(&device.device_token, digest(b"12345"), Some(5), time)
            .await,
        Err(Error::UploadQuota)
    ));
}

#[tokio::test]
async fn upload_finish_rechecks_device_owner_revocation_and_receive_deadline() {
    let mut rig = Rig::new().await;
    let time = now();
    let owner = device(&mut rig.workspace, time).await;
    let other = device(&mut rig.workspace, time).await;
    let grant = rig
        .workspace
        .begin_upload(&owner.device_token, digest(b""), Some(0), time)
        .await
        .unwrap();
    let sealed = grant.writer.seal().unwrap();
    assert_eq!(
        rig.workspace
            .finish_upload(&other.device_token, grant.upload_id, sealed, time)
            .await,
        Err(Error::Auth(auth::AuthError::Forbidden))
    );
    let grant = rig
        .workspace
        .begin_upload(&owner.device_token, digest(b""), Some(0), time)
        .await
        .unwrap();
    let sealed = grant.writer.seal().unwrap();
    assert_eq!(
        rig.workspace
            .finish_upload(&owner.device_token, grant.upload_id, sealed, time + 120)
            .await,
        Err(Error::UploadUnavailable)
    );
    let grant = rig
        .workspace
        .begin_upload(&owner.device_token, digest(b""), Some(0), time)
        .await
        .unwrap();
    let sealed = grant.writer.seal().unwrap();
    rig.workspace
        .revoke_device(owner.device_id.parse().unwrap(), time)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace
            .finish_upload(&owner.device_token, grant.upload_id, sealed, time)
            .await,
        Err(Error::Auth(auth::AuthError::Unauthorized))
    );
}

#[tokio::test]
async fn upload_attach_requires_device_ownership_and_receipt_precedes_upload_expiry() {
    let mut rig = Rig::new().await;
    let time = now();
    let owner = device(&mut rig.workspace, time).await;
    let other = device(&mut rig.workspace, time).await;
    let upload = uploaded(&mut rig.workspace, &owner.device_token, b"data", time).await;
    let foreign = create(&rig, &other, upload.upload_id);
    assert_eq!(
        rig.workspace
            .create_node_authorized(foreign, Some(&other.device_token))
            .await,
        Err(Error::Auth(auth::AuthError::Forbidden))
    );
    let request = create(&rig, &owner, upload.upload_id);
    let receipt = rig
        .workspace
        .create_node_authorized(request.clone(), Some(&owner.device_token))
        .await
        .unwrap();
    sqlx::query("DELETE FROM uploads")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace
            .create_node_authorized(request, Some(&owner.device_token))
            .await
            .unwrap(),
        receipt
    );
}

#[tokio::test]
async fn upload_restart_reclaims_receiving_and_untracked_staging() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let grant = rig
        .workspace
        .begin_upload(&device.device_token, digest(b""), Some(0), time)
        .await
        .unwrap();
    drop(grant);
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM uploads")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        fs::read_dir(rig.dir.path().join("state/staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn sqlite_full_is_preserved_as_no_space() {
    let mut rig = Rig::new().await;
    sqlx::query("CREATE TABLE fill_disk(bytes BLOB)")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA max_page_count=1")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let error = sqlx::query("INSERT INTO fill_disk VALUES(zeroblob(1048576))")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().unwrap().as_ref(),
        "13"
    );
    assert_eq!(state::db_error(error), Error::NoSpace);
}

#[tokio::test]
async fn pending_upload_survives_ttl_and_not_applied_releases_without_consuming() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let upload = uploaded(&mut rig.workspace, &device.device_token, b"pending", time).await;
    let request = create(&rig, &device, upload.upload_id);
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace
            .create_node_authorized(request.clone(), Some(&device.device_token))
            .await,
        Err(Error::Storage)
    );
    rig.workspace
        .cleanup_uploads(upload.expires_at + 1)
        .await
        .unwrap();
    assert!(
        rig.dir
            .path()
            .join(format!("state/staging/upload-{}.ready", upload.upload_id))
            .exists()
    );
    assert!(
        rig.workspace
            .state
            .upload(upload.upload_id)
            .await
            .unwrap()
            .unwrap()
            .pending_operation_id
            .is_some()
    );
    rig.workspace.hook = None;
    rig.workspace.recover().await.unwrap();
    assert_eq!(
        rig.workspace
            .create_node_authorized(request.clone(), Some(&device.device_token))
            .await
            .unwrap()
            .outcome,
        Outcome::NotApplied
    );
    let record = rig
        .workspace
        .state
        .upload(upload.upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, "ready");
    assert_eq!(record.pending_operation_id, None);
    let mut second = request;
    second.operation_id = op_id();
    second.operation_seq = 2;
    second.node_id = node_id();
    assert!(matches!(
        rig.workspace
            .create_node_authorized(second, Some(&device.device_token))
            .await
            .unwrap()
            .outcome,
        Outcome::Created(_)
    ));
}

#[tokio::test]
async fn create_consumption_rolls_back_with_receipt_and_recovery_consumes_on_success() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let upload = uploaded(&mut rig.workspace, &device.device_token, b"atomic", time).await;
    let request = create(&rig, &device, upload.upload_id);
    sqlx::raw_sql("CREATE TRIGGER fail_upload_create BEFORE UPDATE ON operation_receipts WHEN NEW.pending=0 BEGIN SELECT RAISE(ABORT,'injected'); END;").execute(&mut rig.workspace.state.connection).await.unwrap();
    assert_eq!(
        rig.workspace
            .create_node_authorized(request.clone(), Some(&device.device_token))
            .await,
        Err(Error::Storage)
    );
    let record = rig
        .workspace
        .state
        .upload(upload.upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, "ready");
    assert_eq!(record.pending_operation_id, Some(request.operation_id));
    assert_eq!(
        rig.workspace.state.node(request.node_id).await.unwrap(),
        None
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Attachment.bin")).unwrap(),
        b"atomic"
    );
    sqlx::query("DROP TRIGGER fail_upload_create")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert!(
        rig.workspace
            .state
            .upload(upload.upload_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        rig.workspace
            .create_node_authorized(request, Some(&device.device_token))
            .await
            .unwrap()
            .outcome,
        Outcome::Created(_)
    ));
}

#[tokio::test]
async fn recovery_conflict_preserves_unconsumed_upload_for_a_new_operation() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let upload = uploaded(&mut rig.workspace, &device.device_token, b"proposed", time).await;
    let request = create(&rig, &device, upload.upload_id);
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace
            .create_node_authorized(request.clone(), Some(&device.device_token))
            .await,
        Err(Error::Storage)
    );
    fs::write(rig.dir.path().join("notes/Attachment.bin"), b"external").unwrap();
    rig.workspace.hook = None;
    rig.workspace.recover().await.unwrap();
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace
            .resolve_recovery(request.operation_id)
            .await
            .unwrap()
            .outcome,
        Outcome::RecoveryConflict
    );
    let record = rig
        .workspace
        .state
        .upload(upload.upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, "ready");
    assert_eq!(record.pending_operation_id, None);
    let mut second = request;
    second.operation_id = op_id();
    second.operation_seq = 2;
    second.node_id = node_id();
    second.name = "Recovered.bin".into();
    assert!(matches!(
        rig.workspace
            .create_node_authorized(second, Some(&device.device_token))
            .await
            .unwrap()
            .outcome,
        Outcome::Created(_)
    ));
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Recovered.bin")).unwrap(),
        b"proposed"
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Attachment.bin")).unwrap(),
        b"external"
    );
}

#[tokio::test]
async fn completion_db_failure_reclaims_staging_and_byte_quota() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let mut grant = rig
        .workspace
        .begin_upload(&device.device_token, digest(b"data"), Some(4), time)
        .await
        .unwrap();
    grant.writer.write_chunk(b"data").unwrap();
    let sealed = grant.writer.seal().unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_upload_finish BEFORE UPDATE ON uploads WHEN NEW.status='ready' BEGIN SELECT RAISE(ABORT,'injected'); END;").execute(&mut rig.workspace.state.connection).await.unwrap();
    assert_eq!(
        rig.workspace
            .finish_upload(&device.device_token, grant.upload_id, sealed, time)
            .await,
        Err(Error::Storage)
    );
    assert!(
        rig.workspace
            .state
            .upload(grant.upload_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read_dir(rig.dir.path().join("state/staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn ready_staging_corruption_is_diagnosed_without_deleting_evidence() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let upload = uploaded(&mut rig.workspace, &device.device_token, b"good", time).await;
    rig.workspace.close().await.unwrap();
    let file = rig
        .dir
        .path()
        .join(format!("state/staging/upload-{}.ready", upload.upload_id));
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&file, b"evil").unwrap();
    assert!(matches!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await,
        Err(Error::InvalidFormat)
    ));
    assert_eq!(fs::read(file).unwrap(), b"evil");
}

#[tokio::test]
async fn explicit_schema_eight_migration_preserves_pending_create_and_old_fingerprint() {
    let mut rig = Rig::new().await;
    let request = CreateNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: node_id(),
        parent_id: rig.workspace.state.root_id,
        name: "Empty.md".into(),
        kind: NodeKind::File,
        upload_id: None,
    };
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.create_node(request.clone()).await,
        Err(Error::Storage)
    );
    let intent: String = sqlx::query_scalar("SELECT intent FROM write_intents")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert!(!intent.contains("upload_id"));
    sqlx::raw_sql("DROP TABLE uploads; PRAGMA user_version=8;")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    rig.workspace.close().await.unwrap();
    assert!(matches!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.create_node(request).await.unwrap().outcome,
        Outcome::NotApplied
    );
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(version, 9);
}

#[tokio::test]
async fn schema_nine_migration_failure_keeps_schema_eight_and_pending_intent() {
    let mut rig = Rig::new().await;
    let request = rig.command(b"pending migration");
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.save(request.clone()).await,
        Err(Error::Storage)
    );
    sqlx::raw_sql("DROP TABLE uploads; CREATE TABLE uploads(evidence TEXT); INSERT INTO uploads VALUES('preserve'); PRAGMA user_version=8;").execute(&mut rig.workspace.state.connection).await.unwrap();
    rig.workspace.close().await.unwrap();
    assert_eq!(
        Workspace::migrate(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await,
        Err(Error::Storage)
    );
    let mut database = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(rig.dir.path().join("state/state.db")),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut database)
        .await
        .unwrap();
    assert_eq!(version, 8);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT evidence FROM uploads")
            .fetch_one(&mut database)
            .await
            .unwrap(),
        "preserve"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT operation_id FROM write_intents")
            .fetch_one(&mut database)
            .await
            .unwrap(),
        request.operation_id.to_string()
    );
}

#[tokio::test]
async fn sigkill_upload_publication_and_creation_preserve_ready_or_pending_ownership() {
    for phase in [Phase::AfterUploadPublish, Phase::AfterUploadFinish] {
        let mut rig = Rig::new().await;
        let device = device(&mut rig.workspace, now()).await;
        fs::write(
            rig.dir.path().join("upload-request.json"),
            serde_json::to_vec(&(device.device_token, b"durable upload".to_vec())).unwrap(),
        )
        .unwrap();
        rig.workspace.close().await.unwrap();
        super::crash::kill_at(rig.dir.path(), phase, false);
        rig.workspace =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        let records = rig.workspace.state.uploads().await.unwrap();
        assert_eq!(
            records.len(),
            usize::from(phase == Phase::AfterUploadFinish)
        );
        if let Some(record) = records.first() {
            assert_eq!(record.status, "ready");
            assert_eq!(
                record.content_hash.as_deref(),
                Some(digest(b"durable upload").as_str())
            );
            rig.workspace
                .files
                .validate_upload(&record.stored().unwrap())
                .unwrap();
        }
        assert_eq!(
            fs::read_dir(rig.dir.path().join("state/staging"))
                .unwrap()
                .count(),
            records.len()
        );
    }
    for phase in [Phase::AfterAdmission, Phase::AfterFinish] {
        let mut rig = Rig::new().await;
        let time = now();
        let device = device(&mut rig.workspace, time).await;
        let upload = uploaded(
            &mut rig.workspace,
            &device.device_token,
            b"created after kill",
            time,
        )
        .await;
        let request = create(&rig, &device, upload.upload_id);
        fs::write(
            rig.dir.path().join("create-request.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        fs::write(
            rig.dir.path().join("upload-token.txt"),
            &device.device_token,
        )
        .unwrap();
        rig.workspace.close().await.unwrap();
        super::crash::kill_at(rig.dir.path(), phase, false);
        rig.workspace =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        let receipt = rig
            .workspace
            .create_node_authorized(request.clone(), Some(&device.device_token))
            .await
            .unwrap();
        if phase == Phase::AfterAdmission {
            assert_eq!(receipt.outcome, Outcome::NotApplied);
            assert_eq!(
                rig.workspace
                    .state
                    .upload(upload.upload_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                "ready"
            );
        } else {
            assert!(matches!(receipt.outcome, Outcome::Created(_)));
            assert!(
                rig.workspace
                    .state
                    .upload(upload.upload_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                fs::read(rig.dir.path().join("notes/Attachment.bin")).unwrap(),
                b"created after kill"
            );
        }
        assert_eq!(
            rig.workspace
                .create_node_authorized(request, Some(&device.device_token))
                .await
                .unwrap(),
            receipt
        );
    }
}

#[tokio::test]
async fn sqlite_full_reservation_and_auth_write_keep_typed_no_space() {
    use notryx_protocol::WorkspaceRole;
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    sqlx::raw_sql("CREATE TABLE fill_disk(bytes BLOB);
        CREATE TRIGGER full_upload AFTER INSERT ON uploads BEGIN INSERT INTO fill_disk VALUES(zeroblob(1048576)); END;
        CREATE TRIGGER full_principal AFTER INSERT ON principals BEGIN INSERT INTO fill_disk VALUES(zeroblob(1048576)); END;")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    sqlx::query("PRAGMA max_page_count=1")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert!(matches!(
        rig.workspace
            .begin_upload(&device.device_token, digest(b"data"), Some(4), time)
            .await,
        Err(Error::NoSpace)
    ));
    assert_eq!(
        rig.workspace.create_principal(WorkspaceRole::Editor).await,
        Err(auth::AuthError::NoSpace)
    );
    assert!(rig.workspace.state.uploads().await.unwrap().is_empty());
    assert_eq!(
        fs::read_dir(rig.dir.path().join("state/staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn attachment_25_mib_creation_update_and_recovery_preserve_exact_bytes() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let bytes = vec![0xff; notryx_server_storage::MAX_FILE_BYTES];
    let upload = uploaded(&mut rig.workspace, &device.device_token, &bytes, time).await;
    let request = create(&rig, &device, upload.upload_id);
    let receipt = rig
        .workspace
        .create_node_authorized(request.clone(), Some(&device.device_token))
        .await
        .unwrap();
    let Outcome::Created(node) = receipt.outcome else {
        panic!("uploaded attachment created")
    };
    assert_eq!(rig.workspace.read(node.node_id).await.unwrap().1, bytes);
    assert!(matches!(
        rig.workspace
            .begin_upload(
                &device.device_token,
                digest(b""),
                Some(bytes.len() as u64 + 1),
                now()
            )
            .await,
        Err(Error::UploadTooLarge)
    ));
    let changed = vec![0x80; notryx_server_storage::MAX_FILE_BYTES];
    let save = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: request.stream_id,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: node.node_id,
        expected_content_rev: node.content_rev.unwrap(),
        content_hash: digest(&changed),
        bytes: changed.clone(),
    };
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::BeforeFinish {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(rig.workspace.save(save.clone()).await, Err(Error::Storage));
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert!(matches!(
        rig.workspace.save(save).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
    assert_eq!(rig.workspace.read(node.node_id).await.unwrap().1, changed);
}

#[tokio::test]
async fn token_expiry_during_upload_publication_is_checked_before_database_completion() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let grant = rig
        .workspace
        .begin_upload(&device.device_token, digest(b""), Some(0), time)
        .await
        .unwrap();
    let sealed = grant.writer.seal().unwrap();
    let expires = now() + 2;
    sqlx::query("UPDATE devices SET idle_expires_at=? WHERE device_id=?")
        .bind(expires)
        .bind(&device.device_id)
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let published = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = published.clone();
    rig.workspace.hook = Some(Box::new(move |phase| {
        if phase == Phase::AfterUploadPublish {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            while now() < expires {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        Ok(())
    }));
    assert_eq!(
        rig.workspace
            .finish_upload(&device.device_token, grant.upload_id, sealed, time)
            .await,
        Err(Error::Auth(auth::AuthError::Unauthorized))
    );
    assert!(published.load(std::sync::atomic::Ordering::SeqCst));
    assert!(rig.workspace.state.uploads().await.unwrap().is_empty());
    assert_eq!(
        fs::read_dir(rig.dir.path().join("state/staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn successful_create_receipt_survives_staging_cleanup_failure() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let upload = uploaded(&mut rig.workspace, &device.device_token, b"committed", time).await;
    let request = create(&rig, &device, upload.upload_id);
    let staging = rig.dir.path().join("state/staging");
    let blocked = staging.clone();
    let offline = rig.dir.path().join("state/staging-offline");
    let offline_hook = offline.clone();
    rig.workspace.hook = Some(Box::new(move |phase| {
        if phase == Phase::AfterFinish {
            fs::rename(&blocked, &offline_hook).unwrap();
            fs::write(&blocked, b"unavailable directory").unwrap();
        }
        Ok(())
    }));
    let result = rig
        .workspace
        .create_node_authorized(request.clone(), Some(&device.device_token))
        .await;
    fs::remove_file(&staging).unwrap();
    fs::rename(&offline, &staging).unwrap();
    let receipt = result.unwrap();
    assert!(matches!(receipt.outcome, Outcome::Created(_)));
    assert_eq!(fs::read_dir(&staging).unwrap().count(), 1);
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(
        rig.workspace
            .create_node_authorized(request, Some(&device.device_token))
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Attachment.bin")).unwrap(),
        b"committed"
    );
}

#[tokio::test]
async fn lost_upload_completion_response_keeps_committed_staging_and_quota() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let grant = rig
        .workspace
        .begin_upload(&device.device_token, digest(b""), Some(0), time)
        .await
        .unwrap();
    let sealed = grant.writer.seal().unwrap();
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterUploadFinish {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace
            .finish_upload(&device.device_token, grant.upload_id, sealed, time)
            .await,
        Err(Error::Storage)
    );
    assert_eq!(
        rig.workspace
            .state
            .upload(grant.upload_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "ready"
    );
    assert!(
        rig.dir
            .path()
            .join(format!("state/staging/upload-{}.ready", grant.upload_id))
            .exists()
    );
    rig.workspace.close().await.unwrap();
    rig.workspace = Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
        .await
        .unwrap();
    assert_eq!(rig.workspace.state.uploads().await.unwrap().len(), 1);
}

#[test]
fn upload_resource_configuration_has_defaults_and_rejects_inconsistent_budgets() {
    let base = "[workspace]\nnotes_root='/tmp/notryx-notes'\nstate_root='/tmp/notryx-state'\n";
    let config = crate::config::Config::from_toml(base).unwrap();
    assert_eq!(config.workspace.uploads.max_device_bytes, 128 * 1024 * 1024);
    assert_eq!(config.workspace.uploads.max_workspace_uploads, 256);
    for limits in [
        "max_device_bytes=0",
        "max_device_bytes=536870913",
        "max_workspace_bytes=68719476737",
        "max_device_uploads=0",
        "max_workspace_uploads=4097",
        "max_receiving=0",
        "max_receiving=17",
        "unknown=1",
    ] {
        assert!(
            crate::config::Config::from_toml(&format!("{base}[workspace.uploads]\n{limits}\n"))
                .is_err(),
            "{limits}"
        );
    }
    assert!(
        crate::config::Config::from_toml(&format!(
            "{base}[workspace.uploads]\nmax_device_bytes=1\nmax_workspace_bytes=1\n"
        ))
        .is_ok()
    );
}

#[tokio::test]
async fn missing_ready_staging_is_storage_failure_without_admission_or_consumption() {
    let mut rig = Rig::new().await;
    let time = now();
    let device = device(&mut rig.workspace, time).await;
    let upload = uploaded(&mut rig.workspace, &device.device_token, b"missing", time).await;
    let request = create(&rig, &device, upload.upload_id);
    fs::remove_file(
        rig.dir
            .path()
            .join(format!("state/staging/upload-{}.ready", upload.upload_id)),
    )
    .unwrap();
    assert_eq!(
        rig.workspace
            .create_node_authorized(request, Some(&device.device_token))
            .await,
        Err(Error::Storage)
    );
    let record = rig
        .workspace
        .state
        .upload(upload.upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, "ready");
    assert_eq!(record.pending_operation_id, None);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM operation_receipts")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap(),
        0
    );
    assert!(!rig.dir.path().join("notes/Attachment.bin").exists());
}
