use notryx_client_store::{
    ClientStore, OutboxCommit, OutboxConflict, OutboxStatus, RegistrationBinding, StoreError,
};
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

fn server_ack(
    binding: &RegistrationBinding,
    last: u64,
    retired: u64,
) -> notryx_protocol::AckResponse {
    serde_json::from_value(serde_json::json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": last.to_string(),
        "retired_through": retired.to_string(),
    }))
    .unwrap()
}

async fn queue(
    store: &mut ClientStore,
    binding: &RegistrationBinding,
    document_id: &str,
) -> Result<notryx_client_store::OutboxOperation, StoreError> {
    let generation = store
        .read_registered_checkpoint(binding, document_id)
        .await?
        .ok_or(StoreError::MissingDraft)?
        .generation;
    store
        .queue_checkpoint(binding, document_id, generation)
        .await
}

async fn entries(
    store: &mut ClientStore,
    binding: &RegistrationBinding,
) -> Vec<notryx_client_store::OutboxOperation> {
    let page = store.list_outbox(binding, 0, 100).await.unwrap();
    assert!(page.next_after_seq.is_none());
    let mut operations = Vec::new();
    for item in page.entries {
        operations.push(
            store
                .read_outbox_operation(binding, &item.operation_id)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    operations
}

fn committed(op: &notryx_client_store::OutboxOperation, new_revision: &str) -> OutboxCommit {
    OutboxCommit {
        operation_id: op.operation_id.clone(),
        operation_seq: op.operation_seq,
        workspace_epoch: op.binding.workspace_epoch.clone(),
        document_id: op.document_id.clone(),
        content_revision: new_revision.into(),
        content_hash: op.payload_hash.clone(),
    }
}

#[test]
fn debugging_an_operation_does_not_print_document_bytes() {
    let op = notryx_client_store::OutboxOperation {
        binding: binding(),
        operation_id: "operation".into(),
        operation_seq: 1,
        document_id: "doc".into(),
        generation: 1,
        expected_revision: Some("rev-a".into()),
        conflict_revision: None,
        bytes: b"private document contents".to_vec(),
        payload_hash: "hash".into(),
        status: OutboxStatus::Sent,
        frozen: false,
        failure_kind: None,
        abandon_state: notryx_client_store::OutboxAbandonState::None,
    };
    let debug = format!("{op:?}");
    assert!(!debug.contains("private document contents"));
    assert!(!debug.contains("[112, 114, 105, 118"));
}

#[tokio::test]
async fn checkpoint_after_receipt_preserves_updated_base_without_reinserting_it() {
    use sqlx::Connection;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"original", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    let queued = queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(sent.operation_id, queued.operation_id);
    store
        .apply_outbox_commit(&registration, &committed(&sent, "rev-b"))
        .await
        .unwrap();

    let mut injection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_base_insert BEFORE INSERT ON draft_bases BEGIN SELECT RAISE(ABORT, 'unexpected base insertion'); END;")
        .execute(&mut injection).await.unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 2, b"newer")
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, b"newer");
    assert_eq!(draft.base_bytes, b"sent");
    assert_eq!(draft.base_revision, "rev-b");
    sqlx::query("DROP TRIGGER reject_base_insert")
        .execute(&mut injection)
        .await
        .unwrap();
    injection.close().await.unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer");
    assert_eq!(draft.base_bytes, b"sent");
    assert_eq!(draft.base_revision, "rev-b");
    let next = queue(&mut store, &registration, "doc").await.unwrap();
    assert_eq!(next.bytes, b"newer");
    assert_eq!(next.expected_revision.as_deref(), Some("rev-b"));
    store.close().await.unwrap();
}

#[tokio::test]
async fn late_success_preserves_the_latest_draft_and_connects_the_next_base() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"original", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            "doc",
            &opened.edit_session_id,
            5,
            b"generation five",
        )
        .await
        .unwrap();
    let first = queue(&mut store, &registration, "doc").await.unwrap();
    assert_eq!(first.operation_seq, 1);
    assert_eq!(first.generation, 5);
    assert_eq!(first.expected_revision.as_deref(), Some("rev-a"));
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(sent.operation_id, first.operation_id);
    assert_eq!(sent.status, OutboxStatus::Sent);
    store
        .save_registered_checkpoint(
            &registration,
            "doc",
            &opened.edit_session_id,
            7,
            b"generation seven",
        )
        .await
        .unwrap();
    let next = queue(&mut store, &registration, "doc").await.unwrap();
    assert_eq!(next.operation_seq, 2);
    assert_eq!(next.generation, 7);
    assert_eq!(next.expected_revision, None);
    store
        .apply_outbox_commit(&registration, &committed(&first, "rev-b"))
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 7);
    assert_eq!(draft.bytes, b"generation seven");
    assert_eq!(draft.base_bytes, b"generation five");
    assert_eq!(draft.base_revision, "rev-b");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 7);
    assert_eq!(draft.bytes, b"generation seven");
    assert_eq!(draft.base_bytes, b"generation five");
    let entries = entries(&mut store, &registration).await;
    assert_eq!(entries[0].status, OutboxStatus::Committed);
    assert_eq!(entries[1].status, OutboxStatus::Queued);
    assert_eq!(entries[1].expected_revision.as_deref(), Some("rev-b"));
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(sent.operation_id, next.operation_id);
    assert_eq!(sent.bytes, b"generation seven");
    assert_eq!(sent.expected_revision.as_deref(), Some("rev-b"));
    store
        .apply_outbox_commit(&registration, &committed(&sent, "rev-c"))
        .await
        .unwrap();
    store
        .apply_outbox_commit(&registration, &committed(&first, "rev-b"))
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.base_bytes, b"generation seven");
    assert_eq!(draft.base_revision, "rev-c");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 2);
    store.close().await.unwrap();
}

#[tokio::test]
async fn lost_response_retries_exact_operation_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 2, b"later")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let retry = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(retry, sent);
    assert_eq!(retry.bytes, b"sent");
    assert_eq!(retry.expected_revision.as_deref(), Some("rev-a"));
    assert_eq!(entries(&mut store, &registration).await[1].bytes, b"later");
    store.close().await.unwrap();
}

