use notryx_client_store::{ClientStore, RecoveryStore, StoreError};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Connection, SqliteConnection};
use std::path::Path;

async fn failed_v3_fixture(path: &Path) -> SqliteConnection {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal),
    )
    .await
    .unwrap();
    sqlx::raw_sql(
        "PRAGMA wal_autocheckpoint=0;
         CREATE TABLE draft_checkpoints (document_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, format_version INTEGER NOT NULL, bytes BLOB NOT NULL);
         CREATE TABLE draft_bases (document_id TEXT PRIMARY KEY, bytes BLOB NOT NULL, revision TEXT NOT NULL);
         CREATE TABLE client_registrations (id INTEGER PRIMARY KEY, server_origin TEXT NOT NULL, workspace_id TEXT NOT NULL, workspace_epoch TEXT NOT NULL, device_id TEXT NOT NULL, device_stream_id TEXT NOT NULL, status TEXT NOT NULL);
         CREATE TABLE registered_documents (registration_id INTEGER NOT NULL, document_id TEXT NOT NULL, storage_id TEXT NOT NULL UNIQUE, PRIMARY KEY(registration_id, document_id));
         CREATE TABLE outbox_streams (registration_id INTEGER PRIMARY KEY, next_seq INTEGER NOT NULL, ack_through INTEGER NOT NULL);
         CREATE TABLE client_outbox (operation_id TEXT PRIMARY KEY, registration_id INTEGER NOT NULL, operation_seq INTEGER NOT NULL, document_id TEXT NOT NULL, storage_id TEXT NOT NULL, generation INTEGER NOT NULL, expected_revision TEXT, dependency TEXT, bytes BLOB NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL, committed_revision TEXT, conflict_revision TEXT, conflict_etag TEXT, abandon_state TEXT NOT NULL DEFAULT 'none');
         INSERT INTO client_registrations VALUES (1, 'https://example.invalid', '11111111-1111-4111-8111-111111111111', '22222222-2222-4222-8222-222222222222', '33333333-3333-4333-8333-333333333333', '44444444-4444-4444-8444-444444444444', 'frozen');
         INSERT INTO client_registrations VALUES (2, 'https://example.invalid', '11111111-1111-4111-8111-111111111111', '22222222-2222-4222-8222-222222222222', '55555555-5555-4555-8555-555555555555', '66666666-6666-4666-8666-666666666666', 'active');
         INSERT INTO registered_documents VALUES (1, 'same-document', 'old-storage'), (2, 'same-document', 'new-storage');
         INSERT INTO draft_checkpoints VALUES ('old-storage', 7, 1, X'EFBBBF6F6C640D0A'), ('new-storage', 3, 1, X'6E65770A');
         INSERT INTO draft_bases VALUES ('old-storage', X'626173650D0A', 'base-rev'), ('new-storage', X'6F74686572', 'other-rev');
         INSERT INTO outbox_streams VALUES (1, 2, 0);
         INSERT INTO client_outbox VALUES ('op-a', 1, 1, 'same-document', 'old-storage', 6, 'base-rev', NULL, X'7061796C6F61640A', 'synthetic-hash', 'sent', NULL, NULL, NULL, 'none');
         PRAGMA application_id=1314148952;
         PRAGMA user_version=3;",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db
}

