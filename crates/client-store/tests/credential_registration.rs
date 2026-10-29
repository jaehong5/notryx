use notryx_client_store::{
    ClientStore, CredentialPending, RecoveryStore, RegistrationBinding, StoreError,
};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn replacement(old: &RegistrationBinding) -> RegistrationBinding {
    RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..old.clone()
    }
}

async fn fixture(path: &std::path::Path) -> (ClientStore, RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    let old = binding();
    store.activate_registration(&old).await.unwrap();
    let opened = store
        .open_registered_document(&old, "doc", b"base", "revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    store
        .close_registered_document(&old, "doc", &opened.edit_session_id)
        .await
        .unwrap();
    store.queue_checkpoint(&old, "doc", 1).await.unwrap();
    (store, old)
}

async fn assert_preserved(store: &mut ClientStore, binding: &RegistrationBinding, frozen: bool) {
    let draft = store
        .read_registered_checkpoint(binding, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "revision");
    let page = store.list_outbox(binding, 0, 100).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].frozen, frozen);
    let operation = store
        .read_outbox_operation(binding, &page.entries[0].operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.bytes, b"draft");
    assert_eq!(operation.operation_seq, 1);
    if frozen {
        assert_eq!(
            store.receipt_ack_through(binding).await,
            Err(StoreError::StaleRegistration)
        );
    } else {
        assert_eq!(store.receipt_ack_through(binding).await.unwrap(), 0);
    }
    assert_eq!(store.confirmed_receipt_through(binding).await.unwrap(), 0);
}

async fn connect(path: &std::path::Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

#[tokio::test]
async fn credential_pair_commits_binding_and_freezes_old_outbox_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    let (mut store, old) = fixture(&path).await;
    assert_eq!(
        store.active_credential().await.unwrap(),
        Some((old.clone(), None))
    );
    let next = replacement(&old);
    let id = Uuid::new_v4();
    let pending = store.prepare_credential_pending(&next, id).await.unwrap();
    assert_eq!(
        store.pending_credential().await.unwrap(),
        Some(pending.clone())
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_preserved(&mut store, &old, false).await;
    store
        .commit_credential_pending(&pending, true)
        .await
        .unwrap();
    assert_eq!(store.pending_credential().await.unwrap(), None);
    assert_eq!(
        store.active_credential().await.unwrap(),
        Some((next.clone(), Some(id)))
    );
    assert_preserved(&mut store, &old, true).await;
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.active_credential().await.unwrap(),
        Some((next, Some(id)))
    );
    assert_preserved(&mut store, &old, true).await;
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    let ack: i64 = sqlx::query_scalar("SELECT s.ack_through FROM outbox_streams s JOIN client_registrations r ON r.id=s.registration_id WHERE r.device_id=?")
        .bind(&old.device_id).fetch_one(&mut db).await.unwrap();
    assert_eq!(ack, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn credential_session_only_keeps_no_secure_item_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(dir.path().join("client.sqlite"))
        .await
        .unwrap();
    assert_eq!(store.active_credential().await.unwrap(), None);
    let next = binding();
    let pending = store
        .prepare_credential_pending(&next, Uuid::new_v4())
        .await
        .unwrap();
    store
        .commit_credential_pending(&pending, false)
        .await
        .unwrap();
    assert_eq!(store.active_credential().await.unwrap(), Some((next, None)));
    assert!(store.pending_credential().await.unwrap().is_none());
}

#[tokio::test]
async fn credential_pending_does_not_overwrite_or_accept_stale_active() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, old) = fixture(&dir.path().join("client.sqlite")).await;
    let pending = store
        .prepare_credential_pending(&replacement(&old), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        store
            .prepare_credential_pending(&replacement(&old), Uuid::new_v4())
            .await,
        Err(StoreError::ReconciliationRequired)
    );
    assert_eq!(
        store.pending_credential().await.unwrap(),
        Some(pending.clone())
    );
    let other = replacement(&old);
    store.activate_registration(&other).await.unwrap();
    assert_eq!(
        store.commit_credential_pending(&pending, true).await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(store.current_registration().await.unwrap(), Some(other));
    assert_eq!(store.pending_credential().await.unwrap(), Some(pending));
    assert_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_discard_requires_exact_identity_and_preserves_registration() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, old) = fixture(&dir.path().join("client.sqlite")).await;
    let pending = store
        .prepare_credential_pending(&replacement(&old), Uuid::new_v4())
        .await
        .unwrap();
    let stale = CredentialPending {
        id: Uuid::new_v4(),
        ..pending.clone()
    };
    assert_eq!(
        store.discard_credential_pending(&stale).await,
        Err(StoreError::StaleRegistration)
    );
    let mut stale_binding = pending.clone();
    stale_binding.binding.workspace_epoch = Uuid::new_v4().to_string();
    assert_eq!(
        store.discard_credential_pending(&stale_binding).await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(
        store.pending_credential().await.unwrap(),
        Some(pending.clone())
    );
    store.discard_credential_pending(&pending).await.unwrap();
    assert_eq!(store.pending_credential().await.unwrap(), None);
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_preserved(&mut store, &old, false).await;
    assert_eq!(
        store.discard_credential_pending(&pending).await,
        Err(StoreError::StaleRegistration)
    );
}

