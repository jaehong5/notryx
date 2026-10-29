use notryx_client_store::{ClientStore, RegistrationBinding};
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FILE_A: &str = "019a1000-0000-7000-8000-000000000002";
const FILE_B: &str = "019a1000-0000-7000-8000-000000000003";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn info(binding: &RegistrationBinding, head: u64, floor: u64) -> ServerInfo {
    serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2",
        "workspace_id": binding.workspace_id,
        "workspace_epoch": binding.workspace_epoch,
        "root_id": ROOT,
        "head_seq": head.to_string(),
        "journal_floor_seq": floor.to_string(),
        "ready": true,
        "max_document_bytes": 5242880,
        "max_attachment_bytes": 26214400
    }))
    .unwrap()
}

fn root(seq: u64) -> NodeState {
    serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder",
        "deleted": false, "content_rev": null, "content_etag": null,
        "meta_rev": "root-meta", "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"",
        "content_hash": null, "size": null,
        "children_generation": seq.to_string(), "last_change_seq": seq.to_string()
    }))
    .unwrap()
}

fn file(id: &str, name: &str, seq: u64, deleted: bool) -> NodeState {
    serde_json::from_value(json!({
        "node_id": id, "parent_id": ROOT, "name": name, "kind": "file", "deleted": deleted,
        "content_rev": format!("content-{seq}"), "content_etag": format!("\"content-{seq}\""),
        "meta_rev": format!("meta-{seq}"), "meta_etag": format!("\"meta-{seq}\""),
        "node_etag": format!("\"node-{seq}\""),
        "content_hash": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "size": "0", "children_generation": "0", "last_change_seq": seq.to_string()
    }))
    .unwrap()
}

fn inventory(binding: &RegistrationBinding, nodes: Vec<NodeState>) -> InventoryPage {
    InventoryPage {
        workspace_epoch: info(binding, 0, 0).workspace_epoch,
        nodes,
        next_after_id: None,
    }
}

fn changes(
    binding: &RegistrationBinding,
    from: u64,
    through: u64,
    covered: u64,
    nodes: Vec<NodeState>,
) -> ChangesPage {
    serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "from_exclusive": from.to_string(), "through_seq": through.to_string(),
        "covered_through": covered.to_string(), "has_more": covered < through,
        "nodes": nodes
    }))
    .unwrap()
}

