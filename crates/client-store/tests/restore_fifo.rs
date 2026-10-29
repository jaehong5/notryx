use notryx_client_store::{
    ClientStore, OutboxStatus, PendingOutboxOperation, RecoveryOperationKind, RecoveryStore,
    RegistrationBinding, StoreError,
};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
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
fn hash(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}
const RESTORE: &[u8] = br#"{"expected_meta_rev":"meta-1","parent_id":"019a1000-0000-7000-8000-000000000001","name":"restored.md"}"#;

async fn fixture(path: &std::path::Path, registration: &RegistrationBinding, corrupt: bool) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(registration).await.unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("INSERT INTO registered_documents VALUES (1, 'doc', 'storage'); INSERT INTO outbox_streams VALUES (1, 3, 0, 0);").execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, operation_kind, document_id, bytes, payload_hash, status, payload_len) VALUES ('019a1000-0000-7000-8000-000000000003', 1, 1, 'restore', '019a1000-0000-7000-8000-000000000002', ?, ?, 'queued', ?)")
        .bind(RESTORE).bind(if corrupt { hash(b"wrong") } else { hash(RESTORE) }).bind(RESTORE.len() as i64).execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_hash, status, payload_len) VALUES ('content-2', 1, 2, 'doc', 'storage', 1, 'rev-a', ?, ?, 'queued', 4)")
        .bind(&b"edit"[..]).bind(hash(b"edit")).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn mixed_fifo_selects_restore_before_later_content_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let item = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let PendingOutboxOperation::Restore(operation) = item else {
        panic!("restore must retain FIFO priority")
    };
    assert_eq!(
        operation.operation_id,
        "019a1000-0000-7000-8000-000000000003"
    );
    assert_eq!(operation.operation_seq, 1);
    assert_eq!(operation.status, OutboxStatus::Queued);
    assert_eq!(operation.request.name, "restored.md");
    store.close().await.unwrap();
}

#[tokio::test]
async fn corrupt_restore_blocks_later_content_and_reregistration_freezes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, true).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(store.next_pending_outbox(&registration).await.is_err());
    let mut replacement = binding();
    replacement.workspace_id = registration.workspace_id.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert!(matches!(
        store.next_pending_outbox(&registration).await,
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
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let page = recovery.operations(None).await.unwrap();
    let restored = page
        .items
        .iter()
        .find(|item| item.operation_id == "019a1000-0000-7000-8000-000000000003")
        .unwrap();
    assert_eq!(restored.registration_status.as_deref(), Some("frozen"));
    assert_eq!(restored.kind, Some(RecoveryOperationKind::Restore));
    assert_eq!(
        recovery
            .operation_bytes("019a1000-0000-7000-8000-000000000003")
            .await
            .unwrap(),
        RESTORE
    );
}

#[tokio::test]
async fn prepare_restore_commits_sent_before_retry_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let first = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let PendingOutboxOperation::Restore(ref operation) = first else {
        panic!("restore must lead the FIFO")
    };
    assert_eq!(operation.status, OutboxStatus::Sent);
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let retry = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry, first);
    store.close().await.unwrap();
}

#[tokio::test]
async fn prepare_restore_failure_preserves_fifo_and_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_restore_sent BEFORE UPDATE OF status ON client_outbox WHEN OLD.operation_kind='restore' BEGIN SELECT RAISE(ABORT, 'synthetic write failure'); END;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store.prepare_pending_outbox(&registration).await,
        Err(StoreError::Storage)
    ));
    let item = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let PendingOutboxOperation::Restore(operation) = item else {
        panic!("failure must not skip the restore")
    };
    assert_eq!(operation.status, OutboxStatus::Queued);
    assert_eq!(operation.request.name, "restored.md");
    store.close().await.unwrap();
}

fn restored_receipt(
    registration: &RegistrationBinding,
    parent: &str,
) -> notryx_protocol::OperationReceipt {
    serde_json::from_value(serde_json::json!({
        "status": "restored", "operation_id": "019a1000-0000-7000-8000-000000000003", "operation_seq": "1",
        "workspace_epoch": registration.workspace_epoch,
        "node": {"node_id": "019a1000-0000-7000-8000-000000000002", "parent_id": parent, "name": "restored.md", "kind": "folder", "deleted": false, "content_rev": null, "content_etag": null, "meta_rev": "meta-2", "meta_etag": "\"meta-2\"", "node_etag": "\"node-2\"", "content_hash": null, "size": null, "children_generation": "0", "last_change_seq": "11"},
        "commit_seq": "11"
    })).unwrap()
}

