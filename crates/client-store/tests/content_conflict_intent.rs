use notryx_client_store::{
    ClientStore, ContentConflictChoice, ContentConflictIntent, ContentConflictIntentRequest,
    OutboxConflict, RegistrationBinding, StoreError,
};
use notryx_domain::ChangeSeq;
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
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

#[tokio::test]
async fn intent_survives_restart_preserves_all_versions_and_does_not_execute_choice() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    assert_eq!(intent.binding, rig.binding);
    assert_eq!(intent.document_id, DOC);
    assert_eq!(intent.operation_id, rig.operation);
    assert_eq!(intent.generation, 2);
    assert_eq!(intent.base_revision, "base-rev");
    assert_eq!(intent.base_bytes, BASE);
    assert_eq!(intent.local_bytes, LOCAL);
    assert_eq!(intent.remote_revision, "current-rev");
    assert_eq!(intent.remote_hash, digest(REMOTE));
    assert_eq!(intent.remote_bytes, REMOTE);
    assert_eq!(intent.choice, rig.request.choice);
    assert_eq!(
        record(&mut store, &rig, &rig.request).await.unwrap(),
        intent
    );
    let mut changed = rig.request.clone();
    changed.choice = ContentConflictChoice::ChooseRemote;
    assert!(record(&mut store, &rig, &changed).await.is_err());
    assert_originals(&mut store, &rig).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(
        reopened
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_originals(&mut reopened, &rig).await;
}

#[tokio::test]
async fn all_explicit_choices_are_pending_records_and_never_modify_document_or_outbox() {
    for choice in [
        ContentConflictChoice::KeepLocalCopy,
        ContentConflictChoice::ChooseRemote,
        ContentConflictChoice::SaveMerged {
            bytes: b"merged\n".to_vec(),
        },
    ] {
        let (rig, mut store) = rig().await;
        let mut request = rig.request.clone();
        request.choice = choice.clone();
        let recorded = record(&mut store, &rig, &request).await.unwrap();
        assert_eq!(recorded.choice, choice);
        assert_eq!(recorded.base_bytes, BASE);
        assert_eq!(recorded.local_bytes, LOCAL);
        assert_eq!(recorded.remote_bytes, REMOTE);
        assert_originals(&mut store, &rig).await;
    }
}

#[tokio::test]
async fn intent_rejects_unverified_remote_wrong_generation_and_invalid_selection_without_consuming_data()
 {
    let (rig, mut store) = rig().await;
    let mut bad_revision = rig.request.clone();
    bad_revision.remote_revision = "stale-rev".into();
    let mut bad_hash = rig.request.clone();
    bad_hash.remote_hash = digest(b"other");
    let mut bad_bytes = rig.request.clone();
    bad_bytes.remote_bytes = b"corrupt remote".to_vec();
    let mut stale_generation = rig.request.clone();
    stale_generation.generation = 1;
    let mut too_large = rig.request.clone();
    too_large.choice = ContentConflictChoice::SaveMerged {
        bytes: vec![b'x'; 5 * 1024 * 1024 + 1],
    };
    let mut invalid_utf8 = rig.request.clone();
    invalid_utf8.choice = ContentConflictChoice::SaveMerged { bytes: vec![0xff] };
    for bad in [
        bad_revision,
        bad_hash,
        bad_bytes,
        stale_generation,
        too_large,
        invalid_utf8,
    ] {
        assert!(record(&mut store, &rig, &bad).await.is_err());
        assert!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        store
            .record_content_conflict_intent(&rig.binding, OTHER, &rig.operation, &rig.request)
            .await
            .is_err()
    );
    assert!(
        store
            .record_content_conflict_intent(&binding(), DOC, &rig.operation, &rig.request)
            .await
            .is_err()
    );
    assert!(
        store
            .record_content_conflict_intent(
                &rig.binding,
                DOC,
                &Uuid::now_v7().to_string(),
                &rig.request
            )
            .await
            .is_err()
    );
    assert_originals(&mut store, &rig).await;
}

#[tokio::test]
async fn intent_is_not_replaced_when_a_newer_local_checkpoint_arrives() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"newest draft\n")
        .await
        .unwrap();
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    let mut replacement = rig.request.clone();
    replacement.generation = 3;
    assert!(record(&mut store, &rig, &replacement).await.is_err());
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newest draft\n");
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        LOCAL
    );
}

#[tokio::test]
async fn intent_keeps_remote_bytes_after_current_remote_changes_and_cache_is_replaced() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    let (_, original) = remote_nodes();
    let mut value = serde_json::to_value(original).unwrap();
    value["content_rev"] = json!("new-current-rev");
    value["content_etag"] = json!("\"new-current-rev\"");
    value["node_etag"] = json!("\"new-node\"");
    value["content_hash"] = json!(digest(b"remote newer\n"));
    value["size"] = json!("13");
    value["last_change_seq"] = json!("3");
    let current: NodeState = serde_json::from_value(value).unwrap();
    let page = ChangesPage {
        workspace_epoch: info(&rig.binding).workspace_epoch,
        from_exclusive: ChangeSeq::new(2),
        through_seq: ChangeSeq::new(3),
        covered_through: ChangeSeq::new(3),
        has_more: false,
        nodes: vec![current.clone()],
    };
    store
        .apply_changes(&rig.binding, &page, ChangeSeq::new(3))
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&rig.binding, &current, b"remote newer\n")
            .await
            .unwrap()
    );
    let recorded = store
        .read_content_conflict_intent(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorded, intent);
    assert_eq!(recorded.remote_bytes, REMOTE);
    assert_originals(&mut store, &rig).await;
}

#[tokio::test]
async fn failed_intent_insert_rolls_back_without_mutating_draft_or_fifo() {
    let (rig, mut store) = rig().await;
    let mut db = connect(&rig.path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_conflict_intent BEFORE INSERT ON content_conflict_intents BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        record(&mut store, &rig, &rig.request).await,
        Err(StoreError::Storage)
    ));
    assert!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .is_none()
    );
    assert_originals(&mut store, &rig).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::query("DROP TRIGGER reject_conflict_intent")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_originals(&mut reopened, &rig).await;
    assert!(record(&mut reopened, &rig, &rig.request).await.is_ok());
}

#[tokio::test]
async fn registration_replacement_freezes_intent_and_never_transfers_or_sends_it() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = rig.binding.workspace_id.clone();
    replacement.workspace_epoch = rig.binding.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    assert!(record(&mut store, &rig, &rig.request).await.is_err());
    assert!(
        store
            .record_content_conflict_intent(&replacement, DOC, &rig.operation, &rig.request)
            .await
            .is_err()
    );
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.bytes, LOCAL);
    let old = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert!(old.frozen);
    assert_eq!(old.bytes, SENT);
    assert!(
        store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap()
            .frozen
    );
}
