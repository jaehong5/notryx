use notryx_client_store::{ClientStore, OutboxStatus, RegistrationBinding};
use notryx_protocol::RestoreNodeRequest;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const DOC_A: &str = "019a1000-0000-7000-8000-000000000002";
const DOC_B: &str = "019a1000-0000-7000-8000-000000000003";
const DOC_C: &str = "019a1000-0000-7000-8000-000000000004";
const TRASH: &str = "019a1000-0000-7000-8000-000000000005";
const ROOT: &str = "019a1000-0000-7000-8000-000000000001";

fn binding(workspace: &str, epoch: &str) -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: workspace.into(),
        workspace_epoch: epoch.into(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

async fn queue(
    store: &mut ClientStore,
    binding: &RegistrationBinding,
    document: &str,
    bytes: &[u8],
) -> notryx_client_store::OutboxOperation {
    let edit = store
        .open_registered_document(binding, document, b"base", "base-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(binding, document, &edit.edit_session_id, 1, bytes)
        .await
        .unwrap();
    store
        .close_registered_document(binding, document, &edit.edit_session_id)
        .await
        .unwrap();
    store.queue_checkpoint(binding, document, 1).await.unwrap()
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: ClientStore,
    old: RegistrationBinding,
    epoch: RegistrationBinding,
    foreign: RegistrationBinding,
    current: RegistrationBinding,
    sent: notryx_client_store::OutboxOperation,
    successor: notryx_client_store::OutboxOperation,
    other_epoch: notryx_client_store::OutboxOperation,
    foreign_item: notryx_client_store::OutboxOperation,
    active_item: notryx_client_store::OutboxOperation,
    restore_id: String,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing-frozen-list.sqlite");
    let mut store = ClientStore::open(&path).await.unwrap();
    let workspace = Uuid::new_v4().to_string();
    let original_epoch = Uuid::new_v4().to_string();
    let old = binding(&workspace, &original_epoch);
    store.activate_registration(&old).await.unwrap();
    let opened = store
        .open_registered_document(&old, DOC_A, b"original base", "base-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, DOC_A, &opened.edit_session_id, 1, b"sent payload")
        .await
        .unwrap();
    store.queue_checkpoint(&old, DOC_A, 1).await.unwrap();
    let sent = store.prepare_outbox(&old).await.unwrap().unwrap();
    assert_eq!(sent.status, OutboxStatus::Sent);
    let restore = store
        .queue_restore(
            &old,
            TRASH.parse().unwrap(),
            &RestoreNodeRequest {
                expected_meta_rev: notryx_domain::MetaRevision::new("deleted-meta").unwrap(),
                parent_id: ROOT.parse().unwrap(),
                name: "restored.md".into(),
            },
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, DOC_A, &opened.edit_session_id, 2, b"later draft")
        .await
        .unwrap();
    let successor = store.queue_checkpoint(&old, DOC_A, 2).await.unwrap();
    assert_eq!(sent.operation_seq, 1);
    assert_eq!(restore.operation_seq, 2);
    assert_eq!(successor.operation_seq, 3);
    store
        .close_registered_document(&old, DOC_A, &opened.edit_session_id)
        .await
        .unwrap();

    let epoch = binding(&workspace, &Uuid::new_v4().to_string());
    store.activate_registration(&epoch).await.unwrap();
    let other_epoch = queue(&mut store, &epoch, DOC_B, b"other epoch draft").await;
    let foreign = binding(&Uuid::new_v4().to_string(), &original_epoch);
    store.activate_registration(&foreign).await.unwrap();
    let foreign_item = queue(&mut store, &foreign, DOC_A, b"other workspace draft").await;
    let current = binding(&workspace, &original_epoch);
    store.activate_registration(&current).await.unwrap();
    let active_item = queue(&mut store, &current, DOC_C, b"current workspace draft").await;
    Fixture {
        _directory: directory,
        path,
        store,
        old,
        epoch,
        foreign,
        current,
        sent,
        successor,
        other_epoch,
        foreign_item,
        active_item,
        restore_id: restore.operation_id,
    }
}

async fn snapshot(
    path: &Path,
) -> (
    Vec<(String, String, i64, String, Vec<u8>, String, i64)>,
    Vec<(
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        i64,
    )>,
    Vec<(String, i64, Vec<u8>, Vec<u8>, String)>,
    Vec<(i64, i64, i64, i64)>,
    Vec<(i64, String, String, String, String, String, String)>,
) {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let operations = sqlx::query_as("SELECT operation_id,operation_kind,operation_seq,status,bytes,payload_hash,payload_len FROM client_outbox ORDER BY operation_id").fetch_all(&mut db).await.unwrap();
    let operation_evidence = sqlx::query_as("SELECT operation_id,registration_id,expected_revision,dependency,abandon_state,terminal_kind,payload_retired FROM client_outbox ORDER BY operation_id").fetch_all(&mut db).await.unwrap();
    let drafts = sqlx::query_as("SELECT c.document_id,c.generation,c.bytes,b.bytes,b.revision FROM draft_checkpoints c JOIN draft_bases b USING(document_id) ORDER BY c.document_id").fetch_all(&mut db).await.unwrap();
    let watermarks = sqlx::query_as("SELECT registration_id,next_seq,ack_through,confirmed_through FROM outbox_streams ORDER BY registration_id").fetch_all(&mut db).await.unwrap();
    let registrations = sqlx::query_as("SELECT id,server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,status FROM client_registrations ORDER BY id").fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    (
        operations,
        operation_evidence,
        drafts,
        watermarks,
        registrations,
    )
}

#[tokio::test]
async fn existing_draft_and_outbox_listing_keeps_binding_seq_and_original_data() {
    let mut rig = fixture().await;
    let before = snapshot(&rig.path).await;
    let dependent = before
        .1
        .iter()
        .find(|row| row.0 == rig.successor.operation_id)
        .unwrap();
    assert_eq!(dependent.3.as_deref(), Some(rig.sent.operation_id.as_str()));
    let drafts = rig.store.list_registered_drafts().await.unwrap();
    assert_eq!(drafts.len(), 4);
    for (binding, document, generation) in [
        (&rig.old, DOC_A, 2),
        (&rig.epoch, DOC_B, 1),
        (&rig.foreign, DOC_A, 1),
        (&rig.current, DOC_C, 1),
    ] {
        assert!(drafts.iter().any(|item| &item.binding == binding
            && item.document_id == document
            && item.generation == generation));
    }
    let first = rig.store.list_outbox(&rig.old, 0, 1).await.unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.entries[0].operation_id, rig.sent.operation_id);
    assert_eq!(first.entries[0].operation_seq, 1);
    assert_eq!(first.entries[0].status, OutboxStatus::Sent);
    assert!(first.entries[0].frozen);
    assert_eq!(first.next_after_seq, Some(1));
    let second = rig
        .store
        .list_outbox(&rig.old, first.next_after_seq.unwrap(), 1)
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].operation_id, rig.successor.operation_id);
    assert_eq!(second.entries[0].operation_seq, 3);
    assert_eq!(second.entries[0].status, OutboxStatus::Queued);
    assert!(second.entries[0].frozen);
    assert_eq!(second.next_after_seq, None);
    let after_restore = rig.store.list_outbox(&rig.old, 2, 1).await.unwrap();
    assert_eq!(
        after_restore.entries[0].operation_id,
        rig.successor.operation_id
    );
    assert!(
        ![
            rig.sent.operation_id.as_str(),
            rig.successor.operation_id.as_str()
        ]
        .contains(&rig.restore_id.as_str())
    );
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later draft");
    assert_eq!(draft.base_bytes, b"original base");
    let sent = rig
        .store
        .read_outbox_operation(&rig.old, &rig.sent.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent.bytes, b"sent payload");
    assert!(sent.frozen && sent.status == OutboxStatus::Sent);
    let successor = rig
        .store
        .read_outbox_operation(&rig.old, &rig.successor.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(successor.bytes, b"later draft");
    assert!(successor.frozen && successor.expected_revision.is_none());
    let epoch_page = rig.store.list_outbox(&rig.epoch, 0, 10).await.unwrap();
    let foreign_page = rig.store.list_outbox(&rig.foreign, 0, 10).await.unwrap();
    let current_page = rig.store.list_outbox(&rig.current, 0, 10).await.unwrap();
    assert_eq!(
        epoch_page.entries[0].operation_id,
        rig.other_epoch.operation_id
    );
    assert!(epoch_page.entries[0].frozen);
    assert_eq!(
        foreign_page.entries[0].operation_id,
        rig.foreign_item.operation_id
    );
    assert!(foreign_page.entries[0].frozen);
    assert_eq!(
        current_page.entries[0].operation_id,
        rig.active_item.operation_id
    );
    assert!(!current_page.entries[0].frozen);
    assert!(
        rig.store
            .read_registered_checkpoint(&rig.current, DOC_A)
            .await
            .unwrap()
            .is_none()
    );
    assert!(rig.store.next_pending_outbox(&rig.old).await.is_err());
    assert_eq!(snapshot(&rig.path).await, before);
}

#[tokio::test]
async fn existing_outbox_pagination_rejects_invalid_limits_and_keeps_foreign_binding_explicit() {
    let mut rig = fixture().await;
    let before = snapshot(&rig.path).await;
    assert!(rig.store.list_outbox(&rig.old, 0, 0).await.is_err());
    assert!(rig.store.list_outbox(&rig.old, 0, 101).await.is_err());
    assert!(rig.store.list_outbox(&rig.old, u64::MAX, 1).await.is_err());
    let mut unknown = rig.old.clone();
    unknown.device_stream_id = Uuid::new_v4().to_string();
    assert!(rig.store.list_outbox(&unknown, 0, 10).await.is_err());
    assert_eq!(snapshot(&rig.path).await, before);
}