#[tokio::test]
async fn restore_receipt_checks_identity_commits_ack_and_never_moves_metadata_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO remote_index VALUES (1, '019a1000-0000-7000-8000-000000000001', '10')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let wrong = restored_receipt(&registration, "019a1000-0000-7000-8000-000000000004");
    assert!(matches!(
        store.apply_restore_receipt(&registration, &wrong).await,
        Err(StoreError::InvalidReceipt)
    ));
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    let good = restored_receipt(&registration, "019a1000-0000-7000-8000-000000000001");
    store
        .apply_restore_receipt(&registration, &good)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert!(matches!(
        store.next_pending_outbox(&registration).await.unwrap(),
        Some(PendingOutboxOperation::Content(_))
    ));
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let cursor: String =
        sqlx::query_scalar("SELECT cursor FROM remote_index WHERE registration_id=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(cursor, "10");
    let receipt: String =
        sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_seq=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<notryx_protocol::OperationReceipt>(&receipt).unwrap(),
        good
    );
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .apply_restore_receipt(&registration, &good)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    store.close().await.unwrap();
}

#[tokio::test]
async fn restore_receipt_ack_failure_rolls_back_receipt_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_restore_ack BEFORE UPDATE OF ack_through ON outbox_streams BEGIN SELECT RAISE(ABORT, 'synthetic write failure'); END;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let good = restored_receipt(&registration, "019a1000-0000-7000-8000-000000000001");
    assert!(matches!(
        store.apply_restore_receipt(&registration, &good).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    let item = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let PendingOutboxOperation::Restore(operation) = item else {
        panic!("restore must remain pending")
    };
    assert_eq!(operation.status, OutboxStatus::Sent);
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let receipt: Option<String> =
        sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_seq=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(receipt.is_none());
    db.close().await.unwrap();
}

#[tokio::test]
async fn terminal_restore_failures_preserve_result_and_advance_ack() {
    for status in [
        "recovery_conflict",
        "node_deleted",
        "not_deleted",
        "history_expired",
        "metadata_precondition_failed",
        "destination_exists",
        "parent_not_folder",
        "not_found",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drafts.db");
        let registration = binding();
        fixture(&path, &registration, false).await;
        let mut store = ClientStore::open(&path).await.unwrap();
        store
            .prepare_pending_outbox(&registration)
            .await
            .unwrap()
            .unwrap();
        let mut value = serde_json::json!({"status": status, "operation_id": "019a1000-0000-7000-8000-000000000003", "operation_seq": "1", "workspace_epoch": registration.workspace_epoch});
        if status == "metadata_precondition_failed" {
            value["current_meta_rev"] = serde_json::json!("meta-2");
            value["current_meta_etag"] = serde_json::json!("\"meta-2\"");
        }
        let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(value).unwrap();
        store
            .apply_restore_receipt(&registration, &receipt)
            .await
            .unwrap();
        assert_eq!(
            store.receipt_ack_through(&registration).await.unwrap(),
            1,
            "{status}"
        );
        assert!(matches!(
            store.next_pending_outbox(&registration).await.unwrap(),
            Some(PendingOutboxOperation::Content(_))
        ));
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let (saved_status, terminal, json): (String, String, String) = sqlx::query_as(
            "SELECT status, terminal_kind, receipt_json FROM client_outbox WHERE operation_seq=1",
        )
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(saved_status, "conflict");
        assert_eq!(terminal, status);
        assert_eq!(
            serde_json::from_str::<notryx_protocol::OperationReceipt>(&json).unwrap(),
            receipt
        );
        db.close().await.unwrap();
        let mut recovery = RecoveryStore::open(&path).await.unwrap();
        let page = recovery.operations(None).await.unwrap();
        let item = page
            .items
            .iter()
            .find(|item| item.operation_id == "019a1000-0000-7000-8000-000000000003")
            .unwrap();
        assert_eq!(item.terminal_kind.as_deref(), Some(status));
        assert_eq!(
            recovery.operation_bytes(&item.operation_id).await.unwrap(),
            RESTORE
        );
    }
}

#[tokio::test]
async fn unrelated_restore_receipt_identity_never_changes_pending_or_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let good = restored_receipt(&registration, "019a1000-0000-7000-8000-000000000001");
    let original = serde_json::to_value(good).unwrap();
    for (field, value) in [
        ("operation_id", "019a1000-0000-7000-8000-000000000004"),
        ("operation_seq", "2"),
        ("workspace_epoch", "00000000-0000-4000-8000-000000000005"),
    ] {
        let mut altered = original.clone();
        altered[field] = serde_json::json!(value);
        let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(altered).unwrap();
        assert!(
            matches!(
                store.apply_restore_receipt(&registration, &receipt).await,
                Err(StoreError::InvalidReceipt)
            ),
            "{field}"
        );
        assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
        assert!(matches!(
            store.next_pending_outbox(&registration).await.unwrap(),
            Some(PendingOutboxOperation::Restore(_))
        ));
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn content_listing_ignores_restore_without_skipping_fifo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let page = store.list_outbox(&registration, 0, 100).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation_id, "content-2");
    assert!(matches!(
        store.next_pending_outbox(&registration).await.unwrap(),
        Some(PendingOutboxOperation::Restore(_))
    ));
    store.close().await.unwrap();
}

