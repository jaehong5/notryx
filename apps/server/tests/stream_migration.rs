#[path = "support/legacy.rs"]
mod legacy;
use notryx_protocol::{CONTRACT, PairRequest};
use notryx_server::{Error, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::PathBuf;
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
}

impl Fixture {
    async fn new() -> (Self, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&state).unwrap();
        let workspace = Workspace::initialize(&notes, &state).await.unwrap();
        (
            Self {
                _directory: directory,
                notes,
                state,
            },
            workspace,
        )
    }

    async fn db(&self) -> SqliteConnection {
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(self.state.join("state.db"))
                .foreign_keys(true),
        )
        .await
        .unwrap()
    }

    async fn downgrade(&self, version: i64) {
        let mut db = self.db().await;
        legacy::nodes_before_deletion(&mut db).await;
        sqlx::query("ALTER TABLE operation_receipts DROP COLUMN node_etag")
            .execute(&mut db)
            .await
            .unwrap();
        let column: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('operation_streams') WHERE name='retired_through'").fetch_one(&mut db).await.unwrap();
        if column == 1 {
            sqlx::raw_sql("DROP TABLE retired_operations; ALTER TABLE operation_streams DROP COLUMN retired_through;").execute(&mut db).await.unwrap();
        }
        if version == 1 {
            sqlx::raw_sql("DROP TABLE devices; DROP TABLE pairing_secrets; DROP TABLE principals; DROP INDEX auth_workspace_id;").execute(&mut db).await.unwrap();
        }
        sqlx::raw_sql(match version {
            1 => "PRAGMA user_version=1",
            2 => "PRAGMA user_version=2",
            _ => panic!("unsupported legacy fixture"),
        })
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn fresh_schema_records_zero_watermark_for_every_new_stream() {
    let (fixture, mut workspace) = Fixture::new().await;
    let stream = workspace.register_stream().await.unwrap();
    let mut db = fixture.db().await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 9);
    let retired: String =
        sqlx::query_scalar("SELECT retired_through FROM operation_streams WHERE stream_id=?")
            .bind(stream.to_string())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(retired, "0");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retired_operations")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.close().await.unwrap();
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn v2_migration_is_explicit_and_preserves_auth_source_and_old_receipts() {
    let (fixture, mut workspace) = Fixture::new().await;
    let now = 1_800_000_000;
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let grant = workspace.issue_pairing(owner, now).await.unwrap();
    let device = workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            now,
        )
        .await
        .unwrap();
    let identity = workspace
        .authenticate(&device.device_token, now)
        .await
        .unwrap();
    std::fs::write(fixture.notes.join("Document.md"), b"original\r\n").unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    let bytes = b"\xef\xbb\xbfpreserved bytes\r\n";
    let request = SaveContent {
        epoch: workspace.epoch(),
        stream_id: identity.stream_id,
        operation_seq: 1,
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        node_id: document.node_id,
        expected_content_rev: document.content_rev,
        content_hash: Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        bytes: bytes.to_vec(),
    };
    let receipt = workspace.save(request.clone()).await.unwrap();
    workspace.close().await.unwrap();
    fixture.downgrade(2).await;
    assert!(matches!(
        Workspace::open(&fixture.notes, &fixture.state).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .authenticate(&device.device_token, now + 1)
            .await
            .unwrap(),
        identity
    );
    assert_eq!(
        reopened.auth_status().await.unwrap().owner_principal_id,
        owner
    );
    assert_eq!(reopened.epoch(), request.epoch);
    assert_eq!(reopened.save(request).await.unwrap(), receipt);
    assert_eq!(
        std::fs::read(fixture.notes.join("Document.md")).unwrap(),
        bytes
    );
    reopened.close().await.unwrap();
    Workspace::migrate(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let mut db = fixture.db().await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 9);
    let retired: String =
        sqlx::query_scalar("SELECT retired_through FROM operation_streams WHERE stream_id=?")
            .bind(identity.stream_id.to_string())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(retired, "0");
    db.close().await.unwrap();
}

#[tokio::test]
async fn failed_v1_to_v3_migration_rolls_back_auth_and_preserves_colliding_table() {
    let (fixture, workspace) = Fixture::new().await;
    workspace.close().await.unwrap();
    fixture.downgrade(1).await;
    let mut db = fixture.db().await;
    sqlx::raw_sql("CREATE TABLE retired_operations(sentinel TEXT); INSERT INTO retired_operations VALUES('synthetic evidence');").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(
        Workspace::migrate(&fixture.notes, &fixture.state)
            .await
            .is_err()
    );
    let mut db = fixture.db().await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 1);
    let auth_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('principals','devices','pairing_secrets','auth_workspace_id','auth_one_owner')").fetch_one(&mut db).await.unwrap();
    assert_eq!(auth_count, 0);
    let column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('operation_streams') WHERE name='retired_through'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(column, 0);
    let evidence: String = sqlx::query_scalar("SELECT sentinel FROM retired_operations")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(evidence, "synthetic evidence");
    db.close().await.unwrap();
}
