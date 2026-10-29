use notryx_client_store::{
    ClientStore, OutboxAbandonState, OutboxStatus, PendingOutboxOperation, RecoveryOperationKind,
    RecoveryStore, RegistrationBinding, StoreError,
};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const OTHER: &str = "019a1000-0000-7000-8000-000000000003";
const ETAG: &str = "\"node-1\"";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

async fn database(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn fixture(path: &Path, registration: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(registration).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(path).await;
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    let target = json!({"node_id":TARGET,"parent_id":ROOT,"name":"target.md","kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":"meta-1","meta_etag":"\"meta-1\"","node_etag":ETAG,"content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"1"});
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for (id, state) in [(ROOT, root), (TARGET, target)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(id).bind(state.to_string()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn delete_preserves_confirmed_etag_identity_and_fifo_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let deleted = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    assert_eq!(deleted.operation_seq, 1);
    assert_eq!(deleted.node_id.to_string(), TARGET);
    assert_eq!(deleted.expected_node_etag, ETAG);
    assert_eq!(deleted.status, OutboxStatus::Queued);
    assert_eq!(
        Uuid::parse_str(&deleted.operation_id)
            .unwrap()
            .get_version_num(),
        7
    );
    let edit = store
        .open_registered_document(&registration, OTHER, b"other base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            OTHER,
            &edit.edit_session_id,
            1,
            b"other draft",
        )
        .await
        .unwrap();
    let content = store
        .queue_checkpoint(&registration, OTHER, 1)
        .await
        .unwrap();
    assert_eq!(content.operation_seq, 2);
    store
        .close_registered_document(&registration, OTHER, &edit.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Delete(pending) = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("delete must lead FIFO")
    };
    assert_eq!(pending, deleted);
    let draft = store
        .read_registered_checkpoint(&registration, OTHER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"other draft");
    assert_eq!(draft.base_bytes, b"other base");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    store.close().await.unwrap();
}

#[tokio::test]
async fn delete_rejects_stale_confirmation_and_closed_dirty_target_without_consuming_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(
        store
            .queue_delete(&registration, TARGET.parse().unwrap(), "\"old-node\"")
            .await
            .is_err()
    );
    let edit = store
        .open_registered_document(&registration, TARGET, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            TARGET,
            &edit.edit_session_id,
            1,
            b"unsent draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    assert!(
        store
            .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
            .await
            .is_err()
    );
    let draft = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"unsent draft");
    assert_eq!(draft.base_bytes, b"base");
    assert!(
        store
            .next_pending_outbox(&registration)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let allocated: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox_streams WHERE registration_id=1 AND next_seq > 1",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(allocated, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn replacement_registration_freezes_delete_and_preserves_other_draft() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let original = binding();
    fixture(&path, &original).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let deleted = store
        .queue_delete(&original, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    let edit = store
        .open_registered_document(&original, OTHER, b"other base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&original, OTHER, &edit.edit_session_id, 1, b"other draft")
        .await
        .unwrap();
    store
        .close_registered_document(&original, OTHER, &edit.edit_session_id)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = original.workspace_id.clone();
    replacement.workspace_epoch = original.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert!(matches!(
        store.next_pending_outbox(&original).await,
        Err(StoreError::StaleRegistration)
    ));
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let (registration_status, operation_status, sequence): (String, String, i64) = sqlx::query_as("SELECT r.status,o.status,o.operation_seq FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id=?")
        .bind(&deleted.operation_id).fetch_one(&mut db).await.unwrap();
    assert_eq!(
        (
            registration_status.as_str(),
            operation_status.as_str(),
            sequence
        ),
        ("frozen", "queued", 1)
    );
    let (bytes, base): (Vec<u8>, Vec<u8>) = sqlx::query_as("SELECT c.bytes,b.bytes FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.registration_id=1 AND d.document_id=?")
        .bind(OTHER).fetch_one(&mut db).await.unwrap();
    assert_eq!(bytes, b"other draft");
    assert_eq!(base, b"other base");
    db.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let page = recovery.operations(None).await.unwrap();
    let saved = page
        .items
        .iter()
        .find(|item| item.operation_id == deleted.operation_id)
        .unwrap();
    assert_eq!(saved.kind, Some(RecoveryOperationKind::Delete));
    assert_eq!(saved.expected_node_etag.as_deref(), Some(ETAG));
    assert_eq!(saved.registration_status.as_deref(), Some("frozen"));
    assert!(!saved.metadata_error);
}

#[tokio::test]
async fn delete_queue_failure_rolls_back_sequence_and_keeps_existing_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&registration, OTHER, b"other base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            OTHER,
            &edit.edit_session_id,
            1,
            b"other draft",
        )
        .await
        .unwrap();
    let content = store
        .queue_checkpoint(&registration, OTHER, 1)
        .await
        .unwrap();
    store
        .close_registered_document(&registration, OTHER, &edit.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = database(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_delete BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='delete' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store
            .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
            .await,
        Err(StoreError::Storage)
    ));
    let pending = store
        .read_outbox_operation(&registration, &content.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.bytes, b"other draft");
    assert_eq!(pending.operation_seq, 1);
    let draft = store
        .read_registered_checkpoint(&registration, OTHER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"other draft");
    assert_eq!(draft.base_bytes, b"other base");
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let next: i64 =
        sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(next, 2);
    sqlx::query("DROP TRIGGER reject_delete")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let deleted = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    assert_eq!(deleted.operation_seq, 2);
    assert!(matches!(
        store.next_pending_outbox(&registration).await.unwrap(),
        Some(PendingOutboxOperation::Content(_))
    ));
    store.close().await.unwrap();
}

fn deleted_receipt(
    registration: &RegistrationBinding,
    operation_id: &str,
    node_id: &str,
) -> notryx_protocol::OperationReceipt {
    serde_json::from_value(json!({"status":"deleted","operation_id":operation_id,"operation_seq":"1","workspace_epoch":registration.workspace_epoch,
        "node":{"node_id":node_id,"parent_id":ROOT,"name":"target.md","kind":"file","deleted":true,"content_rev":"content-2","content_etag":"\"content-2\"","meta_rev":"meta-2","meta_etag":"\"meta-2\"","node_etag":"\"node-2\"","content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"2"},"commit_seq":"2"})).unwrap()
}

#[tokio::test]
async fn delete_receipt_checks_identity_and_advances_ack_without_moving_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let item = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    let prepared = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(prepared, PendingOutboxOperation::Delete(ref saved) if saved.status == OutboxStatus::Sent)
    );
    let invalid = deleted_receipt(&registration, &item.operation_id, OTHER);
    assert!(matches!(
        store.apply_delete_receipt(&registration, &invalid).await,
        Err(StoreError::InvalidReceipt)
    ));
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    assert_eq!(
        store.next_pending_outbox(&registration).await.unwrap(),
        Some(prepared)
    );
    let receipt = deleted_receipt(&registration, &item.operation_id, TARGET);
    store
        .apply_delete_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert!(
        store
            .next_pending_outbox(&registration)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let (status, etag, saved): (String, String, String) = sqlx::query_as(
        "SELECT status,expected_revision,receipt_json FROM client_outbox WHERE operation_id=?",
    )
    .bind(&item.operation_id)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(status, "committed");
    assert_eq!(etag, ETAG);
    assert_eq!(
        serde_json::from_str::<notryx_protocol::OperationReceipt>(&saved).unwrap(),
        receipt
    );
    let cursor: String =
        sqlx::query_scalar("SELECT cursor FROM remote_index WHERE registration_id=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(cursor, "1");
    db.close().await.unwrap();
}

#[tokio::test]
async fn delete_precondition_failure_is_terminal_and_remains_visible_after_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let original = binding();
    fixture(&path, &original).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let item = store
        .queue_delete(&original, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    store.prepare_pending_outbox(&original).await.unwrap();
    let make = |etag: &str| {
        serde_json::from_value::<notryx_protocol::OperationReceipt>(json!({"status":"node_precondition_failed","operation_id":item.operation_id,"operation_seq":"1","workspace_epoch":original.workspace_epoch,"current_node_etag":etag})).unwrap()
    };
    assert!(matches!(
        store.apply_delete_receipt(&original, &make(ETAG)).await,
        Err(StoreError::InvalidReceipt)
    ));
    assert!(matches!(
        store
            .apply_delete_receipt(&original, &make("invalid"))
            .await,
        Err(StoreError::InvalidReceipt)
    ));
    assert_eq!(store.receipt_ack_through(&original).await.unwrap(), 0);
    let receipt = make("\"node-2\"");
    store
        .apply_delete_receipt(&original, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&original).await.unwrap(), 1);
    assert!(
        store
            .next_pending_outbox(&original)
            .await
            .unwrap()
            .is_none()
    );
    let mut replacement = binding();
    replacement.workspace_id = original.workspace_id.clone();
    replacement.workspace_epoch = original.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    store.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let page = recovery.operations(None).await.unwrap();
    let saved = page
        .items
        .iter()
        .find(|row| row.operation_id == item.operation_id)
        .unwrap();
    assert_eq!(saved.kind, Some(RecoveryOperationKind::Delete));
    assert_eq!(saved.expected_node_etag.as_deref(), Some(ETAG));
    assert_eq!(
        saved.terminal_kind.as_deref(),
        Some("node_precondition_failed")
    );
    assert_eq!(saved.registration_status.as_deref(), Some("frozen"));
    assert!(!saved.metadata_error);
    drop(recovery);
    let mut db = database(&path).await;
    let (status, saved): (String, String) =
        sqlx::query_as("SELECT status,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(&item.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(status, "conflict");
    assert_eq!(
        serde_json::from_str::<notryx_protocol::OperationReceipt>(&saved).unwrap(),
        receipt
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn queued_delete_prevents_reopening_target_but_not_other_documents_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let delete = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store
            .open_registered_document(&registration, TARGET, b"base", "content-1")
            .await,
        Err(StoreError::OutboxBlocked)
    ));
    let other = store
        .open_registered_document(&registration, OTHER, b"other base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            OTHER,
            &other.edit_session_id,
            1,
            b"other draft",
        )
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, OTHER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"other draft");
    assert_eq!(draft.base_bytes, b"other base");
    assert!(
        matches!(store.next_pending_outbox(&registration).await.unwrap(), Some(PendingOutboxOperation::Delete(item)) if item.operation_id == delete.operation_id)
    );
    store
        .close_registered_document(&registration, OTHER, &other.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn pending_delete_does_not_reopen_a_clean_checkpoint_as_active_editor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&registration, TARGET, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, TARGET, &edit.edit_session_id, 1, b"base")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    let item = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    assert!(matches!(
        store.try_open_registered_draft(&registration, TARGET).await,
        Err(StoreError::OutboxBlocked)
    ));
    let checkpoint = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.bytes, b"base");
    assert_eq!(checkpoint.base_bytes, b"base");
    assert!(
        matches!(store.next_pending_outbox(&registration).await.unwrap(), Some(PendingOutboxOperation::Delete(saved)) if saved.operation_id == item.operation_id)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn committed_delete_blocks_stale_index_until_newer_restored_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let item = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    store.prepare_pending_outbox(&registration).await.unwrap();
    store
        .apply_delete_receipt(
            &registration,
            &deleted_receipt(&registration, &item.operation_id, TARGET),
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .open_registered_document(&registration, TARGET, b"base", "content-1")
            .await,
        Err(StoreError::OutboxBlocked)
    ));
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let old: String =
        sqlx::query_scalar("SELECT state FROM remote_nodes WHERE registration_id=1 AND node_id=?")
            .bind(TARGET)
            .fetch_one(&mut db)
            .await
            .unwrap();
    let mut restored: serde_json::Value = serde_json::from_str(&old).unwrap();
    restored["last_change_seq"] = json!("3");
    restored["content_rev"] = json!("content-3");
    restored["content_etag"] = json!("\"content-3\"");
    restored["meta_rev"] = json!("meta-3");
    restored["meta_etag"] = json!("\"meta-3\"");
    restored["node_etag"] = json!("\"node-3\"");
    sqlx::query("UPDATE remote_nodes SET state=? WHERE registration_id=1 AND node_id=?")
        .bind(restored.to_string())
        .bind(TARGET)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let opened = store
        .open_registered_document(&registration, TARGET, b"base", "content-3")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn frozen_recovery_edit_blocks_delete_without_blocking_draft_save() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let active = binding();
    fixture(&path, &active).await;
    let mut old = binding();
    old.workspace_id = active.workspace_id.clone();
    old.workspace_epoch = active.workspace_epoch.clone();
    let mut db = database(&path).await;
    sqlx::query("INSERT INTO client_registrations VALUES (2,?,?,?,?,?,'frozen')")
        .bind(&old.server_origin)
        .bind(&old.workspace_id)
        .bind(&old.workspace_epoch)
        .bind(&old.device_id)
        .bind(&old.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO registered_documents VALUES (2,?,'old-storage')")
        .bind(TARGET)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO draft_checkpoints VALUES ('old-storage',1,1,X'62617365'); INSERT INTO draft_bases VALUES ('old-storage',X'62617365','content-1'); INSERT INTO outbox_streams VALUES (2,2,1,1)").execute(&mut db).await.unwrap();
    let hash = format!(
        "sha256:{}",
        Sha256::digest(b"base")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,committed_revision,payload_len) VALUES ('old-content',2,1,?,'old-storage',1,'earlier',?,?, 'committed','content-1',4)")
        .bind(TARGET).bind(&b"base"[..]).bind(hash).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let recovery = store.open_recovery_draft(&old, TARGET).await.unwrap();
    assert!(
        store
            .queue_delete(&active, TARGET.parse().unwrap(), ETAG)
            .await
            .is_err()
    );
    store
        .save_recovery_checkpoint(
            &old,
            TARGET,
            &recovery.edit_session_id,
            2,
            b"recovered edit",
        )
        .await
        .unwrap();
    store
        .close_recovery_draft(&old, TARGET, &recovery.edit_session_id)
        .await
        .unwrap();
    let saved = store
        .read_registered_checkpoint(&old, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bytes, b"recovered edit");
    assert_eq!(saved.base_bytes, b"base");
    store.close().await.unwrap();
}

#[tokio::test]
async fn dirty_recovery_remains_editable_but_unsent_delete_is_not_prepared() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&registration, TARGET, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, TARGET, &edit.edit_session_id, 1, b"base")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = database(&path).await;
    sqlx::query("UPDATE draft_checkpoints SET generation=2,bytes=? WHERE document_id=(SELECT storage_id FROM registered_documents WHERE registration_id=1 AND document_id=?)").bind(&b"recovered draft"[..]).bind(TARGET).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let (opened, active) = store
        .try_open_registered_draft(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert!(active);
    assert!(matches!(
        store.prepare_pending_outbox(&registration).await,
        Err(StoreError::OutboxBlocked)
    ));
    store
        .save_registered_checkpoint(
            &registration,
            TARGET,
            &opened.edit_session_id,
            3,
            b"recovered edit",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &opened.edit_session_id)
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"recovered edit");
    assert_eq!(draft.base_bytes, b"base");
    store.close().await.unwrap();
}

#[tokio::test]
async fn delete_abandon_intent_survives_restart_and_acknowledges_without_skipping_fifo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_delete(&registration, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    let PendingOutboxOperation::Delete(sent) = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("delete")
    };
    assert!(
        !store
            .request_delete_abandon_matching(&registration, &queued)
            .await
            .unwrap()
    );
    assert!(
        store
            .request_delete_abandon_matching(&registration, &sent)
            .await
            .unwrap()
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let pending = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&pending, PendingOutboxOperation::Delete(item) if item.status == OutboxStatus::Abandoning && item.abandon_state == OutboxAbandonState::Requested && item.expected_node_etag == ETAG)
    );
    let prepared = store
        .prepare_pending_outbox_matching(&registration, &pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&prepared, PendingOutboxOperation::Delete(item) if item.abandon_state == OutboxAbandonState::Sent)
    );
    let receipt = serde_json::from_value::<notryx_protocol::OperationReceipt>(json!({"status":"abandoned","operation_id":sent.operation_id,"operation_seq":"1","workspace_epoch":registration.workspace_epoch})).unwrap();
    store
        .apply_delete_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert!(
        store
            .next_pending_outbox(&registration)
            .await
            .unwrap()
            .is_none()
    );
    let opened = store
        .open_registered_document(&registration, TARGET, b"base", "content-1")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
}

async fn assert_clean_frozen_recovery_blocked(sent: bool) {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let active = binding();
    fixture(&path, &active).await;
    let mut old = binding();
    old.workspace_id = active.workspace_id.clone();
    old.workspace_epoch = active.workspace_epoch.clone();
    let mut db = database(&path).await;
    sqlx::query("INSERT INTO client_registrations VALUES (2,?,?,?,?,?,'frozen')")
        .bind(&old.server_origin)
        .bind(&old.workspace_id)
        .bind(&old.workspace_epoch)
        .bind(&old.device_id)
        .bind(&old.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO registered_documents VALUES (2,?,'old-storage')")
        .bind(TARGET)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO draft_checkpoints VALUES ('old-storage',1,1,X'62617365'); INSERT INTO draft_bases VALUES ('old-storage',X'62617365','content-1'); INSERT INTO outbox_streams VALUES (2,2,1,1)").execute(&mut db).await.unwrap();
    let hash = format!(
        "sha256:{}",
        Sha256::digest(b"base")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,committed_revision,payload_len) VALUES ('old-content',2,1,?,'old-storage',1,'earlier',?,?, 'committed','content-1',4)")
        .bind(TARGET).bind(&b"base"[..]).bind(hash).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_delete(&active, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    if sent {
        store.prepare_pending_outbox(&active).await.unwrap();
    }
    assert!(matches!(
        store.try_open_registered_draft(&old, TARGET).await,
        Err(StoreError::OutboxBlocked)
    ));
    assert!(matches!(
        store.open_recovery_draft(&old, TARGET).await,
        Err(StoreError::OutboxBlocked)
    ));
    let saved = store
        .read_registered_checkpoint(&old, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bytes, b"base");
    assert_eq!(saved.base_bytes, b"base");
    assert!(
        matches!(store.next_pending_outbox(&active).await.unwrap(), Some(PendingOutboxOperation::Delete(ref item)) if item.operation_id == queued.operation_id)
    );
    store.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(
        recovery.checkpoint_bytes("old-storage").await.unwrap(),
        b"base"
    );
    assert_eq!(recovery.base_bytes("old-storage").await.unwrap(), b"base");
}

#[tokio::test]
async fn pending_delete_prevents_new_clean_frozen_recovery_session() {
    for sent in [false, true] {
        assert_clean_frozen_recovery_blocked(sent).await;
    }
}