#[tokio::test]
async fn reregistration_freezes_sent_and_queued_operations_and_rejects_late_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut replacement = registration.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 2, b"queued")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    store
        .close_registered_document(&registration, "doc", &opened.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&replacement).await.unwrap();
    assert_eq!(
        store.prepare_outbox(&registration).await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(
        store
            .apply_outbox_commit(&registration, &committed(&sent, "rev-b"))
            .await,
        Err(StoreError::StaleRegistration)
    );
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let entries = entries(&mut store, &registration).await;
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|op| op.frozen));
    assert_eq!(entries[0].status, OutboxStatus::Sent);
    assert_eq!(entries[1].status, OutboxStatus::Queued);
    assert_eq!(entries[0].bytes, b"sent");
    assert_eq!(entries[1].bytes, b"queued");
    assert!(
        store
            .list_outbox(&replacement, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"queued");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "rev-a");
    store.close().await.unwrap();
}

#[tokio::test]
async fn failed_registration_switch_keeps_previous_binding_and_outbox_active() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let original = binding();
    let replacement = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&original).await.unwrap();
    let opened = store
        .open_registered_document(&original, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&original, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    queue(&mut store, &original, "doc").await.unwrap();
    let sent = store.prepare_outbox(&original).await.unwrap().unwrap();
    store
        .close_registered_document(&original, "doc", &opened.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_switch BEFORE INSERT ON client_registrations BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.activate_registration(&replacement).await,
        Err(StoreError::Storage)
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        store.prepare_outbox(&original).await.unwrap().unwrap(),
        sent
    );
    assert!(!store.list_outbox(&original, 0, 10).await.unwrap().entries[0].frozen);
    store.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_receipt_cannot_change_draft_base_or_watermark() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    let mut wrong_hash = committed(&sent, "rev-b");
    wrong_hash.content_hash = format!("sha256:{}", "0".repeat(64));
    let mut wrong_seq = committed(&sent, "rev-b");
    wrong_seq.operation_seq += 1;
    let mut wrong_document = committed(&sent, "rev-b");
    wrong_document.document_id = "other".into();
    let mut wrong_epoch = committed(&sent, "rev-b");
    wrong_epoch.workspace_epoch = Uuid::new_v4().to_string();
    for wrong in [wrong_hash, wrong_seq, wrong_document, wrong_epoch] {
        assert_eq!(
            store.apply_outbox_commit(&registration, &wrong).await,
            Err(StoreError::InvalidReceipt)
        );
    }
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "rev-a");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    assert_eq!(
        store.prepare_outbox(&registration).await.unwrap().unwrap(),
        sent
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn unsent_edits_coalesce_but_sent_payload_is_immutable() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"first")
        .await
        .unwrap();
    let first = queue(&mut store, &registration, "doc").await.unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 2, b"second")
        .await
        .unwrap();
    let coalesced = queue(&mut store, &registration, "doc").await.unwrap();
    assert_eq!(coalesced.operation_id, first.operation_id);
    assert_eq!(coalesced.operation_seq, first.operation_seq);
    assert_eq!(coalesced.bytes, b"second");
    assert_eq!(coalesced.generation, 2);
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 3, b"third")
        .await
        .unwrap();
    let next = queue(&mut store, &registration, "doc").await.unwrap();
    assert_ne!(next.operation_id, sent.operation_id);
    assert_eq!(next.operation_seq, sent.operation_seq + 1);
    assert_eq!(
        store.prepare_outbox(&registration).await.unwrap().unwrap(),
        sent
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn prepare_matching_preserves_newer_queued_payload_and_detects_corruption() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"first")
        .await
        .unwrap();
    let first = queue(&mut store, &registration, "doc").await.unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 2, b"second")
        .await
        .unwrap();
    let updated = queue(&mut store, &registration, "doc").await.unwrap();
    assert_eq!(
        store
            .prepare_outbox_matching(&registration, &first)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.next_outbox(&registration).await.unwrap(),
        Some(updated.clone())
    );
    let mut injection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
    sqlx::query("UPDATE client_outbox SET payload_hash = 'sha256:corrupt' WHERE operation_id = ?")
        .bind(&updated.operation_id)
        .execute(&mut injection)
        .await
        .unwrap();
    assert_eq!(
        store.prepare_outbox_matching(&registration, &updated).await,
        Err(StoreError::UnsupportedFormat)
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM client_outbox WHERE operation_id = ?")
            .bind(&updated.operation_id)
            .fetch_one(&mut injection)
            .await
            .unwrap();
    assert_eq!(status, "queued");
    sqlx::query("UPDATE client_outbox SET payload_hash = ? WHERE operation_id = ?")
        .bind(&updated.payload_hash)
        .bind(&updated.operation_id)
        .execute(&mut injection)
        .await
        .unwrap();
    injection.close().await.unwrap();
    let sent = store
        .prepare_outbox_matching(&registration, &updated)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent.status, OutboxStatus::Sent);
    assert_eq!(sent.bytes, b"second");
}

#[tokio::test]
async fn failed_ack_transaction_preserves_draft_base_payload_and_watermark() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_base_update BEFORE UPDATE ON draft_bases BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .apply_outbox_commit(&registration, &committed(&sent, "rev-b"))
            .await,
        Err(StoreError::Storage)
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "rev-a");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    assert_eq!(
        store.prepare_outbox(&registration).await.unwrap().unwrap(),
        sent
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn precondition_failure_preserves_the_draft_and_blocks_its_dependent_operation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 2, b"later")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let conflict = OutboxConflict {
        operation_id: sent.operation_id.clone(),
        operation_seq: sent.operation_seq,
        workspace_epoch: registration.workspace_epoch.clone(),
        current_revision: "remote-rev".into(),
        current_etag: "\"remote-rev\"".into(),
    };
    store
        .apply_outbox_conflict(&registration, &conflict)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert_eq!(
        store.prepare_outbox(&registration).await,
        Err(StoreError::OutboxBlocked)
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "rev-a");
    let entries = entries(&mut store, &registration).await;
    assert_eq!(entries[0].status, OutboxStatus::Conflict);
    assert_eq!(entries[0].conflict_revision.as_deref(), Some("remote-rev"));
    assert_eq!(entries[1].bytes, b"later");
    assert_eq!(entries[1].status, OutboxStatus::Queued);
    store.close().await.unwrap();
}

