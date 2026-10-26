use notryx_client_store::{ClientStore, StoreError};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

#[tokio::test]
async fn committed_checkpoint_and_first_base_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let base = "\u{feff}---\r\ntitle: 한글\r\n---\r\n🙂 e\u{301}".as_bytes();
    let edited = "\u{feff}---\r\ntitle: 수정\r\n---\r\n🙂 e\u{301}".as_bytes();
    let mut store = ClientStore::open(&path).await.unwrap();
    let document = store
        .open_document("document-a", base, "rev-a")
        .await
        .unwrap();
    let saved = store
        .save_checkpoint("document-a", &document.edit_session_id, 1, edited)
        .await
        .expect("the first dirty checkpoint must commit before acknowledging it");
    assert_eq!(saved.generation, 1);
    assert_eq!(saved.edit_session_id, document.edit_session_id);
    store.close().await.unwrap();

    let mut reopened = ClientStore::open(&path).await.unwrap();
    let draft = reopened
        .read_checkpoint("document-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 1);
    assert_eq!(draft.bytes, edited);
    assert_eq!(draft.base_bytes, base);
    assert_eq!(draft.base_revision, "rev-a");
    let opened = reopened
        .open_document("document-a", b"new remote bytes", "rev-b")
        .await
        .unwrap();
    assert_eq!(opened.bytes, edited);
    assert_eq!(opened.generation, 1);
    assert_ne!(opened.edit_session_id, document.edit_session_id);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn old_session_cannot_write_after_document_is_closed_and_reopened() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let old = store
        .open_document("document-a", b"base", "rev-a")
        .await
        .unwrap();
    assert_eq!(
        store.open_document("document-a", b"base", "rev-a").await,
        Err(StoreError::SessionAlreadyOpen)
    );
    store
        .save_checkpoint("document-a", &old.edit_session_id, 1, b"first draft")
        .await
        .unwrap();
    store
        .close_document("document-a", &old.edit_session_id)
        .unwrap();
    let current = store
        .open_document("document-a", b"remote changed", "rev-b")
        .await
        .unwrap();
    assert_eq!(
        store
            .save_checkpoint("document-a", &old.edit_session_id, 2, b"late write")
            .await,
        Err(StoreError::StaleSession)
    );
    assert_eq!(
        store.close_document("document-a", &old.edit_session_id),
        Err(StoreError::StaleSession)
    );
    store
        .save_checkpoint("document-a", &current.edit_session_id, 2, b"current draft")
        .await
        .unwrap();
    let draft = store.read_checkpoint("document-a").await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"current draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "rev-a");
    store.close().await.unwrap();
}

#[tokio::test]
async fn rejects_another_writer_and_releases_lock_after_close() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::StoreLocked)
    ));
    store.close().await.unwrap();
    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn unknown_schema_is_rejected_without_reinitializing_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE future_draft (payload BLOB); INSERT INTO future_draft VALUES (X'010203'); PRAGMA user_version=99;")
        .execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    let original = std::fs::read(&path).unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn existing_empty_file_is_not_reinitialized() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    std::fs::write(&path, []).unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), Vec::<u8>::new());
    assert!(!directory.path().join("drafts.db-wal").exists());
}

#[tokio::test]
async fn existing_empty_sqlite_database_is_not_adopted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE abandoned (payload BLOB); DROP TABLE abandoned; VACUUM;")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let original = std::fs::read(&path).unwrap();
    assert!(!original.is_empty());
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!directory.path().join("drafts.db-wal").exists());
}

