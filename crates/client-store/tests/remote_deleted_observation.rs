use notryx_client_store::{ClientStore, RegistrationBinding, RemoteFileObservation, StoreError};
use notryx_domain::ChangeSeq;
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FILE: &str = "019a1000-0000-7000-8000-000000000002";
const UNKNOWN: &str = "019a1000-0000-7000-8000-000000000003";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn info(registration: &RegistrationBinding, head: u64) -> ServerInfo {
    serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2", "workspace_id": registration.workspace_id,
        "workspace_epoch": registration.workspace_epoch, "root_id": ROOT,
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

fn file(seq: u64, deleted: bool, bytes: &[u8]) -> NodeState {
    let hash = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    serde_json::from_value(json!({
        "node_id": FILE, "parent_id": ROOT, "name": "document.md", "kind": "file", "deleted": deleted,
        "content_rev": if deleted { "content-2" } else { "content-1" },
        "content_etag": if deleted { "\"content-2\"" } else { "\"content-1\"" },
        "meta_rev": if deleted { "meta-deleted" } else { "meta-live" },
        "meta_etag": if deleted { "\"meta-deleted\"" } else { "\"meta-live\"" },
        "node_etag": format!("\"node-{seq}\""), "content_hash": format!("sha256:{hash}"),
        "size": bytes.len().to_string(), "children_generation": "0", "last_change_seq": seq.to_string()
    })).unwrap()
}

async fn bootstrap(
    store: &mut ClientStore,
    registration: &RegistrationBinding,
    initial: &NodeState,
) {
    let session = store
        .begin_bootstrap(registration, &info(registration, 1))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            registration,
            &session,
            &InventoryPage {
                workspace_epoch: info(registration, 1).workspace_epoch,
                nodes: vec![root(1), initial.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(registration, &session, &info(registration, 1))
        .await
        .unwrap();
    store
        .bootstrap_publish(registration, &session, &info(registration, 1))
        .await
        .unwrap();
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
async fn synced_tombstone_is_distinct_from_live_absent_and_unbootstrapped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    assert!(matches!(
        store
            .observe_remote_file(&registration, FILE.parse().unwrap())
            .await
            .unwrap(),
        RemoteFileObservation::Pending
    ));
    let live = file(1, false, b"base\n");
    bootstrap(&mut store, &registration, &live).await;
    assert!(
        matches!(store.observe_remote_file(&registration, live.node_id).await.unwrap(), RemoteFileObservation::Live(ref node) if node == &live)
    );
    assert!(matches!(
        store
            .observe_remote_file(&registration, UNKNOWN.parse().unwrap())
            .await
            .unwrap(),
        RemoteFileObservation::Missing
    ));
    let deleted = file(2, true, b"base\n");
    let page = ChangesPage {
        workspace_epoch: info(&registration, 2).workspace_epoch,
        from_exclusive: ChangeSeq::new(1),
        through_seq: ChangeSeq::new(2),
        covered_through: ChangeSeq::new(2),
        has_more: false,
        nodes: vec![root(2), deleted.clone()],
    };
    store
        .apply_changes(&registration, &page, ChangeSeq::new(2))
        .await
        .unwrap();
    assert!(
        matches!(store.observe_remote_file(&registration, deleted.node_id).await.unwrap(), RemoteFileObservation::Deleted(ref node) if node == &deleted)
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    assert!(
        matches!(reopened.observe_remote_file(&registration, deleted.node_id).await.unwrap(), RemoteFileObservation::Deleted(ref node) if node == &deleted)
    );
}

#[tokio::test]
async fn tombstone_observation_preserves_draft_base_fifo_and_rejects_stale_binding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let live = file(1, false, b"base\n");
    bootstrap(&mut store, &registration, &live).await;
    let edit = store
        .open_registered_document(&registration, FILE, b"base\n", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            FILE,
            &edit.edit_session_id,
            1,
            b"local edit\n",
        )
        .await
        .unwrap();
    let operation = store
        .queue_checkpoint(&registration, FILE, 1)
        .await
        .unwrap();
    let deleted = file(2, true, b"base\n");
    let page = ChangesPage {
        workspace_epoch: info(&registration, 2).workspace_epoch,
        from_exclusive: ChangeSeq::new(1),
        through_seq: ChangeSeq::new(2),
        covered_through: ChangeSeq::new(2),
        has_more: false,
        nodes: vec![root(2), deleted.clone()],
    };
    store
        .apply_changes(&registration, &page, ChangeSeq::new(2))
        .await
        .unwrap();
    assert!(matches!(
        store
            .observe_remote_file(&registration, deleted.node_id)
            .await
            .unwrap(),
        RemoteFileObservation::Deleted(_)
    ));
    let draft = store
        .read_registered_checkpoint(&registration, FILE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local edit\n");
    assert_eq!(draft.base_bytes, b"base\n");
    assert_eq!(draft.base_revision, "content-1");
    let queued = store.next_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(queued.operation_id, operation.operation_id);
    assert_eq!(queued.operation_seq, operation.operation_seq);
    assert_eq!(queued.bytes, operation.bytes);
    store
        .close_registered_document(&registration, FILE, &edit.edit_session_id)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = registration.workspace_id.clone();
    replacement.workspace_epoch = registration.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert!(matches!(
        store
            .observe_remote_file(&registration, deleted.node_id)
            .await,
        Err(StoreError::StaleRegistration)
    ));
    assert!(matches!(
        store
            .observe_remote_file(&replacement, deleted.node_id)
            .await
            .unwrap(),
        RemoteFileObservation::Pending
    ));
    let old = store
        .read_registered_checkpoint(&registration, FILE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.bytes, b"local edit\n");
    assert_eq!(old.base_bytes, b"base\n");
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let row: (i64, Vec<u8>, String) =
        sqlx::query_as("SELECT operation_seq,bytes,status FROM client_outbox WHERE operation_id=?")
            .bind(operation.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(row.0, i64::try_from(operation.operation_seq).unwrap());
    assert_eq!(row.1, b"local edit\n");
    assert_eq!(row.2, "queued");
}

#[tokio::test]
async fn corrupted_remote_node_is_an_error_and_never_a_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    bootstrap(&mut store, &registration, &file(1, false, b"base\n")).await;
    store.close().await.unwrap();
    let mut db = database(&path).await;
    sqlx::query("UPDATE remote_nodes SET state=? WHERE registration_id=1 AND node_id=?")
        .bind("{\"deleted\":true}")
        .bind(FILE)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store
            .observe_remote_file(&registration, FILE.parse().unwrap())
            .await,
        Err(StoreError::UnsupportedFormat)
    ));
}

#[tokio::test]
async fn tombstone_is_not_authoritative_if_root_evidence_is_missing() {
    for alter_root_id in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drafts.db");
        let registration = binding();
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&registration).await.unwrap();
        bootstrap(&mut store, &registration, &file(1, false, b"base\n")).await;
        let page = ChangesPage {
            workspace_epoch: info(&registration, 2).workspace_epoch,
            from_exclusive: ChangeSeq::new(1),
            through_seq: ChangeSeq::new(2),
            covered_through: ChangeSeq::new(2),
            has_more: false,
            nodes: vec![root(2), file(2, true, b"base\n")],
        };
        store
            .apply_changes(&registration, &page, ChangeSeq::new(2))
            .await
            .unwrap();
        store.close().await.unwrap();
        let mut db = database(&path).await;
        if alter_root_id {
            sqlx::query("UPDATE remote_index SET root_id=? WHERE registration_id=1")
                .bind(UNKNOWN)
                .execute(&mut db)
                .await
                .unwrap();
        } else {
            sqlx::query("DELETE FROM remote_nodes WHERE registration_id=1 AND node_id=?")
                .bind(ROOT)
                .execute(&mut db)
                .await
                .unwrap();
        }
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert!(matches!(
            store.sync_index(&registration).await,
            Err(StoreError::UnsupportedFormat)
        ));
        assert!(matches!(
            store
                .observe_remote_file(&registration, FILE.parse().unwrap())
                .await,
            Err(StoreError::UnsupportedFormat)
        ));
    }
}