#[tokio::test]
async fn content_edit_after_restore_keeps_original_content_and_gets_later_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("UPDATE client_outbox SET operation_seq=3 WHERE operation_id='content-2'; UPDATE client_outbox SET operation_seq=2 WHERE operation_kind='restore'; UPDATE client_outbox SET operation_seq=1, document_id='019a1000-0000-7000-8000-000000000002' WHERE operation_id='content-2'; UPDATE registered_documents SET document_id='019a1000-0000-7000-8000-000000000002' WHERE storage_id='storage'; INSERT INTO draft_checkpoints VALUES ('storage', 2, 1, X'6e657765722065646974'); INSERT INTO draft_bases VALUES ('storage', X'62617365', 'rev-a');").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_checkpoint(&registration, "019a1000-0000-7000-8000-000000000002", 2)
        .await
        .unwrap();
    assert_eq!(queued.operation_seq, 3);
    assert_eq!(queued.bytes, b"newer edit");
    assert_eq!(queued.expected_revision, None);
    let first = store
        .read_outbox_operation(&registration, "content-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.bytes, b"edit");
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let dependency: String =
        sqlx::query_scalar("SELECT dependency FROM client_outbox WHERE operation_seq=3")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(dependency, "content-2");
    db.close().await.unwrap();
}

#[tokio::test]
async fn ack_confirmation_never_retires_content_because_restore_was_committed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("UPDATE client_outbox SET operation_seq=3 WHERE operation_id='content-2'; UPDATE client_outbox SET operation_seq=2, status='committed' WHERE operation_kind='restore'; UPDATE client_outbox SET operation_seq=1, status='committed', committed_revision='rev-a', document_id='019a1000-0000-7000-8000-000000000002' WHERE operation_id='content-2'; UPDATE registered_documents SET document_id='019a1000-0000-7000-8000-000000000002' WHERE storage_id='storage'; UPDATE outbox_streams SET ack_through=2 WHERE registration_id=1;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let ack: notryx_protocol::AckResponse = serde_json::from_value(serde_json::json!({"workspace_epoch": registration.workspace_epoch, "device_stream_id": registration.device_stream_id, "last_seq": "2", "retired_through": "2"})).unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .confirm_receipt_ack(&registration, &ack)
        .await
        .unwrap();
    assert_eq!(
        store
            .confirmed_receipt_through(&registration)
            .await
            .unwrap(),
        2
    );
    let content = store
        .read_outbox_operation(&registration, "content-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(content.bytes, b"edit");
    store.close().await.unwrap();
}

#[tokio::test]
async fn content_document_status_does_not_decode_restore_as_a_draft_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("UPDATE client_outbox SET operation_seq=3 WHERE operation_id='content-2'; UPDATE client_outbox SET operation_seq=2, status='conflict', terminal_kind='history_expired' WHERE operation_kind='restore'; UPDATE client_outbox SET operation_seq=1, document_id='019a1000-0000-7000-8000-000000000002' WHERE operation_id='content-2'; UPDATE registered_documents SET document_id='019a1000-0000-7000-8000-000000000002' WHERE storage_id='storage';").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let state = store
        .document_outbox_state(&registration, "019a1000-0000-7000-8000-000000000002")
        .await
        .unwrap();
    assert_eq!(state.latest.unwrap().operation_id, "content-2");
    assert!(!state.has_failure);
    store.close().await.unwrap();
}

