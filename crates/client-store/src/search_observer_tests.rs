use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use super::*;
use notryx_protocol::{InventoryPage, ServerInfo};
use serde_json::json;
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const DOCUMENT: &str = "019a1000-0000-7000-8000-000000000002";

async fn seeded() -> (tempfile::TempDir, ClientStore, RegistrationBinding, String) {
    let directory = tempfile::tempdir().unwrap();
    let binding = RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    };
    let info: ServerInfo = serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2", "workspace_id": binding.workspace_id,
        "workspace_epoch": binding.workspace_epoch, "root_id": ROOT,
        "head_seq": "1", "journal_floor_seq": "0", "ready": true,
        "max_document_bytes": 5242880, "max_attachment_bytes": 26214400
    }))
    .unwrap();
    let root: NodeState = serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder", "deleted": false,
        "content_rev": null, "content_etag": null, "meta_rev": "root-meta",
        "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"", "content_hash": null,
        "size": null, "children_generation": "1", "last_change_seq": "1"
    }))
    .unwrap();
    let node: NodeState = serde_json::from_value(json!({
        "node_id": DOCUMENT, "parent_id": ROOT, "name": "document.md", "kind": "file", "deleted": false,
        "content_rev": "content-1", "content_etag": "\"content-1\"", "meta_rev": "meta-1",
        "meta_etag": "\"meta-1\"", "node_etag": "\"node-1\"", "content_hash": hash(b"base"),
        "size": "4", "children_generation": "0", "last_change_seq": "1"
    })).unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&binding).await.unwrap();
    let session = store.begin_bootstrap(&binding, &info).await.unwrap();
    store
        .bootstrap_inventory(
            &binding,
            &session,
            &InventoryPage {
                workspace_epoch: info.workspace_epoch,
                nodes: vec![root, node],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&binding, &session, &info)
        .await
        .unwrap();
    store
        .bootstrap_publish(&binding, &session, &info)
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&binding, DOCUMENT, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, DOCUMENT, &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    (directory, store, binding, opened.edit_session_id)
}