#[tokio::test]
async fn bootstrap_replays_overlapping_inventory_and_preserves_draft_and_outbox() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, FILE_A, b"base", "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            FILE_A,
            &opened.edit_session_id,
            1,
            b"local draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, FILE_A, &opened.edit_session_id)
        .await
        .unwrap();
    let operation = store
        .queue_checkpoint(&registration, FILE_A, 1)
        .await
        .unwrap();

    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(
                &registration,
                vec![
                    root(2),
                    file(FILE_A, "a.md", 1, false),
                    file(FILE_B, "b.md", 2, false),
                ],
            ),
        )
        .await
        .unwrap();
    assert!(store.sync_index(&registration).await.unwrap().is_none());
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
        .await
        .unwrap();
    store
        .bootstrap_changes(
            &registration,
            &session,
            &changes(
                &registration,
                0,
                2,
                2,
                vec![
                    root(1),
                    file(FILE_A, "a.md", 1, false),
                    root(2),
                    file(FILE_A, "a.md", 2, true),
                    file(FILE_B, "b.md", 2, false),
                ],
            ),
        )
        .await
        .unwrap();
    assert!(store.sync_index(&registration).await.unwrap().is_none());
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 2, 0))
        .await
        .unwrap();
    let (cursor, nodes) = store.sync_index(&registration).await.unwrap().unwrap();
    assert_eq!(cursor.get(), 2);
    assert_eq!(nodes.len(), 3);
    assert_eq!(
        nodes
            .iter()
            .find(|node| node.node_id.to_string() == FILE_A)
            .unwrap(),
        &file(FILE_A, "a.md", 2, true)
    );
    assert_eq!(
        nodes
            .iter()
            .find(|node| node.node_id.to_string() == FILE_B)
            .unwrap(),
        &file(FILE_B, "b.md", 2, false)
    );
    let draft = store
        .read_registered_checkpoint(&registration, FILE_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "base-rev");
    assert_eq!(
        store
            .read_outbox_operation(&registration, &operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        operation
    );
    store.close().await.unwrap();

    let mut reopened = ClientStore::open(&path).await.unwrap();
    let (cursor, nodes) = reopened.sync_index(&registration).await.unwrap().unwrap();
    assert_eq!(cursor.get(), 2);
    assert_eq!(nodes.len(), 3);
    let draft = reopened
        .read_registered_checkpoint(&registration, FILE_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft");
    assert_eq!(draft.base_bytes, b"base");
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

async fn seed_root_index(store: &mut ClientStore, registration: &RegistrationBinding) {
    let session = store
        .begin_bootstrap(registration, &info(registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            registration,
            &session,
            &inventory(registration, vec![root(0)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(registration, &session, &info(registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(registration, &session, &info(registration, 0, 0))
        .await
        .unwrap();
}

#[tokio::test]
async fn bootstrap_rejects_invalid_graph_conflicting_post_state_and_expired_range() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    seed_root_index(&mut store, &registration).await;
    let original = store.sync_index(&registration).await.unwrap().unwrap();

    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    let mut invalid_child = file(FILE_B, "child.md", 1, false);
    invalid_child.parent_id = Some(file(FILE_A, "parent.md", 1, false).node_id);
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(
                &registration,
                vec![root(1), file(FILE_A, "parent.md", 1, false), invalid_child],
            ),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    assert!(
        store
            .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
            .await
            .is_err()
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );

    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(0), file(FILE_A, "a.md", 1, false)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let conflicting = file(FILE_A, "different.md", 1, false);
    assert!(
        store
            .bootstrap_changes(
                &registration,
                &session,
                &changes(&registration, 0, 1, 1, vec![root(1), conflicting])
            )
            .await
            .is_err()
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );

    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(0)]),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 1))
            .await,
        Err(notryx_client_store::StoreError::BootstrapHistoryExpired)
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        reopened.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn bootstrap_rejects_late_page_after_registration_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    seed_root_index(&mut store, &registration).await;
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(0)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let replacement = binding();
    store.activate_registration(&replacement).await.unwrap();
    assert!(
        store
            .bootstrap_changes(
                &registration,
                &session,
                &changes(&registration, 0, 1, 1, vec![root(1)])
            )
            .await
            .is_err()
    );
    assert!(
        store
            .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
            .await
            .is_err()
    );
    assert!(store.sync_index(&replacement).await.unwrap().is_none());
    store.close().await.unwrap();
}

#[tokio::test]
async fn failed_publish_rolls_back_index_and_cursor() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    seed_root_index(&mut store, &registration).await;
    let original = store.sync_index(&registration).await.unwrap().unwrap();

    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(1), file(FILE_A, "a.md", 1, false)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_remote_publish BEFORE INSERT ON remote_nodes BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(
        store
            .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
            .await
            .is_err()
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
    store.close().await.unwrap();

    let mut reopened = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        reopened.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn v5_migration_failure_preserves_draft_outbox_and_recovery_access() {
    use notryx_client_store::{OutboxAbandonState, OutboxOperation, OutboxStatus, RecoveryStore};
    use sha2::{Digest, Sha256};
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    let v7 = include_str!("fixtures/client_store_v7.sql");
    let v5 = v7.split("CREATE TABLE sync_bootstrap").next().unwrap();
    sqlx::raw_sql(v5).execute(&mut db).await.unwrap();
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
        .bind(FILE_A)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO draft_checkpoints VALUES ('fixture-storage', 1, 1, X'6c6f63616c206472616674'); INSERT INTO draft_bases VALUES ('fixture-storage', X'62617365', 'base-rev'); INSERT INTO outbox_streams VALUES (1, 2, 0, 0);")
        .execute(&mut db).await.unwrap();
    let operation_id = Uuid::now_v7().to_string();
    let digest = Sha256::digest(b"local draft");
    let hash = format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, bytes, payload_hash, status, payload_len) VALUES (?, 1, 1, ?, 'fixture-storage', 1, 'base-rev', ?, ?, 'queued', ?)")
        .bind(&operation_id).bind(FILE_A).bind(b"local draft".as_slice()).bind(&hash).bind(b"local draft".len() as i64).execute(&mut db).await.unwrap();
    let operation = OutboxOperation {
        binding: registration.clone(),
        operation_id,
        operation_seq: 1,
        document_id: FILE_A.into(),
        generation: 1,
        expected_revision: Some("base-rev".into()),
        conflict_revision: None,
        bytes: b"local draft".to_vec(),
        payload_hash: hash,
        status: OutboxStatus::Queued,
        frozen: false,
        failure_kind: None,
        abandon_state: OutboxAbandonState::None,
    };
    sqlx::query("CREATE TABLE remote_nodes (synthetic_obstruction TEXT)")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    assert!(ClientStore::open(&path).await.is_err());
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 5);
    let checkpoint: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM draft_checkpoints")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let base: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM draft_bases")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(checkpoint, b"local draft");
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
        b"local draft"
    );
    let operations = recovery.operations(None).await.unwrap();
    assert_eq!(operations.items.len(), 1);
    assert_eq!(operations.items[0].operation_id, operation.operation_id);
    drop(recovery);

    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("DROP TABLE remote_nodes")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let saved = store
        .read_registered_checkpoint(&registration, FILE_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bytes, b"local draft");
    assert_eq!(saved.base_bytes, b"base");
    assert_eq!(
        store
            .read_outbox_operation(&registration, &operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        operation
    );
    store.close().await.unwrap();

    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(recovery.drafts(None).await.unwrap().items.len(), 1);
    assert_eq!(recovery.operations(None).await.unwrap().items.len(), 1);
}

#[tokio::test]
async fn bootstrap_does_not_publish_if_floor_advances_past_h0_after_replay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    seed_root_index(&mut store, &registration).await;
    let original = store.sync_index(&registration).await.unwrap().unwrap();

    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(0)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_changes(
            &registration,
            &session,
            &changes(&registration, 0, 1, 1, vec![root(1)]),
        )
        .await
        .unwrap();
    let mut different_root = info(&registration, 1, 1);
    different_root.root_id = FILE_A.parse().unwrap();
    assert_eq!(
        store
            .bootstrap_publish(&registration, &session, &different_root)
            .await,
        Err(notryx_client_store::StoreError::InvalidRequest)
    );
    assert_eq!(
        store
            .bootstrap_publish(&registration, &session, &info(&registration, 1, 1))
            .await,
        Err(notryx_client_store::StoreError::BootstrapHistoryExpired)
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn bootstrap_rejects_node_kind_changes_in_both_observation_orders() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    seed_root_index(&mut store, &registration).await;
    let original = store.sync_index(&registration).await.unwrap().unwrap();
    for inventory_seq in [1, 2] {
        let session = store
            .begin_bootstrap(&registration, &info(&registration, 0, 0))
            .await
            .unwrap();
        let file_node = file(FILE_A, "entry", inventory_seq, false);
        let mut folder_json = serde_json::to_value(&file_node).unwrap();
        folder_json["kind"] = json!("folder");
        folder_json["content_rev"] = json!(null);
        folder_json["content_etag"] = json!(null);
        folder_json["content_hash"] = json!(null);
        folder_json["size"] = json!(null);
        let folder_node: NodeState = serde_json::from_value(folder_json).unwrap();
        let inventory_node = if inventory_seq == 1 {
            file_node
        } else {
            folder_node
        };
        store
            .bootstrap_inventory(
                &registration,
                &session,
                &inventory(&registration, vec![root(0), inventory_node]),
            )
            .await
            .unwrap();
        store
            .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
            .await
            .unwrap();
        let mut folder_json = serde_json::to_value(file(FILE_A, "entry", 2, false)).unwrap();
        folder_json["kind"] = json!("folder");
        folder_json["content_rev"] = json!(null);
        folder_json["content_etag"] = json!(null);
        folder_json["content_hash"] = json!(null);
        folder_json["size"] = json!(null);
        let folder2: NodeState = serde_json::from_value(folder_json).unwrap();
        let page = changes(
            &registration,
            0,
            2,
            2,
            vec![root(1), file(FILE_A, "entry", 1, false), root(2), folder2],
        );
        assert!(
            store
                .bootstrap_changes(&registration, &session, &page)
                .await
                .is_err()
        );
        assert_eq!(
            store.sync_index(&registration).await.unwrap().unwrap(),
            original
        );
    }
}

#[tokio::test]
async fn bootstrap_rejects_head_behind_published_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(1)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let original = store.sync_index(&registration).await.unwrap().unwrap();
    assert_eq!(
        store
            .begin_bootstrap(&registration, &info(&registration, 0, 0))
            .await,
        Err(notryx_client_store::StoreError::ReconciliationRequired)
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
}

#[tokio::test]
async fn bootstrap_rejects_zero_sequence_tombstone_in_corrupt_staging() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    seed_root_index(&mut store, &registration).await;
    let original = store.sync_index(&registration).await.unwrap().unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(0)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 0, 0))
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("INSERT INTO bootstrap_nodes(registration_id,node_id,state) SELECT registration_id,?,? FROM sync_bootstrap")
        .bind(FILE_A).bind(serde_json::to_string(&file(FILE_A, "old.md", 0, true)).unwrap()).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(
        store
            .bootstrap_publish(&registration, &session, &info(&registration, 0, 0))
            .await
            .is_err()
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
}