#[tokio::test]
async fn frozen_restore_without_registered_draft_requires_reconciliation_before_new_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut replacement = binding();
    replacement.workspace_id = registration.workspace_id.clone();
    let node = "019a1000-0000-7000-8000-000000000002";
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&replacement).await.unwrap();
    let opened = store
        .open_registered_document(&replacement, node, b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &replacement,
            node,
            &opened.edit_session_id,
            1,
            b"new local edit",
        )
        .await
        .unwrap();
    assert!(matches!(
        store.queue_checkpoint(&replacement, node, 1).await,
        Err(StoreError::ReconciliationRequired)
    ));
    let draft = store
        .read_registered_checkpoint(&replacement, node)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"new local edit");
    assert_eq!(draft.base_bytes, b"base");
    store
        .close_registered_document(&replacement, node, &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let page = recovery.operations(None).await.unwrap();
    let old = page
        .items
        .iter()
        .find(|item| item.operation_id == "019a1000-0000-7000-8000-000000000003")
        .unwrap();
    assert_eq!(old.registration_status.as_deref(), Some("frozen"));
    assert_eq!(
        recovery.operation_bytes(&old.operation_id).await.unwrap(),
        RESTORE
    );
}

#[tokio::test]
async fn frozen_restore_in_another_workspace_does_not_block_new_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let replacement = binding();
    let node = "019a1000-0000-7000-8000-000000000002";
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&replacement).await.unwrap();
    let opened = store
        .open_registered_document(&replacement, node, b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &replacement,
            node,
            &opened.edit_session_id,
            1,
            b"new local edit",
        )
        .await
        .unwrap();
    let queued = store.queue_checkpoint(&replacement, node, 1).await.unwrap();
    assert_eq!(queued.bytes, b"new local edit");
    assert_eq!(queued.operation_seq, 1);
    store
        .close_registered_document(&replacement, node, &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn queue_restore_allocates_stream_sequence_and_persists_request_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let node = "019a1000-0000-7000-8000-000000000005".parse().unwrap();
    let request: notryx_protocol::RestoreNodeRequest = serde_json::from_slice(RESTORE).unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_restore(&registration, node, &request)
        .await
        .unwrap();
    assert_eq!(queued.operation_seq, 3);
    assert_eq!(queued.node_id, node);
    assert_eq!(queued.request, request);
    assert_eq!(queued.status, OutboxStatus::Queued);
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let next: i64 =
        sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(next, 4);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_outbox WHERE operation_id=? AND operation_seq=3 AND operation_kind='restore'").bind(&queued.operation_id).fetch_one(&mut db).await.unwrap();
    assert_eq!(count, 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn queue_restore_failure_rolls_back_sequence_and_never_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let node = "019a1000-0000-7000-8000-000000000005".parse().unwrap();
    let request: notryx_protocol::RestoreNodeRequest = serde_json::from_slice(RESTORE).unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_restore_insert BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='restore' BEGIN SELECT RAISE(ABORT, 'synthetic insert failure'); END;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store.queue_restore(&registration, node, &request).await,
        Err(StoreError::Storage)
    ));
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let next: i64 =
        sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(next, 3);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_outbox")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 2);
    sqlx::query("DROP TRIGGER reject_restore_insert")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_restore(&registration, node, &request)
        .await
        .unwrap();
    assert_eq!(queued.operation_seq, 3);
    store.close().await.unwrap();
}

