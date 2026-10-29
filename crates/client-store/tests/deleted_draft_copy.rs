use notryx_client_store::{
    ClientStore, DeletedDraftCopyEvidence, DeletedDraftCopyRequest, OutboxFailure,
    OutboxFailureKind, OutboxStatus, RegistrationBinding, StoreError,
};
use notryx_domain::ChangeSeq;
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const DOCUMENT: &str = "019a1000-0000-7000-8000-000000000002";
const OTHER: &str = "019a1000-0000-7000-8000-000000000003";
const FOLDER: &str = "019a1000-0000-7000-8000-000000000004";
const REPLACEMENT_FOLDER: &str = "019a1000-0000-7000-8000-000000000005";
const REMOTE_COLLISION: &str = "019a1000-0000-7000-8000-000000000006";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn info(binding: &RegistrationBinding, head: u64) -> ServerInfo {
    serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2", "workspace_id": binding.workspace_id,
        "workspace_epoch": binding.workspace_epoch, "root_id": ROOT,
        "head_seq": head.to_string(), "journal_floor_seq": "0", "ready": true,
        "max_document_bytes": 5242880, "max_attachment_bytes": 26214400
    }))
    .unwrap()
}

fn root(seq: u64) -> NodeState {
    serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder", "deleted": false,
        "content_rev": null, "content_etag": null, "meta_rev": "root-meta",
        "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"", "content_hash": null,
        "size": null, "children_generation": seq.to_string(), "last_change_seq": seq.to_string()
    }))
    .unwrap()
}

fn file(id: &str, name: &str, seq: u64, deleted: bool, bytes: &[u8]) -> NodeState {
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let revision = if deleted {
        "content-deleted"
    } else {
        "content-live"
    };
    let metadata = if deleted { "meta-deleted" } else { "meta-live" };
    serde_json::from_value(json!({
        "node_id": id, "parent_id": ROOT, "name": name, "kind": "file", "deleted": deleted,
        "content_rev": revision, "content_etag": format!("\"{revision}\""),
        "meta_rev": metadata, "meta_etag": format!("\"{metadata}\""),
        "node_etag": format!("\"node-{seq}\""), "content_hash": format!("sha256:{hash}"),
        "size": bytes.len().to_string(), "children_generation": "0", "last_change_seq": seq.to_string()
    })).unwrap()
}

fn folder(id: &str, seq: u64, deleted: bool) -> NodeState {
    let revision = if deleted {
        "folder-deleted"
    } else {
        "folder-live"
    };
    serde_json::from_value(json!({
        "node_id": id, "parent_id": ROOT, "name": "folder", "kind": "folder", "deleted": deleted,
        "content_rev": null, "content_etag": null, "meta_rev": revision,
        "meta_etag": format!("\"{revision}\""), "node_etag": format!("\"folder-node-{seq}\""),
        "content_hash": null, "size": null, "children_generation": "0", "last_change_seq": seq.to_string()
    })).unwrap()
}

async fn apply_nodes(f: &mut Fixture, seq: u64, nodes: Vec<NodeState>) {
    let page = ChangesPage {
        workspace_epoch: info(&f.binding, seq).workspace_epoch,
        from_exclusive: ChangeSeq::new(seq - 1),
        through_seq: ChangeSeq::new(seq),
        covered_through: ChangeSeq::new(seq),
        has_more: false,
        nodes,
    };
    f.store
        .apply_changes(&f.binding, &page, ChangeSeq::new(seq))
        .await
        .unwrap();
}

struct Fixture {
    directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: ClientStore,
    binding: RegistrationBinding,
    edit_session: String,
    operation: notryx_client_store::OutboxOperation,
}

async fn fixture(sent: bool, terminal: bool) -> Fixture {
    fixture_with_tombstone(sent, terminal, true).await
}