#[tokio::test]
async fn credential_commit_failure_keeps_pending_active_and_outbox() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    let (mut store, old) = fixture(&path).await;
    let next = replacement(&old);
    let pending = store
        .prepare_credential_pending(&next, Uuid::new_v4())
        .await
        .unwrap();
    let mut db = connect(&path).await;
    sqlx::query("CREATE TRIGGER fail_credential_link BEFORE INSERT ON credential_links BEGIN SELECT RAISE(ABORT, 'synthetic'); END").execute(&mut db).await.unwrap();
    assert_eq!(
        store.commit_credential_pending(&pending, true).await,
        Err(StoreError::Storage)
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_eq!(
        store.pending_credential().await.unwrap(),
        Some(pending.clone())
    );
    assert_preserved(&mut store, &old, false).await;
    sqlx::query("DROP TRIGGER fail_credential_link")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.pending_credential().await.unwrap(),
        Some(pending.clone())
    );
    store
        .commit_credential_pending(&pending, true)
        .await
        .unwrap();
    assert_eq!(
        store.active_credential().await.unwrap(),
        Some((next, Some(pending.id)))
    );
    assert_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_commit_rejects_open_session_and_previously_frozen_binding() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, old) = fixture(&dir.path().join("client.sqlite")).await;
    let next = replacement(&old);
    let pending = store
        .prepare_credential_pending(&next, Uuid::new_v4())
        .await
        .unwrap();
    let opened = store
        .try_open_registered_draft(&old, "doc")
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(
        store.commit_credential_pending(&pending, true).await,
        Err(StoreError::OpenEditSessions)
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    store
        .close_registered_document(&old, "doc", &opened.edit_session_id)
        .await
        .unwrap();
    store
        .commit_credential_pending(&pending, true)
        .await
        .unwrap();
    assert_eq!(
        store.prepare_credential_pending(&old, Uuid::new_v4()).await,
        Err(StoreError::StaleRegistration)
    );
    assert_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_item_id_collision_and_corruption_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    let mut store = ClientStore::open(&path).await.unwrap();
    let first = binding();
    let id = Uuid::new_v4();
    let pending = store.prepare_credential_pending(&first, id).await.unwrap();
    store
        .commit_credential_pending(&pending, true)
        .await
        .unwrap();
    assert_eq!(
        store
            .prepare_credential_pending(&replacement(&first), id)
            .await,
        Err(StoreError::InvalidRegistration)
    );
    assert_eq!(
        store
            .prepare_credential_pending(&replacement(&first), Uuid::nil())
            .await,
        Err(StoreError::InvalidRegistration)
    );
    let mut db = connect(&path).await;
    sqlx::query("UPDATE credential_links SET item_id='malformed'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        store.active_credential().await,
        Err(StoreError::UnsupportedFormat)
    );
    assert_eq!(store.current_registration().await.unwrap(), Some(first));
    db.close().await.unwrap();
}