#[tokio::test]
async fn bootstrap_rejects_kind_change_against_published_index() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(
                &registration,
                vec![root(1), file(FILE_A, "entry", 1, false)],
            ),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let original = store.sync_index(&registration).await.unwrap().unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 2, 0))
        .await
        .unwrap();
    let mut folder_json = serde_json::to_value(file(FILE_A, "entry", 2, false)).unwrap();
    folder_json["kind"] = json!("folder");
    folder_json["content_rev"] = json!(null);
    folder_json["content_etag"] = json!(null);
    folder_json["content_hash"] = json!(null);
    folder_json["size"] = json!(null);
    let folder: NodeState = serde_json::from_value(folder_json).unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(2), folder]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
        .await
        .unwrap();
    assert!(
        store
            .bootstrap_publish(&registration, &session, &info(&registration, 2, 0))
            .await
            .is_err()
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
}

#[tokio::test]
async fn bootstrap_rejects_changed_root_across_registrations_in_same_epoch() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    seed_root_index(&mut store, &registration).await;
    let original = store.sync_index(&registration).await.unwrap().unwrap();
    let mut changed = info(&registration, 0, 0);
    changed.root_id = "019a1000-0000-7000-8000-000000000099".parse().unwrap();
    assert!(
        store
            .begin_bootstrap(&registration, &changed)
            .await
            .is_err()
    );
    assert_eq!(
        store.sync_index(&registration).await.unwrap().unwrap(),
        original
    );
    let mut replacement = registration.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    let mut changed = info(&replacement, 0, 0);
    changed.root_id = "019a1000-0000-7000-8000-000000000099".parse().unwrap();
    assert!(store.begin_bootstrap(&replacement, &changed).await.is_err());
    assert!(store.sync_index(&replacement).await.unwrap().is_none());
}