async fn observation_with_pause(
    mut observer: SearchObserver,
    binding: RegistrationBinding,
    observed: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<SearchPage>, StoreError> {
    observer
        .observe_inner(&binding, Some(ROOT.parse().unwrap()), &cancelled, async {
            let _ = observed.send(());
            let _ = release.await;
        })
        .await
}

#[tokio::test]
async fn paused_search_observation_does_not_block_checkpoint_or_publish_stale_draft() {
    let (_directory, store, binding, edit_session) = seeded().await;
    let observer = store.search_observer().unwrap();
    let store = Arc::new(Mutex::new(store));
    let (observed_tx, observed_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(observation_with_pause(
        observer,
        binding.clone(),
        observed_tx,
        release_rx,
        cancelled,
    ));
    tokio::time::timeout(Duration::from_secs(2), observed_rx)
        .await
        .unwrap()
        .unwrap();
    let saved = tokio::time::timeout(Duration::from_secs(1), async {
        store
            .lock()
            .await
            .save_registered_checkpoint(&binding, DOCUMENT, &edit_session, 2, b"new draft")
            .await
    })
    .await;
    let _ = release_tx.send(());
    let result = task.await.unwrap();
    assert!(
        saved.is_ok(),
        "a paused search must not hold the checkpoint writer"
    );
    saved.unwrap().unwrap();
    assert!(
        matches!(result, Ok(None)),
        "an old draft candidate must not be published"
    );
    let draft = store
        .lock()
        .await
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, b"new draft");
    assert_eq!(draft.base_bytes, b"base");
}

#[tokio::test]
async fn paused_search_does_not_block_registration_or_publish_old_registration() {
    let (_directory, mut store, binding, edit_session) = seeded().await;
    store
        .close_registered_document(&binding, DOCUMENT, &edit_session)
        .await
        .unwrap();
    let observer = store.search_observer().unwrap();
    let store = Arc::new(Mutex::new(store));
    let (observed_tx, observed_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(observation_with_pause(
        observer,
        binding.clone(),
        observed_tx,
        release_rx,
        cancelled.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), observed_rx)
        .await
        .unwrap()
        .unwrap();
    let replacement = RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..binding
    };
    let activated = tokio::time::timeout(Duration::from_secs(1), async {
        store.lock().await.activate_registration(&replacement).await
    })
    .await;
    let _ = release_tx.send(());
    let result = task.await.unwrap();
    assert!(
        activated.is_ok(),
        "a paused search must not hold registration changes"
    );
    activated.unwrap().unwrap();
    assert!(
        matches!(result, Ok(None) | Err(StoreError::StaleRegistration)),
        "stale search results must not be published"
    );
}

#[tokio::test]
async fn cancelled_search_does_not_publish_its_candidate() {
    let (_directory, store, binding, _) = seeded().await;
    let observer = store.search_observer().unwrap();
    let (observed_tx, observed_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(observation_with_pause(
        observer,
        binding,
        observed_tx,
        release_rx,
        cancelled.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), observed_rx)
        .await
        .unwrap()
        .unwrap();
    cancelled.store(true, Ordering::Release);
    let _ = release_tx.send(());
    assert!(matches!(task.await.unwrap(), Ok(None)));
}

#[tokio::test]
async fn read_snapshot_does_not_block_checkpoint_and_next_read_sees_new_wal() {
    let (_directory, mut store, binding, edit_session) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let (snapshot_tx, snapshot_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let binding_for_read = binding.clone();
    let task = tokio::spawn(async move {
        observer
            .observe_with_snapshot_hook(
                &binding_for_read,
                Some(ROOT.parse().unwrap()),
                &AtomicBool::new(false),
                async {
                    let _ = snapshot_tx.send(());
                    let _ = release_rx.await;
                },
                async {},
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), snapshot_rx)
        .await
        .unwrap()
        .unwrap();
    let saved = tokio::time::timeout(
        Duration::from_secs(1),
        store.save_registered_checkpoint(&binding, DOCUMENT, &edit_session, 2, b"new WAL draft"),
    )
    .await;
    let _ = release_tx.send(());
    let old = task.await.unwrap();
    assert!(
        saved.is_ok(),
        "a reader snapshot must not block the checkpoint writer"
    );
    saved.unwrap().unwrap();
    assert!(
        matches!(old, Ok(None)),
        "the old snapshot must not be published"
    );
    let next = store
        .search_observer()
        .unwrap()
        .observe(
            &binding,
            Some(ROOT.parse().unwrap()),
            &AtomicBool::new(false),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.candidates.len(), 1);
    assert_eq!(next.candidates[0].bytes(), b"new WAL draft");
}

#[tokio::test]
async fn observer_does_not_treat_a_folder_before_a_document_as_end_of_search() {
    let (_directory, store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let result = observer
        .observe(&binding, None, &AtomicBool::new(false))
        .await
        .unwrap()
        .unwrap();
    assert!(result.candidates.is_empty());
    assert_eq!(result.total_documents, 0);
    assert_eq!(result.available_documents, 0);
    assert_eq!(result.next_after_id, Some(ROOT.parse().unwrap()));
    let next = observer
        .observe(&binding, result.next_after_id, &AtomicBool::new(false))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.total_documents, 1);
    assert_eq!(next.available_documents, 1);
    assert_eq!(next.candidates.len(), 1);
    assert_eq!(next.candidates[0].bytes(), b"draft");
    assert!(next.next_after_id.is_none());
}

#[tokio::test]
async fn observer_keeps_store_locked_until_it_is_dropped() {
    let (directory, store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    observer
        .observe(&binding, None, &AtomicBool::new(false))
        .await
        .unwrap()
        .unwrap();
    store.close().await.unwrap();
    let path = directory.path().join("drafts.db");
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::StoreLocked)
    ));
    drop(observer);
    let reopened = ClientStore::open(&path).await.unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn observer_reuses_readonly_connection_across_pages() {
    let (_directory, store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let opened = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let first = observer
        .observe_with_hooks(
            &binding,
            None,
            &cancelled,
            async {
                opened.fetch_add(1, Ordering::AcqRel);
            },
            async {},
            async {},
        )
        .await
        .unwrap()
        .unwrap();
    let second = observer
        .observe_with_hooks(
            &binding,
            first.next_after_id,
            &cancelled,
            async {
                opened.fetch_add(1, Ordering::AcqRel);
            },
            async {},
            async {},
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.candidates.len(), 1);
    assert_eq!(second.candidates[0].bytes(), b"draft");
    assert_eq!(
        opened.load(Ordering::Acquire),
        1,
        "one search should reuse its reader connection"
    );
}

#[tokio::test]
async fn search_mutation_token_detects_local_and_other_connection_commits() {
    let (directory, mut store, binding, edit_session) = seeded().await;
    let initial = store.search_mutation_token().await.unwrap();
    store
        .save_registered_checkpoint(&binding, DOCUMENT, &edit_session, 2, b"new draft")
        .await
        .unwrap();
    let after_local = store.search_mutation_token().await.unwrap();
    assert_ne!(initial, after_local);
    let mut other = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE client_registrations SET server_origin=? WHERE status='active'")
        .bind("https://other.example")
        .execute(&mut other)
        .await
        .unwrap();
    let after_other = store.search_mutation_token().await.unwrap();
    assert_ne!(after_local, after_other);
    sqlx::query("UPDATE client_registrations SET server_origin=? WHERE status='active'")
        .bind(&binding.server_origin)
        .execute(&mut other)
        .await
        .unwrap();
    other.close().await.unwrap();
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"new draft");
    assert_eq!(draft.base_bytes, b"base");
}

#[tokio::test]
async fn observer_rejects_replaced_database_and_preserves_both_files() {
    let (directory, mut store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let path = directory.path().join("drafts.db");
    let original = directory.path().join("drafts-original.db");
    std::fs::rename(&path, &original).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    let result = observer
        .observe(&binding, None, &AtomicBool::new(false))
        .await;
    assert!(matches!(result, Err(StoreError::StorePathChanged)));
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    assert!(original.exists());
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&original, &path).unwrap();
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
}

#[tokio::test]
async fn reused_observer_rejects_database_replacement_between_pages() {
    let (directory, mut store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let first = observer
        .observe(&binding, None, &AtomicBool::new(false))
        .await
        .unwrap()
        .unwrap();
    let path = directory.path().join("drafts.db");
    let original = directory.path().join("drafts-original.db");
    std::fs::rename(&path, &original).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    let result = observer
        .observe(&binding, first.next_after_id, &AtomicBool::new(false))
        .await;
    assert!(matches!(result, Err(StoreError::StorePathChanged)));
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&original, &path).unwrap();
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
}

#[tokio::test]
async fn already_cancelled_observation_does_not_open_replaced_database() {
    let (directory, store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let path = directory.path().join("drafts.db");
    let original = directory.path().join("drafts-original.db");
    std::fs::rename(&path, &original).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    let result = observer
        .observe(&binding, None, &AtomicBool::new(true))
        .await;
    assert!(
        matches!(result, Ok(None)),
        "already cancelled searches must not open a database"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    assert!(original.exists());
}

#[tokio::test]
async fn cancellation_after_connect_does_not_start_a_read_snapshot() {
    let (_directory, store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let cancelled = AtomicBool::new(false);
    let snapshot_started = AtomicBool::new(false);
    let result = observer
        .observe_with_hooks(
            &binding,
            Some(ROOT.parse().unwrap()),
            &cancelled,
            async { cancelled.store(true, Ordering::Release) },
            async { snapshot_started.store(true, Ordering::Release) },
            async {},
        )
        .await;
    assert!(matches!(result, Ok(None)));
    assert!(
        !snapshot_started.load(Ordering::Acquire),
        "cancelled searches must not start a read transaction"
    );
}

#[tokio::test]
async fn cancellation_after_snapshot_skips_publication_work() {
    let (_directory, store, binding, _) = seeded().await;
    let mut observer = store.search_observer().unwrap();
    let cancelled = AtomicBool::new(false);
    let publication_started = AtomicBool::new(false);
    let result = observer
        .observe_with_hooks(
            &binding,
            Some(ROOT.parse().unwrap()),
            &cancelled,
            async {},
            async { cancelled.store(true, Ordering::Release) },
            async { publication_started.store(true, Ordering::Release) },
        )
        .await;
    assert!(matches!(result, Ok(None)));
    assert!(
        !publication_started.load(Ordering::Acquire),
        "cancelled searches must not enter publication"
    );
}

#[tokio::test]
async fn cancelled_cache_hash_stops_after_current_chunk() {
    let (_directory, mut store, binding, _) = seeded().await;
    let cached_id = "019a1000-0000-7000-8000-000000000003";
    let bytes = vec![b'a'; 5 * 1024 * 1024];
    let root: NodeState = serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder", "deleted": false,
        "content_rev": null, "content_etag": null, "meta_rev": "root-meta",
        "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"", "content_hash": null,
        "size": null, "children_generation": "2", "last_change_seq": "2"
    }))
    .unwrap();
    let node: NodeState = serde_json::from_value(json!({
        "node_id": cached_id, "parent_id": ROOT, "name": "large.md", "kind": "file", "deleted": false,
        "content_rev": "content-2", "content_etag": "\"content-2\"", "meta_rev": "meta-2",
        "meta_etag": "\"meta-2\"", "node_etag": "\"node-2\"", "content_hash": hash(&bytes),
        "size": bytes.len().to_string(), "children_generation": "0", "last_change_seq": "2"
    })).unwrap();
    let changes: notryx_protocol::ChangesPage = serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch, "from_exclusive": "1", "through_seq": "2",
        "covered_through": "2", "has_more": false, "nodes": [root, node]
    }))
    .unwrap();
    store
        .apply_changes(&binding, &changes, notryx_domain::ChangeSeq::new(2))
        .await
        .unwrap();
    assert!(store.cache_content(&binding, &node, &bytes).await.unwrap());
    let observer = store.search_observer().unwrap();
    let connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&observer.database_path)
            .read_only(true),
    )
    .await
    .unwrap();
    let mut reader = ClientStore {
        connection,
        sessions: Default::default(),
        _lock: observer.lock.try_clone().unwrap(),
        database_path: observer.database_path.clone(),
    };
    let cancelled = AtomicBool::new(false);
    let mut processed_chunks = 0;
    let mut progress = |_: usize| {
        processed_chunks += 1;
        cancelled.store(true, Ordering::Release);
    };
    let result = reader
        .search_candidates_with_hook(
            &binding,
            Some(DOCUMENT.parse().unwrap()),
            1,
            Some(&cancelled),
            async {},
            Some(&mut progress),
        )
        .await;
    assert!(matches!(result, Ok(None)));
    assert_eq!(
        processed_chunks, 1,
        "hashing must stop after the chunk in which cancellation was requested"
    );
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
}