async fn fixture_with_tombstone(sent: bool, terminal: bool, tombstone: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    std::fs::write(directory.path().join("unrelated.md"), b"untouched\n").unwrap();
    let binding = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    let live = file(DOCUMENT, "source.md", 1, false, b"base\n");
    let other = file(OTHER, "existing.md", 1, false, b"other\n");
    let session = store
        .begin_bootstrap(&binding, &info(&binding, 1))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &binding,
            &session,
            &InventoryPage {
                workspace_epoch: info(&binding, 1).workspace_epoch,
                nodes: vec![root(1), live, other],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&binding, &session, &info(&binding, 1))
        .await
        .unwrap();
    store
        .bootstrap_publish(&binding, &session, &info(&binding, 1))
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&binding, DOCUMENT, b"base\n", "content-live")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            DOCUMENT,
            &opened.edit_session_id,
            1,
            b"local draft\n",
        )
        .await
        .unwrap();
    let mut operation = store.queue_checkpoint(&binding, DOCUMENT, 1).await.unwrap();
    if sent {
        operation = store.prepare_outbox(&binding).await.unwrap().unwrap();
        assert_eq!(operation.status, OutboxStatus::Sent);
    }
    if terminal {
        assert!(sent);
        store
            .apply_outbox_failure(
                &binding,
                &OutboxFailure {
                    operation_id: operation.operation_id.clone(),
                    operation_seq: operation.operation_seq,
                    workspace_epoch: binding.workspace_epoch.clone(),
                    kind: OutboxFailureKind::NodeDeleted,
                },
            )
            .await
            .unwrap();
        operation = store
            .read_outbox_operation(&binding, &operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.failure_kind, Some(OutboxFailureKind::NodeDeleted));
    }
    let page = ChangesPage {
        workspace_epoch: info(&binding, 2).workspace_epoch,
        from_exclusive: ChangeSeq::new(1),
        through_seq: ChangeSeq::new(2),
        covered_through: ChangeSeq::new(2),
        has_more: false,
        nodes: vec![root(2), file(DOCUMENT, "source.md", 2, true, b"base\n")],
    };
    if tombstone {
        store
            .apply_changes(&binding, &page, ChangeSeq::new(2))
            .await
            .unwrap();
    }
    Fixture {
        directory,
        path,
        store,
        binding,
        edit_session: opened.edit_session_id,
        operation,
    }
}

fn request(fixture: &Fixture, evidence: DeletedDraftCopyEvidence) -> DeletedDraftCopyRequest {
    DeletedDraftCopyRequest {
        edit_session_id: fixture.edit_session.clone(),
        generation: 1,
        local_bytes: b"local draft\n".to_vec(),
        parent_id: ROOT.into(),
        name: "saved-copy.md".into(),
        evidence,
    }
}