#[tokio::test]
async fn bootstrap_rejects_head_behind_old_registration_in_same_epoch() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(1)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let mut replacement = registration.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    assert_eq!(
        store
            .begin_bootstrap(&replacement, &info(&replacement, 0, 0))
            .await,
        Err(notryx_client_store::StoreError::ReconciliationRequired)
    );
    assert!(store.sync_index(&replacement).await.unwrap().is_none());
}

#[tokio::test]
async fn bootstrap_checks_old_epoch_identity_after_origin_changes() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(1)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let mut replacement = registration.clone();
    replacement.server_origin = "https://new-origin.example".into();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    assert_eq!(
        store
            .begin_bootstrap(&replacement, &info(&replacement, 0, 0))
            .await,
        Err(notryx_client_store::StoreError::ReconciliationRequired)
    );
    assert!(store.sync_index(&replacement).await.unwrap().is_none());
}

#[tokio::test]
async fn bootstrap_rejects_conflicting_equal_revisions_in_either_order() {
    for violation in ["hash", "size", "deleted", "name"] {
        for inventory_first in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let registration = binding();
            let mut store = ClientStore::open(directory.path().join("drafts.db"))
                .await
                .unwrap();
            store.activate_registration(&registration).await.unwrap();
            seed_root_index(&mut store, &registration).await;
            let original = store.sync_index(&registration).await.unwrap().unwrap();
            let mut first = file(FILE_A, "a.md", 1, false);
            let mut second = file(FILE_A, "a.md", 2, false);
            match violation {
                "hash" => {
                    second.content_rev = first.content_rev.clone();
                    second.content_etag = first.content_etag.clone();
                    second.content_hash = Some(format!("sha256:{}", "a".repeat(64)));
                }
                "size" => {
                    second.content_rev = first.content_rev.clone();
                    second.content_etag = first.content_etag.clone();
                    second.size = Some("1".into());
                }
                "deleted" => {
                    second.content_rev = first.content_rev.clone();
                    second.content_etag = first.content_etag.clone();
                    if inventory_first {
                        second.deleted = true;
                    } else {
                        first.deleted = true;
                    }
                }
                "name" => {
                    second.meta_rev = first.meta_rev.clone();
                    second.meta_etag = first.meta_etag.clone();
                    second.name = "different.md".into();
                }
                _ => unreachable!(),
            }
            let session = store
                .begin_bootstrap(&registration, &info(&registration, 0, 0))
                .await
                .unwrap();
            let observed = if inventory_first {
                first.clone()
            } else {
                second.clone()
            };
            store
                .bootstrap_inventory(
                    &registration,
                    &session,
                    &inventory(&registration, vec![root(0), observed]),
                )
                .await
                .unwrap();
            store
                .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
                .await
                .unwrap();
            let page = changes(
                &registration,
                0,
                2,
                2,
                vec![root(1), first, root(2), second],
            );
            assert!(
                store
                    .bootstrap_changes(&registration, &session, &page)
                    .await
                    .is_err(),
                "{violation}, inventory_first={inventory_first}"
            );
            assert_eq!(
                store.sync_index(&registration).await.unwrap().unwrap(),
                original
            );
        }
    }
}

