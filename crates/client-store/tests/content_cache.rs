use notryx_client_store::{ClientStore, RegistrationBinding, StoreError};
use notryx_domain::ChangeSeq;
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FILE: &str = "019a1000-0000-7000-8000-000000000002";

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

fn file(seq: u64, deleted: bool, bytes: &[u8]) -> NodeState {
    let hash = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    serde_json::from_value(json!({
        "node_id": FILE, "parent_id": ROOT, "name": "document.md", "kind": "file", "deleted": deleted,
        "content_rev": format!("content-{seq}"), "content_etag": format!("\"content-{seq}\""),
        "meta_rev": if deleted { "meta-deleted" } else { "meta-live" },
        "meta_etag": if deleted { "\"meta-deleted\"" } else { "\"meta-live\"" },
        "node_etag": format!("\"node-{seq}\""),
        "content_hash": format!("sha256:{hash}"), "size": bytes.len().to_string(),
        "children_generation": "0", "last_change_seq": seq.to_string()
    })).unwrap()
}

async fn seeded(
    bytes: &[u8],
) -> (
    tempfile::TempDir,
    ClientStore,
    RegistrationBinding,
    NodeState,
) {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let initial = file(1, false, bytes);
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &InventoryPage {
                workspace_epoch: info(&registration, 1).workspace_epoch,
                nodes: vec![root(1), initial.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1))
        .await
        .unwrap();
    (directory, store, registration, initial)
}

async fn apply(
    store: &mut ClientStore,
    binding: &RegistrationBinding,
    from: u64,
    nodes: Vec<NodeState>,
) {
    let page = ChangesPage {
        workspace_epoch: info(binding, from + 1).workspace_epoch,
        from_exclusive: ChangeSeq::new(from),
        through_seq: ChangeSeq::new(from + 1),
        covered_through: ChangeSeq::new(from + 1),
        has_more: false,
        nodes,
    };
    store
        .apply_changes(binding, &page, ChangeSeq::new(from + 1))
        .await
        .unwrap();
}

