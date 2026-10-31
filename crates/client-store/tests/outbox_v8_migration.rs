use notryx_client_store::{
    ClientStore, OutboxCommit, OutboxStatus, RecoveryStore, RegistrationBinding, StoreError,
};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: "00000000-0000-4000-8000-000000000001".into(),
        workspace_epoch: "00000000-0000-4000-8000-000000000002".into(),
        device_id: "00000000-0000-4000-8000-000000000003".into(),
        device_stream_id: "00000000-0000-4000-8000-000000000004".into(),
    }
}

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn fixture(path: &Path, obstruction: bool) {
    let mut db = connect(path).await;
    sqlx::raw_sql(include_str!("fixtures/client_store_v7.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    let registration = binding();
    sqlx::query("INSERT INTO client_registrations VALUES (1, ?, ?, ?, ?, ?, 'active')")
        .bind(&registration.server_origin)
        .bind(&registration.workspace_id)
        .bind(&registration.workspace_epoch)
        .bind(&registration.device_id)
        .bind(&registration.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO registered_documents VALUES (1, 'doc', 'storage'); INSERT INTO draft_checkpoints VALUES ('storage', 3, 1, X'6e65776572206472616674'); INSERT INTO draft_bases VALUES ('storage', X'62617365', 'rev-a'); INSERT INTO outbox_streams VALUES (1, 4, 1, 1);").execute(&mut db).await.unwrap();
    for (id, seq, bytes, status, revision, dependency) in [
        ("op-1", 1, &b"base"[..], "committed", Some("rev-a"), None),
        ("op-2", 2, &b"in flight"[..], "sent", Some("rev-a"), None),
        ("op-3", 3, &b"newer draft"[..], "queued", None, Some("op-2")),
    ] {
        let digest = Sha256::digest(bytes);
        let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, document_id, storage_id, generation, expected_revision, dependency, bytes, payload_hash, status, committed_revision, payload_len) VALUES (?, 1, ?, 'doc', 'storage', ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(id).bind(seq).bind(seq).bind(revision).bind(dependency).bind(bytes).bind(format!("sha256:{hash}")).bind(status).bind(if status == "committed" { Some("rev-a") } else { None }).bind(bytes.len() as i64).execute(&mut db).await.unwrap();
    }
    if obstruction {
        sqlx::query("CREATE TABLE client_outbox_v8 (obstruction TEXT)")
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

async fn snapshot(path: &Path) -> Vec<String> {
    let mut db = connect(path).await;
    let sql = "SELECT 'draft:' || quote(document_id) || ':' || generation || ':' || format_version || ':' || hex(bytes) FROM draft_checkpoints UNION ALL SELECT 'base:' || quote(document_id) || ':' || hex(bytes) || ':' || quote(revision) FROM draft_bases UNION ALL SELECT 'stream:' || registration_id || ':' || next_seq || ':' || ack_through || ':' || confirmed_through FROM outbox_streams UNION ALL SELECT 'outbox:' || quote(operation_id) || ':' || registration_id || ':' || operation_seq || ':' || quote(document_id) || ':' || quote(storage_id) || ':' || generation || ':' || quote(expected_revision) || ':' || quote(dependency) || ':' || hex(bytes) || ':' || quote(payload_hash) || ':' || quote(status) || ':' || quote(committed_revision) || ':' || quote(conflict_revision) || ':' || quote(conflict_etag) || ':' || quote(abandon_state) || ':' || quote(terminal_kind) || ':' || payload_retired || ':' || payload_len FROM client_outbox ORDER BY 1";
    let rows = sqlx::query_scalar(sql).fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

#[tokio::test]
async fn v7_migration_preserves_cache_and_remote_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    fixture(&path, false).await;
    let bytes = b"cached synthetic markdown\n";
    let digest = Sha256::digest(bytes);
    let hash = format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let node: notryx_protocol::NodeState = serde_json::from_value(serde_json::json!({
        "node_id": "019a1000-0000-7000-8000-000000000002", "parent_id": "019a1000-0000-7000-8000-000000000001",
        "name": "cached.md", "kind": "file", "deleted": false, "content_rev": "content-1",
        "content_etag": "\"content-1\"", "meta_rev": "meta-1", "meta_etag": "\"meta-1\"",
        "node_etag": "\"node-1\"", "content_hash": hash, "size": bytes.len().to_string(),
        "children_generation": "0", "last_change_seq": "1"
    })).unwrap();
    let mut db = connect(&path).await;
    sqlx::query("INSERT INTO remote_nodes VALUES (1, ?, ?, 1)")
        .bind(node.node_id.to_string())
        .bind(serde_json::to_string(&node).unwrap())
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO cached_content VALUES (1, ?, 'content-1', ?, ?, ?, 1)")
        .bind(node.node_id.to_string())
        .bind(&hash)
        .bind(bytes.len().to_string())
        .bind(bytes.as_slice())
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    let (ready, stored_bytes): (i64, Vec<u8>) = sqlx::query_as("SELECT r.content_ready, c.bytes FROM remote_nodes r JOIN cached_content c ON r.registration_id=c.registration_id AND r.node_id=c.node_id WHERE r.node_id=?")
        .bind(node.node_id.to_string()).fetch_one(&mut db).await.unwrap();
    assert_eq!(ready, 1);
    assert_eq!(stored_bytes, bytes);
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let cached = store
        .read_cached_content(&binding(), node.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.bytes, bytes);
    assert_eq!(cached.content_rev, "content-1");
    assert!(cached.current);
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    let ready: i64 = sqlx::query_scalar("SELECT content_ready FROM remote_nodes WHERE node_id = ?")
        .bind(node.node_id.to_string())
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(ready, 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn v7_migration_preserves_conflict_abandon_and_retired_operations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    fixture(&path, false).await;
    let mut db = connect(&path).await;
    sqlx::raw_sql("UPDATE client_outbox SET status='conflict', terminal_kind='node_deleted', conflict_revision='revision', conflict_etag='etag' WHERE operation_id='op-2'; UPDATE client_outbox SET abandon_state='requested' WHERE operation_id='op-3'; UPDATE client_outbox SET bytes=X'', payload_retired=1 WHERE operation_id='op-1';").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&path).await;
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn v7_migration_preserves_pending_edits_dependencies_and_receipt_watermarks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    fixture(&path, false).await;
    let before = snapshot(&path).await;
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    let retry = store.next_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(
        (
            retry.operation_id.as_str(),
            retry.operation_seq,
            retry.status
        ),
        ("op-2", 2, OutboxStatus::Sent)
    );
    store.close().await.unwrap();
    assert_eq!(snapshot(&path).await, before);
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 24);
    let foreign_key_errors: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(foreign_key_errors, 0);
    let indexes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE type='index' AND name IN ('outbox_document_recent','outbox_document_status','outbox_dependencies')").fetch_one(&mut db).await.unwrap();
    assert_eq!(indexes, 3);
    db.close().await.unwrap();

    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    assert_eq!(recovery.operations(None).await.unwrap().items.len(), 3);
    drop(recovery);
    let mut store = ClientStore::open(&path).await.unwrap();
    let retry = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(retry.operation_id, "op-2");
    store
        .apply_outbox_commit(
            &registration,
            &OutboxCommit {
                operation_id: retry.operation_id.clone(),
                operation_seq: retry.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                document_id: retry.document_id.clone(),
                content_revision: "rev-b".into(),
                content_hash: retry.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 2);
    let next = store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert_eq!(next.operation_id, "op-3");
    assert_eq!(next.expected_revision.as_deref(), Some("rev-b"));
    assert_eq!(next.bytes, b"newer draft");
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer draft");
    assert_eq!(draft.base_bytes, b"in flight");
    store.close().await.unwrap();
}

#[tokio::test]
async fn failed_v7_migration_rolls_back_and_keeps_recovery_access() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    fixture(&path, true).await;
    let before = snapshot(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(snapshot(&path).await, before);
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 7);
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
        b"newer draft"
    );
    let operations = recovery.operations(None).await.unwrap();
    assert_eq!(operations.items.len(), 3);
}

#[tokio::test]
async fn v7_copy_failure_rolls_back_new_table_and_preserves_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    fixture(&path, false).await;
    let mut db = connect(&path).await;
    sqlx::raw_sql("PRAGMA ignore_check_constraints = ON; UPDATE client_outbox SET payload_len = -1 WHERE operation_id = 'op-1'; PRAGMA ignore_check_constraints = OFF;")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(snapshot(&path).await, before);
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 7);
    let temporary_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'client_outbox_v8'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(temporary_table, 0);
    db.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let drafts = recovery.drafts(None).await.unwrap();
    assert_eq!(
        recovery
            .draft(&drafts.items[0].storage_id)
            .await
            .unwrap()
            .bytes,
        b"newer draft"
    );
    assert_eq!(recovery.operations(None).await.unwrap().items.len(), 3);
}