async fn write_version_two_store(path: &std::path::Path, collision: bool) {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE draft_checkpoints (document_id TEXT PRIMARY KEY NOT NULL, generation INTEGER NOT NULL, format_version INTEGER NOT NULL, bytes BLOB NOT NULL); CREATE TABLE draft_bases (document_id TEXT PRIMARY KEY NOT NULL REFERENCES draft_checkpoints(document_id), bytes BLOB NOT NULL, revision TEXT NOT NULL); INSERT INTO draft_checkpoints VALUES ('legacy', 5, 1, X'6472616674'); INSERT INTO draft_bases VALUES ('legacy', X'62617365', 'rev-a'); CREATE TABLE client_registrations (id INTEGER PRIMARY KEY, server_origin TEXT NOT NULL, workspace_id TEXT NOT NULL, workspace_epoch TEXT NOT NULL, device_id TEXT NOT NULL, device_stream_id TEXT NOT NULL, status TEXT NOT NULL); CREATE UNIQUE INDEX one_active_registration ON client_registrations(status) WHERE status = 'active'; CREATE TABLE registered_documents (registration_id INTEGER NOT NULL REFERENCES client_registrations(id), document_id TEXT NOT NULL, storage_id TEXT NOT NULL UNIQUE, PRIMARY KEY (registration_id, document_id)); PRAGMA application_id = 1314148952; PRAGMA user_version = 2;").execute(&mut db).await.unwrap();
    if collision {
        sqlx::raw_sql("CREATE TABLE client_outbox (sentinel BLOB); INSERT INTO client_outbox VALUES (X'010203');").execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn version_two_migration_preserves_legacy_drafts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    write_version_two_store(&path, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store.read_checkpoint("legacy").await.unwrap().unwrap();
    assert_eq!(draft.generation, 5);
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "rev-a");
    store.close().await.unwrap();
}

#[tokio::test]
async fn failed_version_two_migration_preserves_legacy_database_and_format() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    write_version_two_store(&path, true).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let sentinel: Vec<u8> = sqlx::query_scalar("SELECT sentinel FROM client_outbox")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM draft_checkpoints WHERE document_id = 'legacy'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    let partial: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'outbox_streams'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(version, 2);
    assert_eq!(sentinel, [1, 2, 3]);
    assert_eq!(original, b"draft");
    assert_eq!(partial, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn failed_version_one_to_three_migration_rolls_back_every_schema_step() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    write_version_two_store(&path, true).await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("DROP TABLE registered_documents; DROP TABLE client_registrations; PRAGMA user_version = 1;")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let registrations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'client_registrations'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM draft_checkpoints WHERE document_id = 'legacy'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(version, 1);
    assert_eq!(registrations, 0);
    assert_eq!(original, b"draft");
    db.close().await.unwrap();
}

async fn write_version_three_store(
    path: &std::path::Path,
    binding: &RegistrationBinding,
    operation_id: &str,
    collision: bool,
) {
    use sha2::{Digest, Sha256};
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    write_version_two_store(path, false).await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE outbox_streams (registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id), next_seq INTEGER NOT NULL, ack_through INTEGER NOT NULL); CREATE TABLE client_outbox (operation_id TEXT PRIMARY KEY NOT NULL, registration_id INTEGER NOT NULL REFERENCES client_registrations(id), operation_seq INTEGER NOT NULL, document_id TEXT NOT NULL, storage_id TEXT NOT NULL REFERENCES registered_documents(storage_id), generation INTEGER NOT NULL, expected_revision TEXT, dependency TEXT REFERENCES client_outbox(operation_id), bytes BLOB NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')), committed_revision TEXT, conflict_revision TEXT, conflict_etag TEXT, UNIQUE(registration_id, operation_seq)); PRAGMA user_version = 3;").execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO client_registrations (id, server_origin, workspace_id, workspace_epoch, device_id, device_stream_id, status) VALUES (1, ?, ?, ?, ?, ?, 'active')")
        .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch).bind(&binding.device_id).bind(&binding.device_stream_id).execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO registered_documents VALUES (1, 'doc', 'legacy')")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox_streams VALUES (1, 2, 0)")
        .execute(&mut db)
        .await
        .unwrap();
    let digest: String = Sha256::digest(b"draft")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_hash, status) VALUES (?, 1, 1, 'doc', 'legacy', 5, 'rev-a', X'6472616674', ?, 'sent')")
        .bind(operation_id).bind(format!("sha256:{digest}")).execute(&mut db).await.unwrap();
    if collision {
        sqlx::query("ALTER TABLE client_outbox ADD COLUMN terminal_kind TEXT")
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn version_three_migration_preserves_sent_payload_and_retry_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let operation_id = Uuid::now_v7().to_string();
    write_version_three_store(&path, &registration, &operation_id, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let retry = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(retry.operation_id, operation_id);
    assert_eq!(retry.operation_seq, 1);
    assert_eq!(retry.bytes, b"draft");
    assert_eq!(retry.expected_revision.as_deref(), Some("rev-a"));
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.bytes, b"draft");
    store.close().await.unwrap();
}

#[tokio::test]
async fn failed_version_three_migration_rolls_back_new_columns() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let operation_id = Uuid::now_v7().to_string();
    write_version_three_store(&path, &registration, &operation_id, true).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let new_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('client_outbox') WHERE name = 'abandon_state'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM client_outbox WHERE operation_id = ?")
            .bind(&operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(version, 3);
    assert_eq!(new_column, 0);
    assert_eq!(original, b"draft");
    db.close().await.unwrap();
}