#[tokio::test]
async fn queue_restore_respects_frozen_unresolved_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut replacement = binding();
    replacement.workspace_id = registration.workspace_id.clone();
    let node = "019a1000-0000-7000-8000-000000000002".parse().unwrap();
    let request: notryx_protocol::RestoreNodeRequest = serde_json::from_slice(RESTORE).unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&replacement).await.unwrap();
    assert!(matches!(
        store.queue_restore(&replacement, node, &request).await,
        Err(StoreError::ReconciliationRequired)
    ));
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn recovery_not_applied_is_a_preserved_terminal_result_with_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(serde_json::json!({"status":"not_applied", "operation_id":"019a1000-0000-7000-8000-000000000003", "operation_seq":"1", "workspace_epoch":registration.workspace_epoch})).unwrap();
    store
        .apply_restore_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert!(matches!(
        store.next_pending_outbox(&registration).await.unwrap(),
        Some(PendingOutboxOperation::Content(_))
    ));
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let (terminal, saved): (String, String) = sqlx::query_as(
        "SELECT terminal_kind, receipt_json FROM client_outbox WHERE operation_seq=1",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(terminal, "not_applied");
    assert_eq!(
        serde_json::from_str::<notryx_protocol::OperationReceipt>(&saved).unwrap(),
        receipt
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn malformed_restored_node_never_commits_or_advances_ack() {
    for case in [
        "wrong_seq",
        "unchanged_meta",
        "wrong_content_etag",
        "bad_content_hash",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drafts.db");
        let registration = binding();
        fixture(&path, &registration, false).await;
        let mut value = serde_json::to_value(restored_receipt(
            &registration,
            "019a1000-0000-7000-8000-000000000001",
        ))
        .unwrap();
        match case {
            "wrong_seq" => value["node"]["last_change_seq"] = serde_json::json!("10"),
            "unchanged_meta" => {
                value["node"]["meta_rev"] = serde_json::json!("meta-1");
                value["node"]["meta_etag"] = serde_json::json!("\"meta-1\"");
            }
            "wrong_content_etag" | "bad_content_hash" => {
                value["node"]["kind"] = serde_json::json!("file");
                value["node"]["content_rev"] = serde_json::json!("content-1");
                value["node"]["content_etag"] =
                    serde_json::json!(if case == "wrong_content_etag" {
                        "\"other\""
                    } else {
                        "\"content-1\""
                    });
                value["node"]["content_hash"] = serde_json::json!(if case == "bad_content_hash" {
                    "sha256:bad".to_string()
                } else {
                    format!("sha256:{}", "0".repeat(64))
                });
                value["node"]["size"] = serde_json::json!("1");
            }
            _ => unreachable!(),
        }
        let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(value).unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        store
            .prepare_pending_outbox(&registration)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                store.apply_restore_receipt(&registration, &receipt).await,
                Err(StoreError::InvalidReceipt)
            ),
            "{case}"
        );
        assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
        assert!(matches!(
            store.next_pending_outbox(&registration).await.unwrap(),
            Some(PendingOutboxOperation::Restore(_))
        ));
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn typed_prepare_matching_rejects_a_changed_restore_without_marking_it_sent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let old = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let altered = br#"{"expected_meta_rev":"meta-1","parent_id":"019a1000-0000-7000-8000-000000000001","name":"other.md"}"#;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE client_outbox SET bytes=?, payload_hash=?, payload_len=? WHERE operation_seq=1",
    )
    .bind(altered.as_slice())
    .bind(hash(altered))
    .bind(altered.len() as i64)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    assert_eq!(
        store
            .prepare_pending_outbox_matching(&registration, &old)
            .await
            .unwrap(),
        None
    );
    let fresh = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let PendingOutboxOperation::Restore(ref operation) = fresh else {
        panic!("restore must remain at the FIFO head")
    };
    assert_eq!(operation.status, OutboxStatus::Queued);
    assert_eq!(operation.request.name, "other.md");
    let prepared = store
        .prepare_pending_outbox_matching(&registration, &fresh)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(prepared, PendingOutboxOperation::Restore(operation) if operation.status == OutboxStatus::Sent)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn restore_abandon_intent_is_durable_and_retires_fifo_only_after_receipt() {
    use notryx_client_store::OutboxAbandonState;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Restore(sent) = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("restore")
    };
    assert_eq!(sent.status, OutboxStatus::Sent);
    let mut stale = sent.clone();
    stale.request.name = "stale.md".into();
    assert!(
        !store
            .request_restore_abandon_matching(&registration, &stale)
            .await
            .unwrap()
    );
    assert!(
        store
            .request_restore_abandon_matching(&registration, &sent)
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
    let PendingOutboxOperation::Restore(ref intent) = pending else {
        panic!("restore")
    };
    assert_eq!(intent.status, OutboxStatus::Abandoning);
    assert_eq!(intent.abandon_state, OutboxAbandonState::Requested);
    assert_eq!(intent.request, sent.request);
    let prepared = store
        .prepare_pending_outbox_matching(&registration, &pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(prepared, PendingOutboxOperation::Restore(ref intent) if intent.abandon_state == OutboxAbandonState::Sent)
    );
    let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(serde_json::json!({"status":"abandoned", "operation_id":sent.operation_id, "operation_seq":"1", "workspace_epoch":registration.workspace_epoch})).unwrap();
    store
        .apply_restore_receipt(&registration, &receipt)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert!(matches!(
        store.next_pending_outbox(&registration).await.unwrap(),
        Some(PendingOutboxOperation::Content(_))
    ));
    let page = store
        .list_restore_operations(&registration, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.entries[0].operation.status, OutboxStatus::Abandoned);
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&receipt));
    store.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(
        recovery.operation_bytes(&sent.operation_id).await.unwrap(),
        RESTORE
    );
}

#[tokio::test]
async fn restore_abandon_write_failure_preserves_sent_request_and_stream() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    fixture(&path, &registration, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Restore(sent) = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("restore")
    };
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_abandon BEFORE UPDATE OF abandon_state ON client_outbox BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        store
            .request_restore_abandon_matching(&registration, &sent)
            .await,
        Err(StoreError::Storage)
    ));
    let current = store
        .next_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current, PendingOutboxOperation::Restore(sent));
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    store.close().await.unwrap();
}