#[tokio::test]
async fn cache_validates_bytes_and_persists_without_touching_draft_or_outbox() {
    let original = b"\xef\xbb\xbf# \xed\x95\x9c\xea\xb8\x80\r\n";
    let (directory, mut store, registration, node) = seeded(original).await;
    let opened = store
        .open_registered_document(&registration, FILE, original, "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            FILE,
            &opened.edit_session_id,
            1,
            b"local draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, FILE, &opened.edit_session_id)
        .await
        .unwrap();
    let operation = store
        .queue_checkpoint(&registration, FILE, 1)
        .await
        .unwrap();
    assert_eq!(
        store.cache_content(&registration, &node, original).await,
        Ok(true)
    );
    let cached = store
        .read_cached_content(&registration, node.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.bytes, original);
    assert_eq!(cached.content_rev, "content-1");
    assert!(cached.current);
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    assert_eq!(
        reopened
            .read_cached_content(&registration, node.node_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        original
    );
    let draft = reopened
        .read_registered_checkpoint(&registration, FILE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft");
    assert_eq!(draft.base_bytes, original);
    assert_eq!(
        reopened
            .read_outbox_operation(&registration, &operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        operation
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn late_download_never_overwrites_current_cache_or_promotes_deleted_node() {
    let (directory, mut store, registration, old) = seeded(b"old").await;
    assert_eq!(
        store.cache_content(&registration, &old, b"old").await,
        Ok(true)
    );
    let new = file(2, false, b"new");
    apply(&mut store, &registration, 1, vec![new.clone()]).await;
    let stale_cache = store
        .read_cached_content(&registration, old.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stale_cache.bytes, b"old");
    assert!(!stale_cache.current);
    assert_eq!(
        store.cache_content(&registration, &old, b"old").await,
        Ok(false)
    );
    assert_eq!(
        store.cache_content(&registration, &new, b"new").await,
        Ok(true)
    );
    assert_eq!(
        store.cache_content(&registration, &old, b"old").await,
        Ok(false)
    );
    let current = store
        .read_cached_content(&registration, old.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.bytes, b"new");
    assert!(current.current);
    let tombstone = file(3, true, b"new");
    apply(&mut store, &registration, 2, vec![root(3), tombstone]).await;
    assert_eq!(
        store.cache_content(&registration, &new, b"new").await,
        Ok(false)
    );
    assert!(
        !store
            .read_cached_content(&registration, old.node_id)
            .await
            .unwrap()
            .unwrap()
            .current
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    assert!(
        !reopened
            .read_cached_content(&registration, old.node_id)
            .await
            .unwrap()
            .unwrap()
            .current
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn invalid_bytes_and_storage_failure_preserve_existing_cache() {
    let (directory, mut store, registration, old) = seeded(b"old").await;
    store
        .cache_content(&registration, &old, b"old")
        .await
        .unwrap();
    assert_eq!(
        store.cache_content(&registration, &old, b"bad").await,
        Err(StoreError::InvalidRequest)
    );
    assert_eq!(
        store
            .cache_content(&registration, &old, &vec![0; 25 * 1024 * 1024 + 1])
            .await,
        Err(StoreError::DocumentTooLarge)
    );
    let new = file(2, false, b"new");
    apply(&mut store, &registration, 1, vec![new.clone()]).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_cache_publish BEFORE UPDATE OF content_ready ON remote_nodes BEGIN SELECT RAISE(ABORT, 'synthetic'); END")
        .execute(&mut db).await.unwrap();
    assert_eq!(
        store.cache_content(&registration, &new, b"new").await,
        Err(StoreError::Storage)
    );
    let cached = store
        .read_cached_content(&registration, old.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.bytes, b"old");
    assert!(!cached.current);
    db.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn stale_registration_and_corrupted_cached_bytes_are_rejected() {
    let (directory, mut store, registration, node) = seeded(b"old").await;
    store
        .cache_content(&registration, &node, b"old")
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE cached_content SET bytes=x'00'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        store.read_cached_content(&registration, node.node_id).await,
        Err(StoreError::UnsupportedFormat)
    );
    let ready: i64 = sqlx::query_scalar("SELECT content_ready FROM remote_nodes WHERE node_id=?")
        .bind(FILE)
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(ready, 0);
    db.close().await.unwrap();
    let replacement = binding();
    store.activate_registration(&replacement).await.unwrap();
    assert_eq!(
        store.cache_content(&registration, &node, b"old").await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(
        store.read_cached_content(&registration, node.node_id).await,
        Err(StoreError::StaleRegistration)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn bootstrap_reuses_verified_cache_without_removing_unpublished_edits() {
    let (directory, mut store, registration, node) = seeded(b"base").await;
    store
        .cache_content(&registration, &node, b"base")
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&registration, FILE, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, FILE, &opened.edit_session_id, 1, b"edited")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, FILE, &opened.edit_session_id)
        .await
        .unwrap();
    let queued = store
        .queue_checkpoint(&registration, FILE, 1)
        .await
        .unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &InventoryPage {
                workspace_epoch: info(&registration, 1).workspace_epoch,
                nodes: vec![root(1), node.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1))
        .await
        .unwrap();
    let cached = store
        .read_cached_content(&registration, node.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.bytes, b"base");
    assert!(cached.current);
    let draft = store
        .read_registered_checkpoint(&registration, FILE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"edited");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(
        store
            .read_outbox_operation(&registration, &queued.operation_id)
            .await
            .unwrap()
            .unwrap(),
        queued
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    assert!(
        reopened
            .read_cached_content(&registration, node.node_id)
            .await
            .unwrap()
            .unwrap()
            .current
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn failed_v6_to_v7_migration_keeps_draft_base_outbox_and_recovery_access() {
    use notryx_client_store::{OutboxAbandonState, OutboxOperation, OutboxStatus, RecoveryStore};
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let node = file(1, false, b"base");
    let path = directory.path().join("drafts.db");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    let v7 = include_str!("fixtures/client_store_v7.sql");
    let v6 = v7.split("CREATE TABLE cached_content (").next().unwrap();
    sqlx::raw_sql(v6).execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO client_registrations VALUES (1, ?, ?, ?, ?, ?, 'active')")
        .bind(&registration.server_origin)
        .bind(&registration.workspace_id)
        .bind(&registration.workspace_epoch)
        .bind(&registration.device_id)
        .bind(&registration.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO registered_documents VALUES (1, ?, 'fixture-storage')")
        .bind(FILE)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO draft_checkpoints VALUES ('fixture-storage', 1, 1, X'656469746564'); INSERT INTO draft_bases VALUES ('fixture-storage', X'62617365', 'content-1'); INSERT INTO outbox_streams VALUES (1, 2, 0, 0);")
        .execute(&mut db).await.unwrap();
    let operation_id = Uuid::now_v7().to_string();
    let hash = format!(
        "sha256:{}",
        Sha256::digest(b"edited")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_hash, status, payload_len) VALUES (?, 1, 1, ?, 'fixture-storage', 1, 'content-1', ?, ?, 'queued', ?)")
        .bind(&operation_id).bind(FILE).bind(b"edited".as_slice()).bind(&hash).bind(b"edited".len() as i64).execute(&mut db).await.unwrap();
    let queued = OutboxOperation {
        binding: registration.clone(),
        operation_id,
        operation_seq: 1,
        document_id: FILE.into(),
        generation: 1,
        expected_revision: Some("content-1".into()),
        conflict_revision: None,
        bytes: b"edited".to_vec(),
        payload_hash: hash,
        status: OutboxStatus::Queued,
        frozen: false,
        failure_kind: None,
        abandon_state: OutboxAbandonState::None,
    };
    sqlx::query("INSERT INTO remote_index VALUES (1, ?, '1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for item in [root(1), node.clone()] {
        sqlx::query("INSERT INTO remote_nodes VALUES (1, ?, ?, 0)")
            .bind(item.node_id.to_string())
            .bind(serde_json::to_string(&item).unwrap())
            .execute(&mut db)
            .await
            .unwrap();
    }
    sqlx::query("CREATE TABLE cached_content (synthetic_obstruction TEXT)")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));

    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 6);
    let checkpoint: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM draft_checkpoints")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let base: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM draft_bases")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(checkpoint, b"edited");
    assert_eq!(base, b"base");
    db.close().await.unwrap();

    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let drafts = recovery.drafts(None).await.unwrap();
    assert_eq!(drafts.items.len(), 1);
    assert_eq!(
        recovery
            .draft(&drafts.items[0].storage_id)
            .await
            .unwrap()
            .bytes,
        b"edited"
    );
    let operations = recovery.operations(None).await.unwrap();
    assert_eq!(operations.items.len(), 1);
    assert_eq!(operations.items[0].operation_id, queued.operation_id);
    drop(recovery);

    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("DROP TABLE cached_content")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    let draft = reopened
        .read_registered_checkpoint(&registration, FILE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"edited");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(
        reopened
            .read_outbox_operation(&registration, &queued.operation_id)
            .await
            .unwrap()
            .unwrap(),
        queued
    );
    assert_eq!(
        reopened.cache_content(&registration, &node, b"base").await,
        Ok(true)
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn exact_25_mib_content_is_cached_and_reopened_byte_for_byte() {
    let original = vec![b'a'; 25 * 1024 * 1024];
    let (directory, mut store, registration, node) = seeded(&original).await;
    assert_eq!(
        store.cache_content(&registration, &node, &original).await,
        Ok(true)
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let cached = reopened
        .read_cached_content(&registration, node.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(cached.current);
    assert_eq!(cached.bytes, original);
    reopened.close().await.unwrap();
}

async fn install_index(
    store: &mut ClientStore,
    registration: &RegistrationBinding,
    node: &NodeState,
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
                nodes: vec![root(1), node.clone()],
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

#[tokio::test]
async fn matching_cache_is_reused_across_epoch_without_linking_old_draft_or_outbox() {
    use notryx_client_store::RecoveryStore;
    let (directory, mut store, old_registration, old_node) = seeded(b"base\r\n").await;
    store
        .cache_content(&old_registration, &old_node, b"base\r\n")
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&old_registration, FILE, b"base\r\n", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &old_registration,
            FILE,
            &opened.edit_session_id,
            1,
            b"private edit",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&old_registration, FILE, &opened.edit_session_id)
        .await
        .unwrap();
    let queued = store
        .queue_checkpoint(&old_registration, FILE, 1)
        .await
        .unwrap();

    let mut replacement = binding();
    replacement.workspace_id = old_registration.workspace_id.clone();
    let mut remote = file(1, false, b"base\r\n");
    remote.content_rev = Some("restored-revision".to_string().try_into().unwrap());
    remote.content_etag = Some("\"restored-revision\"".into());
    store.activate_registration(&replacement).await.unwrap();
    install_index(&mut store, &replacement, &remote).await;
    assert_eq!(
        store
            .reuse_cached_content(&replacement, remote.node_id)
            .await,
        Ok(true)
    );
    let cached = store
        .read_cached_content(&replacement, remote.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(cached.current);
    assert_eq!(cached.content_rev, "restored-revision");
    assert_eq!(cached.bytes, b"base\r\n");
    assert!(
        store
            .read_registered_checkpoint(&replacement, FILE)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .list_outbox(&replacement, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    store.close().await.unwrap();

    let mut recovery = RecoveryStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let drafts = recovery.drafts(None).await.unwrap();
    assert_eq!(drafts.items.len(), 1);
    let old_draft = recovery.draft(&drafts.items[0].storage_id).await.unwrap();
    assert_eq!(old_draft.bytes, b"private edit");
    assert_eq!(old_draft.base_bytes, b"base\r\n");
    let operations = recovery.operations(None).await.unwrap();
    assert_eq!(operations.items.len(), 1);
    assert_eq!(operations.items[0].operation_id, queued.operation_id);
}

#[tokio::test]
async fn cache_reuse_never_crosses_workspace_or_changes_current_content() {
    let (_directory, mut store, old_registration, old_node) = seeded(b"old").await;
    store
        .cache_content(&old_registration, &old_node, b"old")
        .await
        .unwrap();
    let other_workspace = binding();
    store.activate_registration(&other_workspace).await.unwrap();
    install_index(&mut store, &other_workspace, &old_node).await;
    assert_eq!(
        store
            .reuse_cached_content(&other_workspace, old_node.node_id)
            .await,
        Ok(false)
    );
    assert!(
        store
            .read_cached_content(&other_workspace, old_node.node_id)
            .await
            .unwrap()
            .is_none()
    );

    let mut replacement = binding();
    replacement.workspace_id = old_registration.workspace_id.clone();
    let different_content = file(1, false, b"new");
    store.activate_registration(&replacement).await.unwrap();
    install_index(&mut store, &replacement, &different_content).await;
    assert_eq!(
        store
            .reuse_cached_content(&replacement, different_content.node_id)
            .await,
        Ok(false)
    );
    assert!(
        store
            .read_cached_content(&replacement, different_content.node_id)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn corrupted_previous_cache_cannot_become_current_for_new_epoch() {
    let (directory, mut store, old_registration, old_node) = seeded(b"base").await;
    store
        .cache_content(&old_registration, &old_node, b"base")
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = old_registration.workspace_id.clone();
    store.activate_registration(&replacement).await.unwrap();
    install_index(&mut store, &replacement, &old_node).await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE cached_content SET bytes=x'00'")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        store
            .reuse_cached_content(&replacement, old_node.node_id)
            .await,
        Err(StoreError::UnsupportedFormat)
    );
    assert!(
        store
            .read_cached_content(&replacement, old_node.node_id)
            .await
            .unwrap()
            .is_none()
    );
    let current_ready: i64 = sqlx::query_scalar("SELECT content_ready FROM remote_nodes r JOIN client_registrations c ON c.id=r.registration_id WHERE c.status='active' AND r.node_id=?")
        .bind(FILE).fetch_one(&mut db).await.unwrap();
    assert_eq!(current_ready, 0);
    db.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn malformed_cached_revision_is_not_returned_or_reused_as_a_valid_base() {
    let (directory, mut store, old_registration, old_node) = seeded(b"base").await;
    store
        .cache_content(&old_registration, &old_node, b"base")
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE cached_content SET content_rev=?")
        .bind("bad\"revision")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        store
            .read_cached_content(&old_registration, old_node.node_id)
            .await,
        Err(StoreError::UnsupportedFormat)
    );
    let old_ready: i64 =
        sqlx::query_scalar("SELECT content_ready FROM remote_nodes WHERE node_id=?")
            .bind(FILE)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(old_ready, 0);
    let unchanged: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM cached_content")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(unchanged, b"base");

    let mut replacement = binding();
    replacement.workspace_id = old_registration.workspace_id.clone();
    store.activate_registration(&replacement).await.unwrap();
    install_index(&mut store, &replacement, &old_node).await;
    assert_eq!(
        store
            .reuse_cached_content(&replacement, old_node.node_id)
            .await,
        Err(StoreError::UnsupportedFormat)
    );
    assert!(
        store
            .read_cached_content(&replacement, old_node.node_id)
            .await
            .unwrap()
            .is_none()
    );
    db.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn current_remote_file_tracks_changes_and_excludes_tombstones() {
    let (_directory, mut store, registration, initial) = seeded(b"old").await;
    assert_eq!(
        store
            .current_remote_file(&registration, initial.node_id)
            .await
            .unwrap(),
        Some(initial.clone())
    );
    let newer = file(2, false, b"new");
    apply(&mut store, &registration, 1, vec![newer.clone()]).await;
    assert_eq!(
        store
            .current_remote_file(&registration, initial.node_id)
            .await
            .unwrap(),
        Some(newer)
    );
    let deleted = file(3, true, b"new");
    apply(&mut store, &registration, 2, vec![deleted]).await;
    assert_eq!(
        store
            .current_remote_file(&registration, initial.node_id)
            .await
            .unwrap(),
        None
    );
    store.close().await.unwrap();
}
