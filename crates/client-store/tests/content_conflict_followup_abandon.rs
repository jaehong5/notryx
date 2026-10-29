use notryx_client_store::{
    ClientStore, ContentConflictChoice, ContentConflictIntent, ContentConflictIntentRequest,
    OutboxConflict, RegistrationBinding, StoreError,
};
use notryx_protocol::{InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const DOC: &str = "019a1000-0000-7000-8000-000000000002";
const OTHER: &str = "019a1000-0000-7000-8000-000000000003";
const BASE: &[u8] = b"\xef\xbb\xbforiginal\r\n";
const SENT: &[u8] = b"\xef\xbb\xbfsent\n";
const LOCAL: &[u8] = b"\xef\xbb\xbfnewer local\n";
const REMOTE: &[u8] = b"\xef\xbb\xbfpeer version\r\n";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn info(binding: &RegistrationBinding) -> ServerInfo {
    serde_json::from_value(json!({
        "protocol":"mdw-sync/1.2", "workspace_id":binding.workspace_id,
        "workspace_epoch":binding.workspace_epoch, "root_id":ROOT,
        "head_seq":"2", "journal_floor_seq":"0", "ready":true,
        "max_document_bytes":5242880, "max_attachment_bytes":26214400
    }))
    .unwrap()
}

fn remote_nodes() -> (NodeState, NodeState) {
    let root = serde_json::from_value(json!({
        "node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,
        "content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"",
        "node_etag":"\"root-node\"","content_hash":null,"size":null,
        "children_generation":"1","last_change_seq":"2"
    }))
    .unwrap();
    let file = serde_json::from_value(json!({
        "node_id":DOC,"parent_id":ROOT,"name":"document.md","kind":"file","deleted":false,
        "content_rev":"current-rev","content_etag":"\"current-rev\"","meta_rev":"file-meta",
        "meta_etag":"\"file-meta\"","node_etag":"\"file-node\"",
        "content_hash":digest(REMOTE),"size":REMOTE.len().to_string(),
        "children_generation":"0","last_change_seq":"2"
    }))
    .unwrap();
    (root, file)
}

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

struct Rig {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    binding: RegistrationBinding,
    session: String,
    operation: String,
    followup: String,
    request: ContentConflictIntentRequest,
}

async fn rig() -> (Rig, ClientStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let binding = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    let opened = store
        .open_registered_document(&binding, DOC, BASE, "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, DOC, &opened.edit_session_id, 1, SENT)
        .await
        .unwrap();
    let sent = store.queue_checkpoint(&binding, DOC, 1).await.unwrap();
    store.prepare_outbox(&binding).await.unwrap().unwrap();
    store
        .save_registered_checkpoint(&binding, DOC, &opened.edit_session_id, 2, LOCAL)
        .await
        .unwrap();
    let followup = store.queue_checkpoint(&binding, DOC, 2).await.unwrap();
    store
        .apply_outbox_conflict(
            &binding,
            &OutboxConflict {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: binding.workspace_epoch.clone(),
                current_revision: "peer-rev".into(),
                current_etag: "\"peer-rev\"".into(),
            },
        )
        .await
        .unwrap();
    let (root, remote) = remote_nodes();
    let bootstrap = store
        .begin_bootstrap(&binding, &info(&binding))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &binding,
            &bootstrap,
            &InventoryPage {
                workspace_epoch: info(&binding).workspace_epoch,
                nodes: vec![root, remote.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&binding, &bootstrap, &info(&binding))
        .await
        .unwrap();
    store
        .bootstrap_publish(&binding, &bootstrap, &info(&binding))
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&binding, &remote, REMOTE)
            .await
            .unwrap()
    );
    let request = ContentConflictIntentRequest {
        generation: 2,
        remote_revision: "current-rev".into(),
        remote_hash: digest(REMOTE),
        remote_bytes: REMOTE.to_vec(),
        choice: ContentConflictChoice::SaveMerged {
            bytes: b"\xef\xbb\xbfmerged\n".to_vec(),
        },
    };
    (
        Rig {
            _dir: dir,
            path,
            binding,
            session: opened.edit_session_id,
            operation: sent.operation_id,
            followup: followup.operation_id,
            request,
        },
        store,
    )
}

async fn record(
    store: &mut ClientStore,
    rig: &Rig,
    request: &ContentConflictIntentRequest,
) -> Result<ContentConflictIntent, StoreError> {
    store
        .record_content_conflict_intent(&rig.binding, DOC, &rig.operation, request)
        .await
}

async fn assert_originals(store: &mut ClientStore, rig: &Rig) {
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, "base-rev");
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.generation, 2);
    let failed = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.bytes, SENT);
    assert_eq!(failed.expected_revision.as_deref(), Some("base-rev"));
    assert_eq!(failed.conflict_revision.as_deref(), Some("peer-rev"));
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    let later = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(later.bytes, LOCAL);
    assert_eq!(later.operation_seq, failed.operation_seq + 1);
    assert_eq!(later.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
    assert!(store.prepare_outbox(&rig.binding).await.is_err());
}

async fn operation_record(
    path: &Path,
    operation_id: &str,
) -> (String, i64, String, String, Vec<u8>, String, Option<String>) {
    let mut db = connect(path).await;
    let row = sqlx::query_as("SELECT operation_kind, operation_seq, status, abandon_state, bytes, payload_hash, dependency FROM client_outbox WHERE operation_id=?")
        .bind(operation_id).fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();
    row
}