#[tokio::test]
async fn credential_v8_migration_preserves_data_and_rolls_back_on_schema_collision() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    let old = binding();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("fixtures/client_store_v7.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql(r#"
        PRAGMA defer_foreign_keys=ON;
        CREATE TABLE client_outbox_v8 (
            operation_id TEXT PRIMARY KEY NOT NULL, registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            operation_seq INTEGER NOT NULL CHECK(operation_seq>0), operation_kind TEXT NOT NULL DEFAULT 'content' CHECK(operation_kind IN ('content','restore')),
            document_id TEXT NOT NULL, storage_id TEXT REFERENCES registered_documents(storage_id), generation INTEGER,
            expected_revision TEXT, dependency TEXT REFERENCES client_outbox_v8(operation_id),
            bytes BLOB NOT NULL CHECK(length(bytes)<=5242880), payload_hash TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('queued','sent','committed','conflict')),
            committed_revision TEXT, conflict_revision TEXT, conflict_etag TEXT,
            abandon_state TEXT NOT NULL DEFAULT 'none' CHECK(abandon_state IN ('none','requested','sent')),
            terminal_kind TEXT CHECK(terminal_kind IN ('abandoned','node_deleted','recovery_conflict','not_found','not_applied','not_deleted','history_expired','metadata_precondition_failed','destination_exists','parent_not_folder')),
            payload_retired INTEGER NOT NULL DEFAULT 0 CHECK(payload_retired IN (0,1)),
            payload_len INTEGER NOT NULL DEFAULT 0 CHECK(payload_len>=0 AND payload_len<=5242880),
            receipt_json TEXT CHECK(receipt_json IS NULL OR length(receipt_json)<=65536),
            CHECK((operation_kind='content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation>0) OR (operation_kind='restore' AND storage_id IS NULL AND generation IS NULL)),
            UNIQUE(registration_id,operation_seq)
        );
        DROP TABLE client_outbox;
        ALTER TABLE client_outbox_v8 RENAME TO client_outbox;
        CREATE INDEX outbox_document_recent ON client_outbox(registration_id,document_id,operation_seq DESC);
        CREATE INDEX outbox_document_status ON client_outbox(registration_id,document_id,status,terminal_kind);
        CREATE INDEX outbox_dependencies ON client_outbox(registration_id,dependency,status);
        PRAGMA user_version=8;
    "#).execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO client_registrations VALUES (1,?,?,?,?,?,'active')")
        .bind(&old.server_origin)
        .bind(&old.workspace_id)
        .bind(&old.workspace_epoch)
        .bind(&old.device_id)
        .bind(&old.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO registered_documents VALUES (1,'doc','fixture-storage'); INSERT INTO draft_checkpoints VALUES ('fixture-storage',1,1,X'6472616674'); INSERT INTO draft_bases VALUES ('fixture-storage',X'62617365','revision'); INSERT INTO outbox_streams VALUES (1,2,0,0);").execute(&mut db).await.unwrap();
    let operation_id = Uuid::now_v7().to_string();
    let hash = format!(
        "sha256:{}",
        Sha256::digest(b"draft")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,payload_len) VALUES (?,1,1,'doc','fixture-storage',1,'revision',?,?,'queued',5)")
        .bind(operation_id).bind(b"draft".as_slice()).bind(hash).execute(&mut db).await.unwrap();
    sqlx::query("CREATE TABLE credential_links (synthetic_obstruction TEXT)")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 8);
    let pending_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE name='credential_pending'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(pending_table, 0);
    sqlx::query("DROP TABLE credential_links")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(recovery.drafts(None).await.unwrap().items.len(), 1);
    drop(recovery);
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.active_credential().await.unwrap(),
        Some((old.clone(), None))
    );
    assert_preserved(&mut store, &old, false).await;
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        21
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        0
    );
    let row = sqlx::query("PRAGMA journal_mode")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(
        row.try_get::<String, _>(0).unwrap().to_ascii_lowercase(),
        "wal"
    );
    db.close().await.unwrap();
}