#[tokio::test]
async fn failed_legacy_migration_keeps_unregistered_raw_draft_and_base() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE draft_checkpoints (document_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, format_version INTEGER NOT NULL, bytes BLOB NOT NULL); CREATE TABLE draft_bases (document_id TEXT PRIMARY KEY, bytes BLOB NOT NULL, revision TEXT NOT NULL); CREATE TABLE client_registrations (collision INTEGER); INSERT INTO draft_checkpoints VALUES ('legacy', 2, 1, X'FF0041'); INSERT INTO draft_bases VALUES ('legacy', X'424153450D0A', 'legacy-rev'); PRAGMA application_id=1314148952; PRAGMA user_version=1;").execute(&mut fixture).await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let drafts = recovery.drafts(None).await.unwrap().items;
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].document_id, "legacy");
    assert!(drafts[0].registration.is_none());
    let draft = recovery.draft("legacy").await.unwrap();
    assert_eq!(draft.bytes, b"\xff\0A");
    assert_eq!(draft.base_bytes, b"BASE\r\n");
    assert!(recovery.operations(None).await.unwrap().items.is_empty());
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn unknown_schema_is_not_interpreted_but_can_be_exported_as_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE future_data (bytes BLOB NOT NULL); CREATE TABLE future_refs (target INTEGER NOT NULL); INSERT INTO future_data VALUES (X'0000'), (X'FF0061'); DELETE FROM future_data WHERE rowid=1; INSERT INTO future_refs VALUES (2); PRAGMA application_id=1314148952; PRAGMA user_version=99;").execute(&mut fixture).await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert!(matches!(
        recovery.drafts(None).await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert!(matches!(
        recovery.operations(None).await,
        Err(StoreError::UnsupportedFormat)
    ));
    let snapshot_path = dir.path().join("future.db");
    recovery.export_snapshot(&snapshot_path).await.unwrap();
    let mut snapshot = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(snapshot_path)
            .read_only(true),
    )
    .await
    .unwrap();
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM future_data")
        .fetch_one(&mut snapshot)
        .await
        .unwrap();
    assert_eq!(bytes, b"\xff\0a");
    let target: Vec<u8> = sqlx::query_scalar(
        "SELECT f.bytes FROM future_data f JOIN future_refs r ON f.rowid=r.target",
    )
    .fetch_one(&mut snapshot)
    .await
    .unwrap();
    assert_eq!(target, b"\xff\0a");
    snapshot.close().await.unwrap();
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn failed_migration_exposes_distinct_drafts_bases_and_outbox_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = failed_v3_fixture(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let drafts = recovery.drafts(None).await.unwrap().items;
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].document_id, "same-document");
    assert_eq!(drafts[1].document_id, "same-document");
    assert_ne!(drafts[0].registration, drafts[1].registration);
    let old = recovery.draft("old-storage").await.unwrap();
    assert_eq!(old.bytes, b"\xef\xbb\xbfold\r\n");
    assert_eq!(old.base_bytes, b"base\r\n");
    assert_eq!(old.base_revision, "base-rev");
    assert_eq!(recovery.draft("new-storage").await.unwrap().bytes, b"new\n");
    let operations = recovery.operations(None).await.unwrap().items;
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].operation_id, "op-a");
    assert_eq!(operations[0].status, "sent");
    assert_eq!(operations[0].terminal_kind, None);
    assert_eq!(operations[0].registration_status.as_deref(), Some("frozen"));
    assert_eq!(
        recovery.operation_bytes("op-a").await.unwrap(),
        b"payload\n"
    );
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut fixture)
        .await
        .unwrap();
    assert_eq!(version, 3);
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn damaged_base_and_checkpoint_can_be_recovered_independently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = failed_v3_fixture(&path).await;
    sqlx::raw_sql("DELETE FROM draft_bases WHERE document_id='old-storage'; UPDATE draft_checkpoints SET format_version=99 WHERE document_id='new-storage';").execute(&mut fixture).await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(
        recovery.checkpoint_bytes("old-storage").await.unwrap(),
        b"\xef\xbb\xbfold\r\n"
    );
    assert!(matches!(
        recovery.base_bytes("old-storage").await,
        Err(StoreError::MissingDraft)
    ));
    assert!(matches!(
        recovery.checkpoint_bytes("new-storage").await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert_eq!(recovery.base_bytes("new-storage").await.unwrap(), b"other");
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn base_only_and_rows_with_damaged_metadata_do_not_hide_other_items() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = failed_v3_fixture(&path).await;
    sqlx::raw_sql("INSERT INTO draft_bases VALUES ('base-only', X'626173650A', 'rev'); INSERT INTO registered_documents VALUES (1, 'base-doc', 'base-only'); UPDATE registered_documents SET registration_id=99 WHERE storage_id='old-storage'; UPDATE draft_checkpoints SET generation=0 WHERE document_id='new-storage'; UPDATE client_outbox SET registration_id=99, operation_seq=0 WHERE operation_id='op-a'; INSERT INTO client_outbox VALUES ('op-b', 1, 2, 'doc', 'base-only', 2, 'rev', NULL, X'7061796C6F6164', 'hash', 'sent', NULL, NULL, NULL, 'none');").execute(&mut fixture).await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let drafts = recovery.drafts(None).await.unwrap().items;
    let base = drafts
        .iter()
        .find(|draft| draft.storage_id == "base-only")
        .unwrap();
    assert!(!base.has_checkpoint && base.has_base);
    assert_eq!(recovery.base_bytes("base-only").await.unwrap(), b"base\n");
    assert!(
        drafts
            .iter()
            .find(|draft| draft.storage_id == "old-storage")
            .unwrap()
            .metadata_error
    );
    assert!(
        drafts
            .iter()
            .find(|draft| draft.storage_id == "new-storage")
            .unwrap()
            .metadata_error
    );
    let operations = recovery.operations(None).await.unwrap().items;
    assert_eq!(operations.len(), 2);
    assert!(
        operations
            .iter()
            .find(|operation| operation.operation_id == "op-a")
            .unwrap()
            .metadata_error
    );
    assert!(
        !operations
            .iter()
            .find(|operation| operation.operation_id == "op-b")
            .unwrap()
            .metadata_error
    );
    assert_eq!(
        recovery.operation_bytes("op-a").await.unwrap(),
        b"payload\n"
    );
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn damaged_metadata_does_not_prevent_checkpoint_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = failed_v3_fixture(&path).await;
    sqlx::query("UPDATE draft_checkpoints SET generation=0 WHERE document_id='new-storage'")
        .execute(&mut fixture)
        .await
        .unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(
        recovery.checkpoint_bytes("new-storage").await.unwrap(),
        b"new\n"
    );
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn damaged_metadata_does_not_prevent_base_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = failed_v3_fixture(&path).await;
    sqlx::query("UPDATE draft_bases SET revision='' WHERE document_id='old-storage'")
        .execute(&mut fixture)
        .await
        .unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(
        recovery.base_bytes("old-storage").await.unwrap(),
        b"base\r\n"
    );
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn v4_terminal_result_is_not_confused_with_the_storage_status() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = failed_v3_fixture(&path).await;
    sqlx::raw_sql("ALTER TABLE client_outbox ADD COLUMN terminal_kind TEXT; UPDATE client_outbox SET status='committed', terminal_kind='abandoned'; PRAGMA user_version=4;").execute(&mut fixture).await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let operations = recovery.operations(None).await.unwrap().items;
    assert_eq!(operations[0].status, "committed");
    assert_eq!(operations[0].terminal_kind.as_deref(), Some("abandoned"));
    assert_eq!(operations[0].registration_status.as_deref(), Some("frozen"));
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn recovery_pages_do_not_drop_any_of_more_than_ten_thousand_items() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let mut fixture = failed_v3_fixture(&path).await;
    sqlx::raw_sql("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10003) INSERT INTO draft_checkpoints SELECT printf('page-%05d',x), 1, 1, X'61' FROM n; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10003) INSERT INTO draft_bases SELECT printf('page-%05d',x), X'62', 'rev' FROM n; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10003) INSERT INTO registered_documents SELECT 1, printf('document-%05d',x), printf('page-%05d',x) FROM n; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10003) INSERT INTO client_outbox SELECT printf('page-op-%05d',x), 1, x+1, printf('document-%05d',x), printf('page-%05d',x), 1, 'rev', NULL, X'61', 'hash', 'sent', NULL, NULL, NULL, 'none' FROM n;").execute(&mut fixture).await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let mut cursor = None;
    let mut drafts = Vec::new();
    loop {
        let page = recovery.drafts(cursor.as_deref()).await.unwrap();
        assert!(page.items.len() <= 100);
        drafts.extend(page.items.into_iter().map(|item| item.storage_id));
        cursor = page.next_after_id;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(drafts.len(), 10005);
    assert!(drafts.windows(2).all(|pair| pair[0] < pair[1]));
    let mut cursor = None;
    let mut operations = Vec::new();
    loop {
        let page = recovery.operations(cursor.as_deref()).await.unwrap();
        assert!(page.items.len() <= 100);
        operations.extend(page.items.into_iter().map(|item| item.operation_id));
        cursor = page.next_after_id;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(operations.len(), 10004);
    assert!(operations.windows(2).all(|pair| pair[0] < pair[1]));
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn snapshot_contains_wal_and_never_replaces_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let fixture = failed_v3_fixture(&path).await;
    assert!(path.with_extension("db-wal").exists());
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let existing = dir.path().join("existing.db");
    std::fs::write(&existing, b"existing").unwrap();
    assert!(recovery.export_snapshot(&existing).await.is_err());
    assert_eq!(std::fs::read(&existing).unwrap(), b"existing");
    let target = dir.path().join("snapshot.db");
    recovery.export_snapshot(&target).await.unwrap();
    let mut snapshot = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&target)
            .read_only(true),
    )
    .await
    .unwrap();
    let checkpoint: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM draft_checkpoints WHERE document_id = 'old-storage'")
            .fetch_one(&mut snapshot)
            .await
            .unwrap();
    let base: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM draft_bases WHERE document_id = 'old-storage'")
            .fetch_one(&mut snapshot)
            .await
            .unwrap();
    let outbox: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM client_outbox WHERE operation_id = 'op-a'")
            .fetch_one(&mut snapshot)
            .await
            .unwrap();
    assert_eq!(checkpoint, b"\xef\xbb\xbfold\r\n");
    assert_eq!(base, b"base\r\n");
    assert_eq!(outbox, b"payload\n");
    snapshot.close().await.unwrap();
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn recovery_and_normal_store_share_the_same_single_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        RecoveryStore::open(&path).await,
        Err(StoreError::StoreLocked)
    ));
    store.close().await.unwrap();
    let recovery = RecoveryStore::open(&path).await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::StoreLocked)
    ));
    drop(recovery);
    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}