async fn confirm_conflict(store: &mut ClientStore, rig: &Rig) {
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({
        "workspace_epoch":rig.binding.workspace_epoch,
        "device_stream_id":rig.binding.device_stream_id,
        "last_seq":"1", "retired_through":"1"
    }))
    .unwrap();
    store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
}

async fn mark_merge_followup(
    store: &mut ClientStore,
    rig: &Rig,
    version: u64,
    item: &notryx_client_store::OutboxOperation,
) -> Result<notryx_client_store::OutboxOperation, StoreError> {
    store
        .request_content_conflict_merge_followup(
            &rig.binding,
            &rig.operation,
            version,
            &item.operation_id,
            item.operation_seq,
            item.generation,
            &item.payload_hash,
        )
        .await
}

#[tokio::test]
async fn intent_marks_only_earliest_same_document_followup_and_survives_restart() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirm_conflict(&mut store, &rig).await;
    let other = store
        .open_registered_document(&rig.binding, OTHER, b"other base\n", "other-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &rig.binding,
            OTHER,
            &other.edit_session_id,
            1,
            b"other draft\n",
        )
        .await
        .unwrap();
    let other_operation = store
        .queue_checkpoint(&rig.binding, OTHER, 1)
        .await
        .unwrap();
    let create: notryx_protocol::CreateNodeRequest = serde_json::from_value(json!({
        "node_id":"019a1000-0000-7000-8000-000000000004", "parent_id":ROOT,
        "name":"other-folder", "kind":"folder"
    }))
    .unwrap();
    let structure = store.queue_create(&rig.binding, &create).await.unwrap();
    let other_before = operation_record(&rig.path, &other_operation.operation_id).await;
    let structure_before = operation_record(&rig.path, &structure.operation_id).await;
    let conflict_before = operation_record(&rig.path, &rig.operation).await;
    let before = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.status, notryx_client_store::OutboxStatus::Queued);
    let candidate = store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(candidate.operation_id, before.operation_id);
    let marked = mark_merge_followup(&mut store, &rig, intent.version, &candidate)
        .await
        .unwrap();
    assert_eq!(marked.operation_id, rig.followup);
    assert_eq!(marked.operation_seq, before.operation_seq);
    assert_eq!(marked.bytes, before.bytes);
    assert_eq!(marked.payload_hash, before.payload_hash);
    assert_eq!(
        marked.abandon_state,
        notryx_client_store::OutboxAbandonState::Requested
    );
    assert_eq!(
        operation_record(&rig.path, &other_operation.operation_id).await,
        other_before
    );
    assert_eq!(
        operation_record(&rig.path, &structure.operation_id).await,
        structure_before
    );
    assert_eq!(
        operation_record(&rig.path, &rig.operation).await,
        conflict_before
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
    let repeated = mark_merge_followup(&mut store, &rig, intent.version, &candidate)
        .await
        .unwrap();
    assert_eq!(repeated.operation_id, marked.operation_id);
    assert_eq!(
        repeated.abandon_state,
        notryx_client_store::OutboxAbandonState::Requested
    );
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    store
        .close_registered_document(&rig.binding, OTHER, &other.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    let saved = reopened
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.abandon_state,
        notryx_client_store::OutboxAbandonState::Requested
    );
    assert_eq!(saved.bytes, LOCAL);
    assert_eq!(
        reopened
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_eq!(
        operation_record(&rig.path, &other_operation.operation_id).await,
        other_before
    );
    assert_eq!(
        operation_record(&rig.path, &structure.operation_id).await,
        structure_before
    );
    assert_eq!(
        operation_record(&rig.path, &rig.operation).await,
        conflict_before
    );
    assert_eq!(reopened.receipt_ack_through(&rig.binding).await.unwrap(), 1);
}

#[tokio::test]
async fn missing_intent_and_wrong_binding_or_operation_never_mark_followup() {
    let (rig, mut store) = rig().await;
    let before = operation_record(&rig.path, &rig.followup).await;
    assert!(
        store
            .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
            .await
            .is_err()
    );
    assert!(
        store
            .request_content_conflict_followup_abandon(&binding(), &rig.operation)
            .await
            .is_err()
    );
    assert!(
        store
            .request_content_conflict_followup_abandon(&rig.binding, &Uuid::now_v7().to_string())
            .await
            .is_err()
    );
    assert_eq!(operation_record(&rig.path, &rig.followup).await, before);
    assert_originals(&mut store, &rig).await;
}

#[tokio::test]
async fn sent_followup_is_not_abandoned_without_a_server_result() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirm_conflict(&mut store, &rig).await;
    let candidate = store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap()
        .unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::query(
        "UPDATE client_outbox SET status='sent', expected_revision='base-rev' WHERE operation_id=?",
    )
    .bind(&rig.followup)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let before = operation_record(&rig.path, &rig.followup).await;
    let result = mark_merge_followup(&mut store, &rig, intent.version, &candidate).await;
    assert!(result.is_err());
    assert_eq!(operation_record(&rig.path, &rig.followup).await, before);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
}

#[tokio::test]
async fn abandon_mark_write_failure_rolls_back_without_affecting_other_work() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    confirm_conflict(&mut store, &rig).await;
    let candidate = store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap()
        .unwrap();
    let before = operation_record(&rig.path, &rig.followup).await;
    let mut db = connect(&rig.path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_conflict_followup BEFORE UPDATE ON client_outbox WHEN OLD.operation_seq=2 AND NEW.abandon_state='requested' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        mark_merge_followup(&mut store, &rig, intent.version, &candidate).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(operation_record(&rig.path, &rig.followup).await, before);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_originals(&mut store, &rig).await;
}