#[tokio::test]
async fn bootstrap_rejects_equal_revision_conflict_against_published_index() {
    for violation in ["hash", "name", "deleted"] {
        let directory = tempfile::tempdir().unwrap();
        let registration = binding();
        let mut store = ClientStore::open(directory.path().join("drafts.db"))
            .await
            .unwrap();
        store.activate_registration(&registration).await.unwrap();
        let first = file(FILE_A, "a.md", 1, false);
        let session = store
            .begin_bootstrap(&registration, &info(&registration, 1, 0))
            .await
            .unwrap();
        store
            .bootstrap_inventory(
                &registration,
                &session,
                &inventory(&registration, vec![root(1), first.clone()]),
            )
            .await
            .unwrap();
        store
            .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
            .await
            .unwrap();
        store
            .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
            .await
            .unwrap();
        let original = store.sync_index(&registration).await.unwrap().unwrap();
        let mut second = file(FILE_A, "a.md", 2, false);
        match violation {
            "hash" => {
                second.content_rev = first.content_rev.clone();
                second.content_etag = first.content_etag.clone();
                second.content_hash = Some(format!("sha256:{}", "a".repeat(64)));
            }
            "name" => {
                second.meta_rev = first.meta_rev.clone();
                second.meta_etag = first.meta_etag.clone();
                second.name = "different.md".into();
            }
            "deleted" => {
                second.content_rev = first.content_rev.clone();
                second.content_etag = first.content_etag.clone();
                second.deleted = true;
            }
            _ => unreachable!(),
        }
        let session = store
            .begin_bootstrap(&registration, &info(&registration, 1, 0))
            .await
            .unwrap();
        let inventory_nodes = if violation == "deleted" {
            vec![root(1)]
        } else {
            vec![root(1), second.clone()]
        };
        store
            .bootstrap_inventory(
                &registration,
                &session,
                &inventory(&registration, inventory_nodes),
            )
            .await
            .unwrap();
        store
            .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
            .await
            .unwrap();
        store
            .bootstrap_changes(
                &registration,
                &session,
                &changes(&registration, 1, 2, 2, vec![root(2), second]),
            )
            .await
            .unwrap();
        assert!(
            store
                .bootstrap_publish(&registration, &session, &info(&registration, 2, 0))
                .await
                .is_err(),
            "{violation}"
        );
        assert_eq!(
            store.sync_index(&registration).await.unwrap().unwrap(),
            original
        );
    }
}

#[tokio::test]
async fn bootstrap_rejects_folder_generation_decrease_in_either_order() {
    for inventory_first in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let registration = binding();
        let mut store = ClientStore::open(directory.path().join("drafts.db"))
            .await
            .unwrap();
        store.activate_registration(&registration).await.unwrap();
        seed_root_index(&mut store, &registration).await;
        let original = store.sync_index(&registration).await.unwrap().unwrap();
        let mut first = root(1);
        let mut second = root(2);
        first.children_generation = notryx_domain::ChangeSeq::new(2);
        second.children_generation = notryx_domain::ChangeSeq::new(1);
        let session = store
            .begin_bootstrap(&registration, &info(&registration, 0, 0))
            .await
            .unwrap();
        let observed = if inventory_first {
            first.clone()
        } else {
            second.clone()
        };
        store
            .bootstrap_inventory(
                &registration,
                &session,
                &inventory(&registration, vec![observed]),
            )
            .await
            .unwrap();
        store
            .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
            .await
            .unwrap();
        assert!(
            store
                .bootstrap_changes(
                    &registration,
                    &session,
                    &changes(&registration, 0, 2, 2, vec![first, second])
                )
                .await
                .is_err()
        );
        assert_eq!(
            store.sync_index(&registration).await.unwrap().unwrap(),
            original
        );
    }
}