async fn write_version_four_store(
    path: &std::path::Path,
    binding: &RegistrationBinding,
    operation_id: &str,
    collision: bool,
) {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    write_version_three_store(path, binding, operation_id, false).await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE client_outbox ADD COLUMN abandon_state TEXT NOT NULL DEFAULT 'none'; ALTER TABLE client_outbox ADD COLUMN terminal_kind TEXT; PRAGMA user_version = 4;").execute(&mut db).await.unwrap();
    if collision {
        sqlx::query("ALTER TABLE client_outbox ADD COLUMN payload_len INTEGER")
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn version_four_migration_preserves_inflight_payload_and_retry_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let operation_id = Uuid::now_v7().to_string();
    write_version_four_store(&path, &registration, &operation_id, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(sent.operation_id, operation_id);
    assert_eq!(sent.status, OutboxStatus::Sent);
    assert_eq!(sent.bytes, b"draft");
    assert_eq!(
        store
            .confirmed_receipt_through(&registration)
            .await
            .unwrap(),
        0
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn failed_version_four_migration_rolls_back_all_new_columns() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let operation_id = Uuid::now_v7().to_string();
    write_version_four_store(&path, &registration, &operation_id, true).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let first: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('outbox_streams') WHERE name = 'confirmed_through'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    let second: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('client_outbox') WHERE name = 'payload_retired'",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    let payload: Vec<u8> =
        sqlx::query_scalar("SELECT bytes FROM client_outbox WHERE operation_id = ?")
            .bind(&operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(version, 4);
    assert_eq!((first, second), (0, 0));
    assert_eq!(payload, b"draft");
    db.close().await.unwrap();
}

#[tokio::test]
async fn explicit_abandon_of_blocked_work_survives_restart_and_unblocks_the_next_document() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let a = store
        .open_registered_document(&registration, "a", b"a-base", "rev-a")
        .await
        .unwrap();
    let b = store
        .open_registered_document(&registration, "b", b"b-base", "rev-b")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "a", &a.edit_session_id, 1, b"a-sent")
        .await
        .unwrap();
    queue(&mut store, &registration, "a").await.unwrap();
    let first = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .save_registered_checkpoint(&registration, "a", &a.edit_session_id, 2, b"a-later")
        .await
        .unwrap();
    let blocked = queue(&mut store, &registration, "a").await.unwrap();
    store
        .save_registered_checkpoint(&registration, "b", &b.edit_session_id, 1, b"b-pending")
        .await
        .unwrap();
    let next = queue(&mut store, &registration, "b").await.unwrap();
    store
        .apply_outbox_conflict(
            &registration,
            &notryx_client_store::OutboxConflict {
                operation_id: first.operation_id.clone(),
                operation_seq: first.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                current_revision: "remote".into(),
                current_etag: "\"remote\"".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.prepare_outbox(&registration).await,
        Err(StoreError::OutboxBlocked)
    );
    store
        .request_outbox_abandon(&registration, &blocked.operation_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let abandonment = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(abandonment.operation_id, blocked.operation_id);
    assert_eq!(abandonment.operation_seq, 2);
    assert_eq!(abandonment.status, OutboxStatus::Abandoning);
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.prepare_outbox(&registration).await.unwrap().unwrap(),
        abandonment
    );
    store
        .apply_outbox_abandoned(
            &registration,
            &notryx_client_store::OutboxAbandoned {
                operation_id: abandonment.operation_id.clone(),
                operation_seq: abandonment.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 2);
    assert_eq!(
        store
            .prepare_outbox(&registration)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        next.operation_id
    );
    let draft = store
        .read_registered_checkpoint(&registration, "a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"a-later");
    assert_eq!(draft.base_bytes, b"a-base");
    assert_eq!(draft.base_revision, "rev-a");
    store.close().await.unwrap();
}

#[tokio::test]
async fn sent_operation_cannot_be_replaced_with_abandon() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(
        store
            .request_outbox_abandon(&registration, &sent.operation_id)
            .await,
        Err(StoreError::InvalidReceipt)
    );
    assert_eq!(
        store.prepare_outbox(&registration).await.unwrap().unwrap(),
        sent
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn reregistration_preserves_whether_abandon_was_sent() {
    use notryx_client_store::OutboxAbandonState;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let first = binding();
    let mut second = first.clone();
    second.device_id = Uuid::new_v4().to_string();
    second.device_stream_id = Uuid::new_v4().to_string();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&first).await.unwrap();
    for id in ["a", "b"] {
        let opened = store
            .open_registered_document(&first, id, b"base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&first, id, &opened.edit_session_id, 1, id.as_bytes())
            .await
            .unwrap();
        let queued = queue(&mut store, &first, id).await.unwrap();
        store
            .request_outbox_abandon(&first, &queued.operation_id)
            .await
            .unwrap();
        store
            .close_registered_document(&first, id, &opened.edit_session_id)
            .await
            .unwrap();
    }
    let sent = store.prepare_outbox(&first).await.unwrap().unwrap();
    assert_eq!(sent.abandon_state, OutboxAbandonState::Sent);
    store.activate_registration(&second).await.unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let page = store.list_outbox(&first, 0, 100).await.unwrap();
    assert_eq!(page.entries.len(), 2);
    assert!(page.entries.iter().all(|item| item.frozen));
    assert_eq!(page.entries[0].abandon_state, OutboxAbandonState::Sent);
    assert_eq!(page.entries[1].abandon_state, OutboxAbandonState::Requested);
    assert_eq!(
        store
            .apply_outbox_abandoned(
                &first,
                &notryx_client_store::OutboxAbandoned {
                    operation_id: sent.operation_id,
                    operation_seq: sent.operation_seq,
                    workspace_epoch: first.workspace_epoch.clone(),
                }
            )
            .await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&first, "a")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"a"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn outbox_listing_is_bounded_and_payload_is_loaded_only_for_one_operation() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    for id in ["a", "b", "c"] {
        let opened = store
            .open_registered_document(&registration, id, b"base", "rev")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &registration,
                id,
                &opened.edit_session_id,
                1,
                id.as_bytes(),
            )
            .await
            .unwrap();
        queue(&mut store, &registration, id).await.unwrap();
    }
    assert_eq!(
        store.list_outbox(&registration, 0, 0).await,
        Err(StoreError::InvalidRequest)
    );
    assert_eq!(
        store.list_outbox(&registration, 0, 101).await,
        Err(StoreError::InvalidRequest)
    );
    let first = store.list_outbox(&registration, 0, 2).await.unwrap();
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.entries[0].document_id, "a");
    assert_eq!(first.entries[1].document_id, "b");
    assert_eq!(first.next_after_seq, Some(2));
    let last = store
        .list_outbox(&registration, first.next_after_seq.unwrap(), 2)
        .await
        .unwrap();
    assert_eq!(last.entries.len(), 1);
    assert_eq!(last.entries[0].document_id, "c");
    assert_eq!(last.next_after_seq, None);
    let payload = store
        .read_outbox_operation(&registration, &last.entries[0].operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload.bytes, b"c");
    store.close().await.unwrap();
}

#[tokio::test]
async fn metadata_page_identifies_terminal_failure_without_loading_payload() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .apply_outbox_failure(
            &registration,
            &notryx_client_store::OutboxFailure {
                operation_id: sent.operation_id,
                operation_seq: sent.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                kind: notryx_client_store::OutboxFailureKind::RecoveryConflict,
            },
        )
        .await
        .unwrap();
    let page = store.list_outbox(&registration, 0, 10).await.unwrap();
    assert_eq!(
        page.entries[0].failure_kind,
        Some(notryx_client_store::OutboxFailureKind::RecoveryConflict)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn terminal_deletion_and_recovery_conflict_preserve_draft_and_block_dependents() {
    for failure in [
        notryx_client_store::OutboxFailureKind::NodeDeleted,
        notryx_client_store::OutboxFailureKind::RecoveryConflict,
        notryx_client_store::OutboxFailureKind::NotFound,
        notryx_client_store::OutboxFailureKind::NotApplied,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drafts.db");
        let registration = binding();
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&registration).await.unwrap();
        let opened = store
            .open_registered_document(&registration, "doc", b"base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"sent")
            .await
            .unwrap();
        queue(&mut store, &registration, "doc").await.unwrap();
        let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
        store
            .save_registered_checkpoint(
                &registration,
                "doc",
                &opened.edit_session_id,
                2,
                b"new draft",
            )
            .await
            .unwrap();
        queue(&mut store, &registration, "doc").await.unwrap();
        let receipt = notryx_client_store::OutboxFailure {
            operation_id: sent.operation_id.clone(),
            operation_seq: sent.operation_seq,
            workspace_epoch: registration.workspace_epoch.clone(),
            kind: failure,
        };
        store
            .apply_outbox_failure(&registration, &receipt)
            .await
            .unwrap();
        assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
        assert_eq!(
            store.prepare_outbox(&registration).await,
            Err(StoreError::OutboxBlocked)
        );
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        let entries = entries(&mut store, &registration).await;
        assert_eq!(entries[0].failure_kind, Some(failure));
        assert_eq!(entries[0].bytes, b"sent");
        let draft = store
            .read_registered_checkpoint(&registration, "doc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"new draft");
        assert_eq!(draft.base_bytes, b"base");
        assert_eq!(draft.base_revision, "rev-a");
        store
            .apply_outbox_failure(&registration, &receipt)
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn reregistration_and_restored_epoch_block_automatic_queue_for_unreconciled_document() {
    for restored_epoch in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drafts.db");
        let original = binding();
        let mut replacement = original.clone();
        replacement.device_id = Uuid::new_v4().to_string();
        replacement.device_stream_id = Uuid::new_v4().to_string();
        if restored_epoch {
            replacement.workspace_epoch = Uuid::new_v4().to_string();
        }
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&original).await.unwrap();
        let old = store
            .open_registered_document(&original, "doc", b"old base", "old-rev")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&original, "doc", &old.edit_session_id, 1, b"old sent")
            .await
            .unwrap();
        queue(&mut store, &original, "doc").await.unwrap();
        let sent = store.prepare_outbox(&original).await.unwrap().unwrap();
        store
            .save_registered_checkpoint(&original, "doc", &old.edit_session_id, 2, b"old draft")
            .await
            .unwrap();
        queue(&mut store, &original, "doc").await.unwrap();
        store
            .close_registered_document(&original, "doc", &old.edit_session_id)
            .await
            .unwrap();
        store.activate_registration(&replacement).await.unwrap();
        let new = store
            .open_registered_document(&replacement, "doc", b"current base", "current-rev")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&replacement, "doc", &new.edit_session_id, 1, b"new draft")
            .await
            .unwrap();
        assert_eq!(
            queue(&mut store, &replacement, "doc").await,
            Err(StoreError::ReconciliationRequired)
        );
        assert_eq!(
            store
                .apply_outbox_commit(&original, &committed(&sent, "late-rev"))
                .await,
            Err(StoreError::StaleRegistration)
        );
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert!(
            store
                .list_outbox(&replacement, 0, 10)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&replacement, "doc")
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"new draft"
        );
        let old = store
            .read_registered_checkpoint(&original, "doc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.bytes, b"old draft");
        assert_eq!(old.base_bytes, b"old base");
        assert_eq!(
            store
                .list_outbox(&original, 0, 10)
                .await
                .unwrap()
                .entries
                .len(),
            2
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn old_draft_from_another_workspace_does_not_block_new_workspace_queue() {
    let directory = tempfile::tempdir().unwrap();
    let original = binding();
    let replacement = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&original).await.unwrap();
    let old = store
        .open_registered_document(&original, "doc", b"old base", "old-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&original, "doc", &old.edit_session_id, 1, b"old draft")
        .await
        .unwrap();
    store
        .close_registered_document(&original, "doc", &old.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&replacement).await.unwrap();
    let new = store
        .open_registered_document(&replacement, "doc", b"new base", "new-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&replacement, "doc", &new.edit_session_id, 1, b"new draft")
        .await
        .unwrap();
    assert_eq!(
        queue(&mut store, &replacement, "doc").await.unwrap().bytes,
        b"new draft"
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&original, "doc")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"old draft"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn previously_committed_current_generation_does_not_block_reregistration() {
    let directory = tempfile::tempdir().unwrap();
    let original = binding();
    let mut replacement = original.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&original).await.unwrap();
    let old = store
        .open_registered_document(&original, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&original, "doc", &old.edit_session_id, 1, b"committed")
        .await
        .unwrap();
    queue(&mut store, &original, "doc").await.unwrap();
    let sent = store.prepare_outbox(&original).await.unwrap().unwrap();
    store
        .apply_outbox_commit(&original, &committed(&sent, "rev-b"))
        .await
        .unwrap();
    store
        .close_registered_document(&original, "doc", &old.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&replacement).await.unwrap();
    let new = store
        .open_registered_document(&replacement, "doc", b"committed", "rev-b")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&replacement, "doc", &new.edit_session_id, 1, b"next")
        .await
        .unwrap();
    assert_eq!(
        queue(&mut store, &replacement, "doc").await.unwrap().bytes,
        b"next"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn recovery_edit_after_new_queue_is_checked_again_before_sending() {
    let directory = tempfile::tempdir().unwrap();
    let original = binding();
    let mut replacement = original.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&original).await.unwrap();
    let old = store
        .open_registered_document(&original, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&original, "doc", &old.edit_session_id, 1, b"committed")
        .await
        .unwrap();
    queue(&mut store, &original, "doc").await.unwrap();
    let sent = store.prepare_outbox(&original).await.unwrap().unwrap();
    store
        .apply_outbox_commit(&original, &committed(&sent, "rev-b"))
        .await
        .unwrap();
    store
        .close_registered_document(&original, "doc", &old.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&replacement).await.unwrap();
    let new = store
        .open_registered_document(&replacement, "doc", b"committed", "rev-b")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&replacement, "doc", &new.edit_session_id, 1, b"new queued")
        .await
        .unwrap();
    let queued = queue(&mut store, &replacement, "doc").await.unwrap();
    let recovered = store.open_recovery_draft(&original, "doc").await.unwrap();
    store
        .save_recovery_checkpoint(
            &original,
            "doc",
            &recovered.edit_session_id,
            2,
            b"recovery changed",
        )
        .await
        .unwrap();
    assert_eq!(
        store.prepare_outbox(&replacement).await,
        Err(StoreError::ReconciliationRequired)
    );
    let persisted = store
        .read_outbox_operation(&replacement, &queued.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.status, OutboxStatus::Queued);
    assert_eq!(persisted.bytes, b"new queued");
    store.close().await.unwrap();
}

#[tokio::test]
async fn uncertain_frozen_payload_blocks_same_workspace_even_if_checkpoint_is_missing() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let original = binding();
    let mut replacement = original.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&original).await.unwrap();
    let old = store
        .open_registered_document(&original, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&original, "doc", &old.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    queue(&mut store, &original, "doc").await.unwrap();
    let sent = store.prepare_outbox(&original).await.unwrap().unwrap();
    store
        .close_registered_document(&original, "doc", &old.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&replacement).await.unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("DELETE FROM draft_bases WHERE document_id IN (SELECT storage_id FROM registered_documents WHERE document_id = 'doc')").execute(&mut db).await.unwrap();
    sqlx::query("DELETE FROM draft_checkpoints WHERE document_id IN (SELECT storage_id FROM registered_documents WHERE document_id = 'doc')").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let new = store
        .open_registered_document(&replacement, "doc", b"remote", "rev-b")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&replacement, "doc", &new.edit_session_id, 1, b"new draft")
        .await
        .unwrap();
    assert_eq!(
        queue(&mut store, &replacement, "doc").await,
        Err(StoreError::ReconciliationRequired)
    );
    let old = store
        .read_outbox_operation(&original, &sent.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(old.frozen);
    assert_eq!(old.status, OutboxStatus::Sent);
    assert_eq!(old.bytes, b"sent");
    store.close().await.unwrap();
}

#[tokio::test]
async fn reopen_queues_an_unqueued_checkpoint_and_reuses_the_same_sent_and_committed_operation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let document = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            "doc",
            &document.edit_session_id,
            5,
            b"saved before queue",
        )
        .await
        .unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let document = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    assert_eq!(document.generation, 5);
    let queued = queue(&mut store, &registration, "doc").await.unwrap();
    assert_eq!(queued.status, OutboxStatus::Queued);
    assert_eq!(queued.bytes, b"saved before queue");
    assert_eq!(
        queue(&mut store, &registration, "doc").await.unwrap(),
        queued
    );
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    assert_eq!(queue(&mut store, &registration, "doc").await.unwrap(), sent);
    store
        .apply_outbox_commit(&registration, &committed(&sent, "rev-b"))
        .await
        .unwrap();
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .open_registered_document(&registration, "doc", b"saved before queue", "rev-b")
        .await
        .unwrap();
    let completed = queue(&mut store, &registration, "doc").await.unwrap();
    assert_eq!(completed.status, OutboxStatus::Committed);
    assert_eq!(completed.operation_id, sent.operation_id);
    assert_eq!(completed.operation_seq, sent.operation_seq);
    assert!(store.prepare_outbox(&registration).await.unwrap().is_none());
    assert_eq!(
        store
            .list_outbox(&registration, 0, 10)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn same_generation_with_different_checkpoint_bytes_never_reuses_an_operation() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let document = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &document.edit_session_id, 1, b"sent")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("UPDATE draft_checkpoints SET bytes = X'6368616e676564' WHERE document_id IN (SELECT storage_id FROM registered_documents WHERE document_id = 'doc')").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        queue(&mut store, &registration, "doc").await,
        Err(StoreError::GenerationMismatch)
    );
    assert_eq!(
        store.prepare_outbox(&registration).await.unwrap().unwrap(),
        sent
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn delayed_queue_rejects_a_newer_checkpoint_and_preserves_the_previous_payload() {
    use std::sync::Arc;
    use tokio::sync::{Mutex, oneshot};
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"stable")
        .await
        .unwrap();
    let old = queue(&mut store, &registration, "doc").await.unwrap();
    let shared = Arc::new(Mutex::new(store));
    let (release, wait) = oneshot::channel();
    let task_store = shared.clone();
    let task_binding = registration.clone();
    let delayed = tokio::spawn(async move {
        wait.await.unwrap();
        task_store
            .lock()
            .await
            .queue_checkpoint(&task_binding, "doc", 1)
            .await
    });
    shared
        .lock()
        .await
        .save_registered_checkpoint(
            &registration,
            "doc",
            &opened.edit_session_id,
            2,
            b"ime in progress",
        )
        .await
        .unwrap();
    release.send(()).unwrap();
    assert_eq!(delayed.await.unwrap(), Err(StoreError::GenerationMismatch));
    let mut store = shared.lock().await;
    let item = store
        .read_outbox_operation(&registration, &old.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.generation, 1);
    assert_eq!(item.bytes, b"stable");
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, "doc")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"ime in progress"
    );
}

#[tokio::test]
async fn server_ack_retires_only_superseded_committed_payload_and_preserves_late_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let doc = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &doc.edit_session_id, 1, b"first")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let first = store.prepare_outbox(&registration).await.unwrap().unwrap();
    let first_receipt = committed(&first, "rev-b");
    store
        .apply_outbox_commit(&registration, &first_receipt)
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &doc.edit_session_id, 2, b"second")
        .await
        .unwrap();
    queue(&mut store, &registration, "doc").await.unwrap();
    let second = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .apply_outbox_commit(&registration, &committed(&second, "rev-c"))
        .await
        .unwrap();
    assert_eq!(
        store
            .read_outbox_operation(&registration, &first.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"first"
    );
    store
        .confirm_receipt_ack(&registration, &server_ack(&registration, 2, 2))
        .await
        .unwrap();
    assert_eq!(
        store
            .confirmed_receipt_through(&registration)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, &first.operation_id)
            .await,
        Err(StoreError::PayloadRetired)
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, &second.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"second"
    );
    let page = store.list_outbox(&registration, 0, 10).await.unwrap();
    assert!(page.entries[0].payload_retired);
    assert!(!page.entries[1].payload_retired);
    assert_eq!(page.entries[0].payload_len, b"first".len());
    let mut wrong = first_receipt.clone();
    wrong.content_hash = format!("sha256:{}", "0".repeat(64));
    assert_eq!(
        store.apply_outbox_commit(&registration, &wrong).await,
        Err(StoreError::InvalidReceipt)
    );
    store
        .apply_outbox_commit(&registration, &first_receipt)
        .await
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"second");
    assert_eq!(draft.base_bytes, b"second");
    assert_eq!(draft.base_revision, "rev-c");
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .confirmed_receipt_through(&registration)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, &first.operation_id)
            .await,
        Err(StoreError::PayloadRetired)
    );
    store
        .apply_outbox_commit(&registration, &first_receipt)
        .await
        .unwrap();
    assert_eq!(
        store
            .queue_checkpoint(&registration, "doc", 2)
            .await
            .unwrap()
            .status,
        OutboxStatus::Committed
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn bad_ack_or_storage_failure_never_retires_payload_or_advances_confirmation() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let doc = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    for (generation, bytes, revision) in [
        (1, b"first".as_slice(), "rev-b"),
        (2, b"second".as_slice(), "rev-c"),
    ] {
        store
            .save_registered_checkpoint(
                &registration,
                "doc",
                &doc.edit_session_id,
                generation,
                bytes,
            )
            .await
            .unwrap();
        queue(&mut store, &registration, "doc").await.unwrap();
        let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
        store
            .apply_outbox_commit(&registration, &committed(&sent, revision))
            .await
            .unwrap();
    }
    let first = store
        .list_outbox(&registration, 0, 10)
        .await
        .unwrap()
        .entries[0]
        .operation_id
        .clone();
    let mut wrong = server_ack(&registration, 2, 2);
    wrong.device_stream_id = Uuid::new_v4().to_string();
    assert_eq!(
        store.confirm_receipt_ack(&registration, &wrong).await,
        Err(StoreError::InvalidReceipt)
    );
    assert_eq!(
        store
            .confirm_receipt_ack(&registration, &server_ack(&registration, 3, 3))
            .await,
        Err(StoreError::InvalidReceipt)
    );
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_retire BEFORE UPDATE OF bytes ON client_outbox BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .confirm_receipt_ack(&registration, &server_ack(&registration, 2, 2))
            .await,
        Err(StoreError::Storage)
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .confirmed_receipt_through(&registration)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, &first)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"first"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn stale_registration_ack_cannot_retire_frozen_payload() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let original = binding();
    let mut replacement = original.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&original).await.unwrap();
    let doc = store
        .open_registered_document(&original, "doc", b"base", "rev-a")
        .await
        .unwrap();
    let mut first = None;
    for (generation, bytes, revision) in [
        (1, b"first".as_slice(), "rev-b"),
        (2, b"second".as_slice(), "rev-c"),
    ] {
        store
            .save_registered_checkpoint(&original, "doc", &doc.edit_session_id, generation, bytes)
            .await
            .unwrap();
        queue(&mut store, &original, "doc").await.unwrap();
        let sent = store.prepare_outbox(&original).await.unwrap().unwrap();
        if generation == 1 {
            first = Some(sent.operation_id.clone());
        }
        store
            .apply_outbox_commit(&original, &committed(&sent, revision))
            .await
            .unwrap();
    }
    store
        .close_registered_document(&original, "doc", &doc.edit_session_id)
        .await
        .unwrap();
    store.activate_registration(&replacement).await.unwrap();
    assert_eq!(
        store
            .confirm_receipt_ack(&original, &server_ack(&original, 2, 2))
            .await,
        Err(StoreError::StaleRegistration)
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(store.confirmed_receipt_through(&original).await.unwrap(), 0);
    assert_eq!(
        store
            .read_outbox_operation(&original, &first.unwrap())
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"first"
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&original, "doc")
            .await
            .unwrap()
            .unwrap()
            .base_bytes,
        b"second"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn acknowledgement_preserves_pending_conflict_and_latest_base_payload() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let doc = store
        .open_registered_document(&registration, "doc", b"base", "rev-a")
        .await
        .unwrap();
    for (generation, bytes, revision) in [
        (1, b"first".as_slice(), "rev-b"),
        (2, b"second".as_slice(), "rev-c"),
    ] {
        store
            .save_registered_checkpoint(
                &registration,
                "doc",
                &doc.edit_session_id,
                generation,
                bytes,
            )
            .await
            .unwrap();
        queue(&mut store, &registration, "doc").await.unwrap();
        let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
        store
            .apply_outbox_commit(&registration, &committed(&sent, revision))
            .await
            .unwrap();
    }
    store
        .save_registered_checkpoint(&registration, "doc", &doc.edit_session_id, 3, b"pending")
        .await
        .unwrap();
    let pending = queue(&mut store, &registration, "doc").await.unwrap();
    store
        .confirm_receipt_ack(&registration, &server_ack(&registration, 2, 2))
        .await
        .unwrap();
    assert_eq!(
        store
            .read_outbox_operation(&registration, &pending.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"pending"
    );
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"pending");
    assert_eq!(draft.base_bytes, b"second");
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .apply_outbox_failure(
            &registration,
            &notryx_client_store::OutboxFailure {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                kind: notryx_client_store::OutboxFailureKind::RecoveryConflict,
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&registration, &server_ack(&registration, 3, 3))
        .await
        .unwrap();
    let failed = store
        .read_outbox_operation(&registration, &sent.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, OutboxStatus::Failed);
    assert_eq!(failed.bytes, b"pending");
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"pending");
    assert_eq!(draft.base_bytes, b"second");
    store.close().await.unwrap();
}