async fn assert_sources(fixture: &mut Fixture) {
    let draft = fixture
        .store
        .read_registered_checkpoint(&fixture.binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    assert_eq!(draft.base_revision, "content-live");
    assert_eq!(
        fixture
            .store
            .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
            .await
            .unwrap(),
        Some(fixture.operation.clone())
    );
    assert_eq!(
        std::fs::read(fixture.directory.path().join("unrelated.md")).unwrap(),
        b"untouched\n"
    );
    assert!(!fixture.directory.path().join("saved-copy.md").exists());
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

#[tokio::test]
async fn observed_tombstone_confirms_copy_without_concluding_a_sent_operation() {
    let mut f = fixture(true, false).await;
    let original = f.operation.clone();
    let request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    let intent = f
        .store
        .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
        .await
        .unwrap();
    assert_eq!(intent.local_bytes, b"local draft\n");
    assert_eq!(intent.evidence, request.evidence);
    assert_eq!(intent.parent_id, ROOT);
    assert_eq!(intent.name, "saved-copy.md");
    assert_eq!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .unwrap(),
        intent
    );
    assert_sources(&mut f).await;
    assert_eq!(
        f.store
            .list_outbox(&f.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        f.store
            .read_outbox_operation(&f.binding, &original.operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        OutboxStatus::Sent
    );
    f.store
        .close_registered_document(&f.binding, DOCUMENT, &f.edit_session)
        .await
        .unwrap();
    f.store.close().await.unwrap();
    let mut reopened = ClientStore::open(&f.path).await.unwrap();
    assert_eq!(
        reopened
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        Some(intent)
    );
    assert_eq!(
        reopened
            .read_outbox_operation(&f.binding, &original.operation_id)
            .await
            .unwrap(),
        Some(original)
    );
}

#[tokio::test]
async fn exact_confirmation_retry_keeps_ids_after_destination_name_is_taken() {
    let mut f = fixture(false, false).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    let intent = f
        .store
        .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
        .await
        .unwrap();
    apply_nodes(
        &mut f,
        3,
        vec![
            root(3),
            file(REMOTE_COLLISION, "saved-copy.md", 3, false, b"peer\n"),
        ],
    )
    .await;
    assert_eq!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .unwrap(),
        intent
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn exact_confirmation_retry_keeps_ids_after_destination_parent_is_deleted() {
    let mut f = fixture(false, false).await;
    apply_nodes(&mut f, 3, vec![root(3), folder(FOLDER, 3, false)]).await;
    let mut request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    request.parent_id = FOLDER.into();
    let intent = f
        .store
        .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
        .await
        .unwrap();
    apply_nodes(&mut f, 4, vec![root(4), folder(FOLDER, 4, true)]).await;
    assert_eq!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .unwrap(),
        intent
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn node_deleted_receipt_is_recorded_separately_from_the_tombstone() {
    for include_receipt in [false, true] {
        let mut f = fixture(true, true).await;
        if include_receipt {
            let mut db = database(&f.path).await;
            let receipt = json!({"status":"node_deleted", "operation_id":f.operation.operation_id.clone(),
                "operation_seq":f.operation.operation_seq.to_string(), "workspace_epoch":f.binding.workspace_epoch.clone()}).to_string();
            sqlx::query("UPDATE client_outbox SET receipt_json=? WHERE operation_id=?")
                .bind(receipt)
                .bind(&f.operation.operation_id)
                .execute(&mut db)
                .await
                .unwrap();
            db.close().await.unwrap();
        }
        let request = request(
            &f,
            DeletedDraftCopyEvidence::NodeDeleted {
                operation_id: f.operation.operation_id.clone(),
            },
        );
        let intent = f
            .store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .unwrap();
        assert_eq!(intent.evidence, request.evidence);
        assert_eq!(
            f.operation.failure_kind,
            Some(OutboxFailureKind::NodeDeleted)
        );
        assert_sources(&mut f).await;
    }
}

#[tokio::test]
async fn terminal_alone_cannot_confirm_deletion_from_a_live_cached_node() {
    let mut f = fixture_with_tombstone(true, true, false).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::NodeDeleted {
            operation_id: f.operation.operation_id.clone(),
        },
    );
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn old_terminal_cannot_confirm_after_tombstone_was_restored() {
    let mut f = fixture(true, true).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::NodeDeleted {
            operation_id: f.operation.operation_id.clone(),
        },
    );
    let mut restored = file(DOCUMENT, "source.md", 3, false, b"base\n");
    restored.content_rev = Some(notryx_domain::ContentRevision::new("content-restored").unwrap());
    restored.content_etag = Some("\"content-restored\"".into());
    apply_nodes(&mut f, 3, vec![root(3), restored]).await;
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn stale_generation_and_occupied_destination_do_not_record_a_choice() {
    let mut f = fixture(false, false).await;
    let mut stale = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    stale.generation = 0;
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &stale)
            .await
            .is_err()
    );
    let mut collision = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    collision.name = "existing.md".into();
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &collision)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn later_edit_preserves_the_confirmed_copy_bytes_and_new_checkpoint() {
    let mut f = fixture(false, false).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    let intent = f
        .store
        .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
        .await
        .unwrap();
    f.store
        .save_registered_checkpoint(&f.binding, DOCUMENT, &f.edit_session, 2, b"newer draft\n")
        .await
        .unwrap();
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        Some(intent.clone())
    );
    assert_eq!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .unwrap(),
        intent
    );
    let draft = f
        .store
        .read_registered_checkpoint(&f.binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    assert_eq!(
        f.store
            .read_outbox_operation(&f.binding, &f.operation.operation_id)
            .await
            .unwrap(),
        Some(f.operation.clone())
    );
}

#[tokio::test]
async fn confirmed_copy_bytes_corruption_after_later_edit_is_rejected() {
    let mut f = fixture(false, false).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    let intent = f
        .store
        .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
        .await
        .unwrap();
    f.store
        .save_registered_checkpoint(&f.binding, DOCUMENT, &f.edit_session, 2, b"newer draft\n")
        .await
        .unwrap();
    let mut db = database(&f.path).await;
    assert_eq!(b"other draft\n".len(), intent.local_bytes.len());
    sqlx::query("UPDATE deleted_draft_copy_intents SET local_bytes=? WHERE node_id=?")
        .bind(b"other draft\n".as_slice())
        .bind(&intent.node_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        f.store.read_deleted_draft_copy(&f.binding, DOCUMENT).await,
        Err(StoreError::UnsupportedFormat)
    ));
    let draft = f
        .store
        .read_registered_checkpoint(&f.binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    assert_eq!(
        f.store
            .read_outbox_operation(&f.binding, &f.operation.operation_id)
            .await
            .unwrap(),
        Some(f.operation.clone())
    );
}

#[tokio::test]
async fn replacement_registration_cannot_confirm_or_rebind_an_old_draft() {
    let mut f = fixture(false, false).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    f.store
        .close_registered_document(&f.binding, DOCUMENT, &f.edit_session)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = f.binding.workspace_id.clone();
    replacement.workspace_epoch = f.binding.workspace_epoch.clone();
    f.store.activate_registration(&replacement).await.unwrap();
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    assert!(
        f.store
            .record_deleted_draft_copy(&replacement, DOCUMENT, &request)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    let draft = f
        .store
        .read_registered_checkpoint(&f.binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    let old = f
        .store
        .read_outbox_operation(&f.binding, &f.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(old.frozen);
    assert_eq!(old.bytes, f.operation.bytes);
}

#[tokio::test]
async fn tombstone_must_match_the_current_document_and_latest_deletion_sequence() {
    let mut f = fixture(false, false).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    let mut wrong_seq = request.clone();
    wrong_seq.evidence = DeletedDraftCopyEvidence::Tombstone {
        last_change_seq: ChangeSeq::new(3),
    };
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &wrong_seq)
            .await
            .is_err()
    );
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, OTHER, &request)
            .await
            .is_err()
    );
    let mut wrong_epoch = f.binding.clone();
    wrong_epoch.workspace_epoch = Uuid::new_v4().to_string();
    assert!(
        f.store
            .record_deleted_draft_copy(&wrong_epoch, DOCUMENT, &request)
            .await
            .is_err()
    );
    let mut restored = file(DOCUMENT, "source.md", 3, false, b"base\n");
    restored.content_rev = Some(notryx_domain::ContentRevision::new("content-restored").unwrap());
    restored.content_etag = Some("\"content-restored\"".into());
    apply_nodes(&mut f, 3, vec![root(3), restored]).await;
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    let mut deleted_again = file(DOCUMENT, "source.md", 4, true, b"base\n");
    deleted_again.content_rev =
        Some(notryx_domain::ContentRevision::new("content-deleted-again").unwrap());
    deleted_again.content_etag = Some("\"content-deleted-again\"".into());
    deleted_again.meta_rev = notryx_domain::MetaRevision::new("meta-deleted-again").unwrap();
    deleted_again.meta_etag = "\"meta-deleted-again\"".into();
    apply_nodes(&mut f, 4, vec![root(4), deleted_again]).await;
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn deleted_or_replaced_destination_parent_does_not_authorize_a_copy() {
    let mut f = fixture(false, false).await;
    apply_nodes(&mut f, 3, vec![root(3), folder(FOLDER, 3, false)]).await;
    let mut request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    request.parent_id = FOLDER.into();
    apply_nodes(&mut f, 4, vec![root(4), folder(FOLDER, 4, true)]).await;
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    apply_nodes(
        &mut f,
        5,
        vec![root(5), folder(REPLACEMENT_FOLDER, 5, false)],
    )
    .await;
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn mismatched_session_and_same_generation_bytes_or_base_are_rejected() {
    for corrupted in ["session", "checkpoint", "base"] {
        let mut f = fixture(false, false).await;
        let mut request = request(
            &f,
            DeletedDraftCopyEvidence::Tombstone {
                last_change_seq: ChangeSeq::new(2),
            },
        );
        if corrupted == "session" {
            request.edit_session_id = Uuid::new_v4().to_string();
        } else {
            let mut db = database(&f.path).await;
            let sql = if corrupted == "checkpoint" {
                "UPDATE draft_checkpoints SET bytes=X'6f746865722064726166740a'"
            } else {
                "UPDATE draft_bases SET bytes=X'6f7468657220626173650a'"
            };
            sqlx::query(sql).execute(&mut db).await.unwrap();
            db.close().await.unwrap();
        }
        assert!(
            f.store
                .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
                .await
                .is_err(),
            "{corrupted}"
        );
        assert_eq!(
            f.store
                .read_deleted_draft_copy(&f.binding, DOCUMENT)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            f.store
                .read_outbox_operation(&f.binding, &f.operation.operation_id)
                .await
                .unwrap(),
            Some(f.operation.clone())
        );
        assert_eq!(
            std::fs::read(f.directory.path().join("unrelated.md")).unwrap(),
            b"untouched\n"
        );
    }
}

#[tokio::test]
async fn terminal_evidence_requires_matching_document_failure_and_acknowledged_sequence() {
    for mutation in [
        "document",
        "kind",
        "ack",
        "sequence",
        "receipt_id",
        "receipt_seq",
        "receipt_epoch",
        "receipt_kind",
        "receipt_invalid",
    ] {
        let mut f = fixture(true, true).await;
        let request = request(
            &f,
            DeletedDraftCopyEvidence::NodeDeleted {
                operation_id: f.operation.operation_id.clone(),
            },
        );
        let mut db = database(&f.path).await;
        let sql = match mutation {
            "document" => {
                "UPDATE client_outbox SET document_id='019a1000-0000-7000-8000-000000000003'"
            }
            "kind" => "UPDATE client_outbox SET terminal_kind='not_found'",
            "ack" => "UPDATE outbox_streams SET ack_through=0",
            "sequence" => "UPDATE client_outbox SET operation_seq=2",
            _ => "UPDATE client_outbox SET receipt_json=?",
        };
        if mutation.starts_with("receipt_") {
            let receipt = if mutation == "receipt_invalid" {
                "{}".to_owned()
            } else {
                json!({"status":if mutation == "receipt_kind" {"not_found"} else {"node_deleted"},
                    "operation_id":if mutation == "receipt_id" {Uuid::now_v7().to_string()} else {f.operation.operation_id.clone()},
                    "operation_seq":if mutation == "receipt_seq" {"2".to_owned()} else {f.operation.operation_seq.to_string()},
                    "workspace_epoch":if mutation == "receipt_epoch" {Uuid::new_v4().to_string()} else {f.binding.workspace_epoch.clone()}}).to_string()
            };
            sqlx::query(sql)
                .bind(receipt)
                .execute(&mut db)
                .await
                .unwrap();
        } else {
            sqlx::query(sql).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        assert!(
            f.store
                .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
                .await
                .is_err(),
            "{mutation}"
        );
        assert_eq!(
            f.store
                .read_deleted_draft_copy(&f.binding, DOCUMENT)
                .await
                .unwrap(),
            None
        );
        let draft = f
            .store
            .read_registered_checkpoint(&f.binding, DOCUMENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft\n");
        assert_eq!(draft.base_bytes, b"base\n");
        assert_eq!(
            std::fs::read(f.directory.path().join("unrelated.md")).unwrap(),
            b"untouched\n"
        );
    }
}

#[tokio::test]
async fn recorded_terminal_evidence_with_damaged_receipt_is_not_reused() {
    let mut f = fixture(true, true).await;
    let request = request(
        &f,
        DeletedDraftCopyEvidence::NodeDeleted {
            operation_id: f.operation.operation_id.clone(),
        },
    );
    let intent = f
        .store
        .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
        .await
        .unwrap();
    let mut db = database(&f.path).await;
    sqlx::query("UPDATE client_outbox SET receipt_json='{}' WHERE operation_id=?")
        .bind(&f.operation.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT local_bytes FROM deleted_draft_copy_intents WHERE node_id=?")
            .bind(&intent.node_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        f.store.read_deleted_draft_copy(&f.binding, DOCUMENT).await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert_eq!(bytes, b"local draft\n");
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn terminal_evidence_cannot_use_a_different_registrations_operation() {
    let mut f = fixture(true, true).await;
    let other_operation = Uuid::now_v7().to_string();
    let mut db = database(&f.path).await;
    let other = binding();
    sqlx::query("INSERT INTO client_registrations VALUES (2,?,?,?,?,?,'frozen')")
        .bind(&other.server_origin)
        .bind(&other.workspace_id)
        .bind(&other.workspace_epoch)
        .bind(&other.device_id)
        .bind(&other.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox_streams VALUES (2,2,1,0)")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,dependency,bytes,payload_hash,status,committed_revision,conflict_revision,conflict_etag,abandon_state,terminal_kind,payload_retired,payload_len,receipt_json) SELECT ?,2,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,dependency,bytes,payload_hash,status,committed_revision,conflict_revision,conflict_etag,abandon_state,terminal_kind,payload_retired,payload_len,receipt_json FROM client_outbox WHERE operation_id=?")
        .bind(&other_operation).bind(&f.operation.operation_id).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let request = request(
        &f,
        DeletedDraftCopyEvidence::NodeDeleted {
            operation_id: other_operation,
        },
    );
    assert!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    assert_sources(&mut f).await;
}

#[tokio::test]
async fn failed_confirmation_transaction_preserves_sources_and_creates_no_intent() {
    let mut f = fixture(false, false).await;
    let mut db = database(&f.path).await;
    sqlx::raw_sql("CREATE TRIGGER fail_deleted_copy BEFORE INSERT ON deleted_draft_copy_intents BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let request = request(
        &f,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: ChangeSeq::new(2),
        },
    );
    assert!(matches!(
        f.store
            .record_deleted_draft_copy(&f.binding, DOCUMENT, &request)
            .await,
        Err(StoreError::Storage)
    ));
    assert_eq!(
        f.store
            .read_deleted_draft_copy(&f.binding, DOCUMENT)
            .await
            .unwrap(),
        None
    );
    assert_sources(&mut f).await;
}