#[tokio::test]
async fn bootstrap_accepts_delete_restore_with_new_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    let restored = file(FILE_A, "a.md", 3, false);
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(3), restored.clone()]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 3, 0))
        .await
        .unwrap();
    let page = changes(
        &registration,
        0,
        3,
        3,
        vec![
            root(1),
            file(FILE_A, "a.md", 1, false),
            root(2),
            file(FILE_A, "a.md", 2, true),
            root(3),
            restored.clone(),
        ],
    );
    store
        .bootstrap_changes(&registration, &session, &page)
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 3, 0))
        .await
        .unwrap();
    let (cursor, nodes) = store.sync_index(&registration).await.unwrap().unwrap();
    assert_eq!(cursor.get(), 3);
    assert_eq!(
        nodes.iter().find(|n| n.node_id.to_string() == FILE_A),
        Some(&restored)
    );
}

#[tokio::test]
async fn bootstrap_accepts_unchanged_content_revision_when_name_changes() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let first = file(FILE_A, "a.md", 1, false);
    let mut renamed = file(FILE_A, "b.md", 2, false);
    renamed.content_rev = first.content_rev.clone();
    renamed.content_etag = first.content_etag.clone();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(2), renamed.clone()]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
        .await
        .unwrap();
    let page = changes(
        &registration,
        0,
        2,
        2,
        vec![root(1), first, root(2), renamed.clone()],
    );
    store
        .bootstrap_changes(&registration, &session, &page)
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 2, 0))
        .await
        .unwrap();
    let (_, nodes) = store.sync_index(&registration).await.unwrap().unwrap();
    assert_eq!(
        nodes.iter().find(|n| n.node_id.to_string() == FILE_A),
        Some(&renamed)
    );
}

#[tokio::test]
async fn bootstrap_accepts_origin_change_with_same_workspace_epoch_and_valid_history() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(1)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 1, 0))
        .await
        .unwrap();
    let mut replacement = registration.clone();
    replacement.server_origin = "https://new-origin.example".into();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    let session = store
        .begin_bootstrap(&replacement, &info(&replacement, 2, 0))
        .await
        .unwrap();
    store
        .bootstrap_inventory(
            &replacement,
            &session,
            &inventory(&replacement, vec![root(2)]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&replacement, &session, &info(&replacement, 2, 0))
        .await
        .unwrap();
    store
        .bootstrap_publish(&replacement, &session, &info(&replacement, 2, 0))
        .await
        .unwrap();
    let (cursor, nodes) = store.sync_index(&replacement).await.unwrap().unwrap();
    assert_eq!(cursor.get(), 2);
    assert_eq!(nodes, vec![root(2)]);
}

#[tokio::test]
async fn bootstrap_accepts_decimal_size_with_leading_zeroes_for_equal_values() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .begin_bootstrap(&registration, &info(&registration, 0, 0))
        .await
        .unwrap();
    let first = file(FILE_A, "a.md", 1, false);
    let mut second = file(FILE_A, "a.md", 2, false);
    second.content_rev = first.content_rev.clone();
    second.content_etag = first.content_etag.clone();
    let mut observed = second.clone();
    observed.size = Some("000".into());
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &inventory(&registration, vec![root(2), observed]),
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info(&registration, 2, 0))
        .await
        .unwrap();
    store
        .bootstrap_changes(
            &registration,
            &session,
            &changes(
                &registration,
                0,
                2,
                2,
                vec![root(1), first, root(2), second],
            ),
        )
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info(&registration, 2, 0))
        .await
        .unwrap();
    let (cursor, nodes) = store.sync_index(&registration).await.unwrap().unwrap();
    assert_eq!(cursor.get(), 2);
    assert_eq!(
        nodes
            .iter()
            .find(|n| n.node_id.to_string() == FILE_A)
            .unwrap()
            .size
            .as_deref(),
        Some("000")
    );
}