#[tokio::test]
async fn corrupt_newer_committed_payload_never_retires_older_payload_or_ack_watermark() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    for corruption in ["payload", "base", "checkpoint", "revision"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drafts.db");
        let registration = binding();
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&registration).await.unwrap();
        let doc = store
            .open_registered_document(&registration, "doc", b"base", "rev-a")
            .await
            .unwrap();
        let mut first = String::new();
        let mut second = String::new();
        for (generation, bytes, revision) in [
            (1, b"first".as_slice(), "rev-b"),
            (2, b"second".as_slice(), "rev-c"),
        ] {
            store
                .save_registered_checkpoint(
                    &registration,
                    "doc",
                    &doc.edit_session_id,
                    generation,
                    bytes,
                )
                .await
                .unwrap();
            queue(&mut store, &registration, "doc").await.unwrap();
            let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
            if generation == 1 {
                first = sent.operation_id.clone();
            } else {
                second = sent.operation_id.clone();
            }
            store
                .apply_outbox_commit(&registration, &committed(&sent, revision))
                .await
                .unwrap();
        }
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        match corruption {
            "payload" => {
                sqlx::query(
                    "UPDATE client_outbox SET bytes = X'62726f6b656e' WHERE operation_id = ?",
                )
                .bind(&second)
                .execute(&mut db)
                .await
                .unwrap();
            }
            "base" => {
                sqlx::query("UPDATE draft_bases SET bytes = X'62726f6b656e'")
                    .execute(&mut db)
                    .await
                    .unwrap();
            }
            "checkpoint" => {
                sqlx::query("UPDATE draft_checkpoints SET bytes = X'62726f6b656e'")
                    .execute(&mut db)
                    .await
                    .unwrap();
            }
            "revision" => {
                sqlx::query(
                    "UPDATE client_outbox SET committed_revision = ? WHERE operation_id = ?",
                )
                .bind("invalid\"revision")
                .bind(&second)
                .execute(&mut db)
                .await
                .unwrap();
                sqlx::query("UPDATE draft_bases SET revision = ?")
                    .bind("invalid\"revision")
                    .execute(&mut db)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .confirm_receipt_ack(&registration, &server_ack(&registration, 2, 2))
                .await,
            Err(StoreError::UnsupportedFormat)
        );
        assert_eq!(
            store
                .confirmed_receipt_through(&registration)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .read_outbox_operation(&registration, &first)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"first"
        );
        let draft = store
            .read_registered_checkpoint(&registration, "doc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            draft.bytes,
            if corruption == "checkpoint" {
                b"broken"
            } else {
                b"second"
            }
        );
        assert_eq!(
            draft.base_bytes,
            if corruption == "base" {
                b"broken"
            } else {
                b"second"
            }
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn corrupt_old_receipt_revision_never_retires_payload_or_ack_watermark() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    for invalid in [None, Some("invalid\"revision")] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drafts.db");
        let registration = binding();
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&registration).await.unwrap();
        let doc = store
            .open_registered_document(&registration, "doc", b"base", "rev-a")
            .await
            .unwrap();
        let mut first = String::new();
        for (generation, bytes, revision) in [
            (1, b"first".as_slice(), "rev-b"),
            (2, b"second".as_slice(), "rev-c"),
        ] {
            store
                .save_registered_checkpoint(
                    &registration,
                    "doc",
                    &doc.edit_session_id,
                    generation,
                    bytes,
                )
                .await
                .unwrap();
            queue(&mut store, &registration, "doc").await.unwrap();
            let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
            if generation == 1 {
                first = sent.operation_id.clone();
            }
            store
                .apply_outbox_commit(&registration, &committed(&sent, revision))
                .await
                .unwrap();
        }
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::query("UPDATE client_outbox SET committed_revision = ? WHERE operation_id = ?")
            .bind(invalid)
            .bind(&first)
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .confirm_receipt_ack(&registration, &server_ack(&registration, 2, 2))
                .await,
            Err(StoreError::UnsupportedFormat)
        );
        assert_eq!(
            store
                .confirmed_receipt_through(&registration)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .read_outbox_operation(&registration, &first)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"first"
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn document_status_does_not_depend_on_scanning_an_entire_long_stream() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    for id in ["doc", "other"] {
        let opened = store
            .open_registered_document(&registration, id, b"base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&registration, id, &opened.edit_session_id, 1, b"draft")
            .await
            .unwrap();
        store
            .close_registered_document(&registration, id, &opened.edit_session_id)
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO outbox_streams (registration_id, next_seq, ack_through) VALUES (1, 10004, 0)",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::raw_sql("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 10001) INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_hash, status) SELECT printf('synthetic-%d', x), 1, x, 'other', (SELECT storage_id FROM registered_documents WHERE document_id = 'other'), 1, 'rev-a', X'64', 'synthetic', 'queued' FROM n;").execute(&mut *tx).await.unwrap();
    sqlx::raw_sql("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_hash, status, conflict_revision, conflict_etag) VALUES ('conflict', 1, 10002, 'doc', (SELECT storage_id FROM registered_documents WHERE document_id = 'doc'), 1, 'rev-a', X'64', 'synthetic', 'conflict', 'remote', '\"remote\"'); INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_hash, status) VALUES ('latest', 1, 10003, 'doc', (SELECT storage_id FROM registered_documents WHERE document_id = 'doc'), 2, 'rev-a', X'64', 'synthetic', 'queued');").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let status = store
        .document_outbox_state(&registration, "doc")
        .await
        .unwrap();
    assert_eq!(status.latest.unwrap().operation_seq, 10003);
    assert!(status.has_conflict);
    assert!(!status.has_failure);
    store.close().await.unwrap();
}