#[tokio::test]
async fn corrupt_database_is_not_deleted_or_reinitialized() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let original = b"not a sqlite file; preserve this for recovery";
    std::fs::write(&path, original).unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn validates_native_size_and_generation_limits_without_changing_the_draft() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    assert_eq!(
        store.open_document("", b"base", "rev-a").await,
        Err(StoreError::InvalidDocument)
    );
    let document = store
        .open_document("document-a", b"base", "rev-a")
        .await
        .unwrap();
    let large = vec![b'a'; notryx_client_store::MAX_DOCUMENT_BYTES];
    store
        .save_checkpoint("document-a", &document.edit_session_id, 1, &large)
        .await
        .unwrap();
    let oversized = vec![b'a'; notryx_client_store::MAX_DOCUMENT_BYTES + 1];
    assert_eq!(
        store
            .save_checkpoint("document-a", &document.edit_session_id, 2, &oversized)
            .await,
        Err(StoreError::DocumentTooLarge)
    );
    assert_eq!(
        store
            .save_checkpoint(
                "document-a",
                &document.edit_session_id,
                0,
                b"bad generation"
            )
            .await,
        Err(StoreError::InvalidGeneration)
    );
    assert_eq!(
        store
            .save_checkpoint(
                "document-a",
                &document.edit_session_id,
                u64::MAX,
                b"bad generation"
            )
            .await,
        Err(StoreError::InvalidGeneration)
    );
    let draft = store.read_checkpoint("document-a").await.unwrap().unwrap();
    assert_eq!(draft.generation, 1);
    assert_eq!(draft.bytes, large);
    store.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_write_failure_preserves_previous_checkpoint_and_base() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let mut store = ClientStore::open(&path).await.unwrap();
    let document = store
        .open_document("document-a", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_checkpoint("document-a", &document.edit_session_id, 1, b"first draft")
        .await
        .unwrap();
    let mut fault = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_checkpoint BEFORE UPDATE ON draft_checkpoints BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;")
        .execute(&mut fault).await.unwrap();
    assert_eq!(
        store
            .save_checkpoint("document-a", &document.edit_session_id, 2, b"not committed")
            .await,
        Err(StoreError::Storage)
    );
    let draft = store.read_checkpoint("document-a").await.unwrap().unwrap();
    assert_eq!(draft.generation, 1);
    assert_eq!(draft.bytes, b"first draft");
    assert_eq!(draft.base_bytes, b"base");
    sqlx::query("DROP TRIGGER fail_checkpoint")
        .execute(&mut fault)
        .await
        .unwrap();
    fault.close().await.unwrap();
    store
        .save_checkpoint(
            "document-a",
            &document.edit_session_id,
            2,
            b"retry succeeds",
        )
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn actual_database_uses_wal_and_commits_readable_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let mut store = ClientStore::open(&path).await.unwrap();
    let document = store
        .open_document("document-a", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_checkpoint("document-a", &document.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    assert!(directory.path().join("drafts.db-wal").exists());
    let mut reader = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut reader)
        .await
        .unwrap();
    assert_eq!(journal, "wal");
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut reader)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    reader.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn first_dirty_save_commits_base_and_checkpoint_together_or_neither() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let mut store = ClientStore::open(&path).await.unwrap();
    let document = store
        .open_document("document-a", b"base", "rev-a")
        .await
        .unwrap();
    let mut fault = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_base BEFORE INSERT ON draft_bases BEGIN SELECT RAISE(ABORT, 'injected base failure'); END;")
        .execute(&mut fault).await.unwrap();
    assert_eq!(
        store
            .save_checkpoint("document-a", &document.edit_session_id, 1, b"draft")
            .await,
        Err(StoreError::Storage)
    );
    assert!(store.read_checkpoint("document-a").await.unwrap().is_none());
    sqlx::query("DROP TRIGGER fail_base")
        .execute(&mut fault)
        .await
        .unwrap();
    fault.close().await.unwrap();
    store
        .save_checkpoint("document-a", &document.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    let draft = reopened
        .read_checkpoint("document-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
    reopened.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_path_alias_does_not_bypass_the_store_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let alias = directory.path().join("alias.db");
    let store = ClientStore::open(&path).await.unwrap();
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    assert!(matches!(
        ClientStore::open(&alias).await,
        Err(StoreError::StoreLocked)
    ));
    store.close().await.unwrap();
}

#[tokio::test]
async fn checkpoint_retry_is_idempotent_and_cannot_rewind_or_replace_a_generation() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let document = store
        .open_document("document-a", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_checkpoint("document-a", &document.edit_session_id, 7, b"new draft")
        .await
        .unwrap();
    assert_eq!(
        store
            .save_checkpoint("document-a", &document.edit_session_id, 6, b"old draft")
            .await,
        Err(StoreError::StaleGeneration),
    );
    assert_eq!(
        store
            .save_checkpoint(
                "document-a",
                &document.edit_session_id,
                7,
                b"different draft"
            )
            .await,
        Err(StoreError::GenerationMismatch),
    );
    let retry = store
        .save_checkpoint("document-a", &document.edit_session_id, 7, b"new draft")
        .await
        .unwrap();
    assert_eq!(retry.generation, 7);
    let draft = store.read_checkpoint("document-a").await.unwrap().unwrap();
    assert_eq!(draft.generation, 7);
    assert_eq!(draft.bytes, b"new draft");
    assert_eq!(draft.base_bytes, b"base");
    store.close().await.unwrap();
}

#[tokio::test]
async fn renderer_reload_reclaims_sessions_preserves_drafts_and_rejects_old_requests() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let first = store
        .open_document("document-a", b"base\r\n", "base-rev")
        .await
        .unwrap();
    let second = store
        .open_document("document-b", b"other base", "other-rev")
        .await
        .unwrap();
    store
        .save_checkpoint("document-a", &first.edit_session_id, 1, b"saved draft\n")
        .await
        .unwrap();
    store.reset_edit_sessions();
    let reopened = store
        .open_document("document-a", b"new remote", "new-rev")
        .await
        .unwrap();
    assert_eq!(reopened.generation, 1);
    assert_eq!(reopened.bytes, b"saved draft\n");
    assert_ne!(reopened.edit_session_id, first.edit_session_id);
    let base = store.read_checkpoint("document-a").await.unwrap().unwrap();
    assert_eq!(base.base_bytes, b"base\r\n");
    assert_eq!(base.base_revision, "base-rev");
    assert_eq!(
        store
            .save_checkpoint("document-a", &first.edit_session_id, 2, b"late stale")
            .await,
        Err(StoreError::StaleSession)
    );
    assert_eq!(
        store.close_document("document-a", &first.edit_session_id),
        Err(StoreError::StaleSession)
    );
    assert_eq!(
        store
            .save_checkpoint("document-b", &second.edit_session_id, 1, b"late other")
            .await,
        Err(StoreError::StaleSession)
    );
    store
        .save_checkpoint("document-a", &reopened.edit_session_id, 2, b"new draft\n")
        .await
        .unwrap();
    assert_eq!(
        store
            .read_checkpoint("document-a")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"new draft\n"
    );
    store.close().await.unwrap();
}