#[tokio::test]
async fn cache_hash_does_not_hold_wal_snapshot_after_candidate_copy() {
    let (directory, mut store, binding, edit_session) = seeded().await;
    store
        .save_registered_checkpoint(&binding, DOCUMENT, &edit_session, 2, b"base")
        .await
        .unwrap();
    // Equal checkpoint and base bytes alone do not prove the draft was transmitted.
    store.queue_checkpoint(&binding, DOCUMENT, 2).await.unwrap();
    let operation = store.prepare_outbox(&binding).await.unwrap().unwrap();
    store
        .apply_outbox_commit(
            &binding,
            &crate::OutboxCommit {
                operation_id: operation.operation_id,
                operation_seq: operation.operation_seq,
                workspace_epoch: binding.workspace_epoch.clone(),
                document_id: DOCUMENT.into(),
                content_revision: "content-1".into(),
                content_hash: hash(b"base"),
            },
        )
        .await
        .unwrap();
    let ack = serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": "1", "retired_through": "1"
    }))
    .unwrap();
    store.confirm_receipt_ack(&binding, &ack).await.unwrap();
    let observer = store.search_observer().unwrap();
    let connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&observer.database_path)
            .read_only(true),
    )
    .await
    .unwrap();
    let mut reader = ClientStore {
        connection,
        sessions: Default::default(),
        _lock: observer.lock.try_clone().unwrap(),
        database_path: observer.database_path.clone(),
    };
    let checkpoint_result = Arc::new(std::sync::Mutex::new(None));
    let observed = checkpoint_result.clone();
    let path = directory.path().join("drafts.db");
    let mut progress = move |_: usize| {
        let mut result = observed.lock().unwrap();
        if result.is_some() {
            return;
        }
        let path = path.clone();
        *result = Some(
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let mut checkpoint = sqlx::SqliteConnection::connect_with(
                            &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
                        )
                        .await
                        .unwrap();
                        let row = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                            .fetch_one(&mut checkpoint)
                            .await
                            .unwrap();
                        let log: i64 = row.try_get(1).unwrap();
                        let checkpointed: i64 = row.try_get(2).unwrap();
                        checkpoint.close().await.unwrap();
                        (log, checkpointed)
                    })
            })
            .join()
            .unwrap(),
        );
    };
    let page = reader
        .search_candidates_with_hook(
            &binding,
            Some(ROOT.parse().unwrap()),
            1,
            None,
            async {
                store
                    .save_registered_checkpoint(&binding, DOCUMENT, &edit_session, 3, b"new draft")
                    .await
                    .unwrap();
            },
            Some(&mut progress),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.candidates.len(), 1);
    assert_eq!(page.candidates[0].bytes(), b"base");
    let (log, checkpointed) = checkpoint_result.lock().unwrap().unwrap();
    assert!(
        log > 0,
        "the writer must create WAL frames after the read snapshot"
    );
    assert_eq!(
        checkpointed, log,
        "hashing must not retain an old WAL snapshot"
    );
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"new draft");
    assert_eq!(draft.base_bytes, b"base");
}