#[tokio::test]
async fn dirty_history_does_not_starve_cleanup_for_a_clean_document() {
    use sha2::{Digest, Sha256};
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    for (id, generation, bytes) in [
        ("dirty", 34, b"uncommitted".as_slice()),
        ("clean", 2, b"two".as_slice()),
    ] {
        let opened = store
            .open_registered_document(&registration, id, b"base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &registration,
                id,
                &opened.edit_session_id,
                generation,
                bytes,
            )
            .await
            .unwrap();
        store
            .close_registered_document(&registration, id, &opened.edit_session_id)
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    sqlx::query("UPDATE draft_bases SET bytes = X'74776f', revision = 'rev-c' WHERE document_id = (SELECT storage_id FROM registered_documents WHERE document_id = 'clean')").execute(&mut *tx).await.unwrap();
    sqlx::query(
        "INSERT INTO outbox_streams (registration_id, next_seq, ack_through) VALUES (1, 36, 35)",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let digest = |bytes: &[u8]| {
        let value: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("sha256:{value}")
    };
    for seq in 1..=33_i64 {
        sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_len, payload_hash, status, committed_revision) VALUES (?, 1, ?, 'dirty', (SELECT storage_id FROM registered_documents WHERE document_id = 'dirty'), ?, 'rev-a', X'78', 1, ?, 'committed', 'rev-b')")
            .bind(format!("dirty-{seq}")).bind(seq).bind(seq).bind(digest(b"x")).execute(&mut *tx).await.unwrap();
    }
    for (seq, generation, bytes, revision) in [
        (34_i64, 1_i64, b"one".as_slice(), "rev-b"),
        (35, 2, b"two".as_slice(), "rev-c"),
    ] {
        sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_len, payload_hash, status, committed_revision) VALUES (?, 1, ?, 'clean', (SELECT storage_id FROM registered_documents WHERE document_id = 'clean'), ?, 'rev-a', ?, 3, ?, 'committed', ?)")
            .bind(format!("clean-{seq}")).bind(seq).bind(generation).bind(bytes).bind(digest(bytes)).bind(revision).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .confirm_receipt_ack(&registration, &server_ack(&registration, 35, 35))
        .await
        .unwrap();
    assert_eq!(
        store.read_outbox_operation(&registration, "clean-34").await,
        Err(StoreError::PayloadRetired)
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, "dirty-1")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"x"
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, "dirty")
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"uncommitted"
    );
    store.close().await.unwrap();
}
