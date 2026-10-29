use notryx_client_store::{
    ClientStore, FrozenDeleteChoice, PendingOutboxOperation, RegistrationBinding,
};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const ETAG: &str = "\"node-1\"";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
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

async fn fixture(path: &Path, registration: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(registration).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(path).await;
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    let target = json!({"node_id":TARGET,"parent_id":ROOT,"name":"target.md","kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":"meta-1","meta_etag":"\"meta-1\"","node_etag":ETAG,"content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"1"});
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for (id, state) in [(ROOT, root), (TARGET, target)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(id).bind(state.to_string()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn frozen_fixture(path: &Path) -> (RegistrationBinding, RegistrationBinding, String) {
    let old = binding();
    fixture(path, &old).await;
    let mut store = ClientStore::open(path).await.unwrap();
    let item = store
        .queue_delete(&old, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    store.prepare_pending_outbox(&old).await.unwrap();
    let mut current = binding();
    current.workspace_id = old.workspace_id.clone();
    current.workspace_epoch = old.workspace_epoch.clone();
    store.activate_registration(&current).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(path).await;
    sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    (old, current, item.operation_id)
}

#[tokio::test]
async fn frozen_delete_requires_explicit_review_and_reissues_only_after_exact_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (old, current, operation_id) = frozen_fixture(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let page = store
        .list_frozen_deletes(&current, None, 100)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation.operation_id, operation_id);
    assert_eq!(page.entries[0].operation.expected_node_etag, ETAG);
    assert_eq!(page.entries[0].operation.binding, old);
    assert!(
        store
            .queue_delete(&current, TARGET.parse().unwrap(), ETAG)
            .await
            .is_err()
    );
    let stale = FrozenDeleteChoice::Apply {
        expected_node_etag: "\"stale\"".into(),
    };
    assert!(
        store
            .resolve_frozen_delete(&old, &current, &operation_id, &stale)
            .await
            .is_err()
    );
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    let choice = FrozenDeleteChoice::Apply {
        expected_node_etag: ETAG.into(),
    };
    store
        .resolve_frozen_delete(&old, &current, &operation_id, &choice)
        .await
        .unwrap();
    let Some(PendingOutboxOperation::Delete(new)) =
        store.next_pending_outbox(&current).await.unwrap()
    else {
        panic!("new delete")
    };
    assert_ne!(new.operation_id, operation_id);
    assert_eq!(new.operation_seq, 1);
    assert_eq!(new.expected_node_etag, ETAG);
    store
        .resolve_frozen_delete(&old, &current, &operation_id, &choice)
        .await
        .unwrap();
    assert!(
        matches!(store.next_pending_outbox(&current).await.unwrap(), Some(PendingOutboxOperation::Delete(item)) if item.operation_id == new.operation_id)
    );
    assert!(
        store
            .list_frozen_deletes(&current, None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let (status, sequence, etag): (String, i64, String) = sqlx::query_as(
        "SELECT status,operation_seq,expected_revision FROM client_outbox WHERE operation_id=?",
    )
    .bind(operation_id)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), sequence, etag.as_str()),
        ("sent", 1, ETAG)
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_delete_dismiss_preserves_original_and_permits_new_confirmed_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (old, current, operation_id) = frozen_fixture(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .resolve_frozen_delete(&old, &current, &operation_id, &FrozenDeleteChoice::Dismiss)
        .await
        .unwrap();
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    let item = store
        .queue_delete(&current, TARGET.parse().unwrap(), ETAG)
        .await
        .unwrap();
    assert_ne!(item.operation_id, operation_id);
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let status: String =
        sqlx::query_scalar("SELECT status FROM client_outbox WHERE operation_id=?")
            .bind(operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(status, "sent");
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_delete_apply_rolls_back_on_decision_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (old, current, operation_id) = frozen_fixture(&path).await;
    let mut db = database(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_delete_decision BEFORE UPDATE ON delete_reconciliations WHEN NEW.decision='applied' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let choice = FrozenDeleteChoice::Apply {
        expected_node_etag: ETAG.into(),
    };
    assert!(matches!(
        store
            .resolve_frozen_delete(&old, &current, &operation_id, &choice)
            .await,
        Err(notryx_client_store::StoreError::Storage)
    ));
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    assert_eq!(
        store
            .list_frozen_deletes(&current, None, 100)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delete_reconciliations")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let next: Option<i64> =
        sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=2")
            .fetch_optional(&mut db)
            .await
            .unwrap();
    assert!(next.is_none());
    let status: String =
        sqlx::query_scalar("SELECT status FROM client_outbox WHERE operation_id=?")
            .bind(operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(status, "sent");
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_delete_rejects_dirty_current_draft_and_foreign_binding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (old, current, operation_id) = frozen_fixture(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&current, TARGET, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&current, TARGET, &edit.edit_session_id, 1, b"new draft")
        .await
        .unwrap();
    let choice = FrozenDeleteChoice::Apply {
        expected_node_etag: ETAG.into(),
    };
    let foreign = binding();
    assert!(
        store
            .resolve_frozen_delete(&old, &foreign, &operation_id, &choice)
            .await
            .is_err()
    );
    assert!(
        store
            .resolve_frozen_delete(&old, &current, &operation_id, &choice)
            .await
            .is_err()
    );
    store
        .close_registered_document(&current, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    assert!(
        store
            .resolve_frozen_delete(&old, &current, &operation_id, &choice)
            .await
            .is_err()
    );
    let draft = store
        .read_registered_checkpoint(&current, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"new draft");
    assert_eq!(draft.base_bytes, b"base");
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    assert_eq!(
        store
            .list_frozen_deletes(&current, None, 100)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn frozen_delete_rejects_damaged_terminal_receipt_without_issuing_new_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (old, current, operation_id) = frozen_fixture(&path).await;
    let mut db = database(&path).await;
    sqlx::query("UPDATE client_outbox SET status='conflict',terminal_kind='node_precondition_failed',receipt_json='{}' WHERE operation_id=?").bind(&operation_id).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(
        store
            .list_frozen_deletes(&current, None, 100)
            .await
            .is_err()
    );
    let choice = FrozenDeleteChoice::Apply {
        expected_node_etag: ETAG.into(),
    };
    assert!(
        store
            .resolve_frozen_delete(&old, &current, &operation_id, &choice)
            .await
            .is_err()
    );
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let saved: String =
        sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(saved, "{}");
    db.close().await.unwrap();
}

async fn add_current_412(
    path: &Path,
    current: &RegistrationBinding,
    ack: i64,
    corrupt: bool,
) -> String {
    let id = "019a1000-0000-7000-8000-000000000010";
    let receipt = json!({"status":"node_precondition_failed","operation_id":if corrupt { "019a1000-0000-7000-8000-000000000011" } else { id },"operation_seq":"1","workspace_epoch":current.workspace_epoch,"current_node_etag":ETAG}).to_string();
    let mut db = database(path).await;
    sqlx::query("INSERT INTO outbox_streams VALUES (2,2,?,0)")
        .bind(ack)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,terminal_kind,payload_len,receipt_json) VALUES (?,2,1,'delete',?,'\"old-node\"',X'','sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','conflict','node_precondition_failed',0,?)")
        .bind(id).bind(TARGET).bind(receipt).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    id.to_owned()
}

#[tokio::test]
async fn frozen_delete_can_be_reissued_after_current_verified_412_and_new_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (old, current, operation_id) = frozen_fixture(&path).await;
    let previous = add_current_412(&path, &current, 1, false).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let choice = FrozenDeleteChoice::Apply {
        expected_node_etag: ETAG.into(),
    };
    store
        .resolve_frozen_delete(&old, &current, &operation_id, &choice)
        .await
        .unwrap();
    let Some(PendingOutboxOperation::Delete(next)) =
        store.next_pending_outbox(&current).await.unwrap()
    else {
        panic!("new request")
    };
    assert_eq!(next.operation_seq, 2);
    assert_eq!(next.expected_node_etag, ETAG);
    assert_ne!(next.operation_id, operation_id);
    store.close().await.unwrap();
    let mut db = database(&path).await;
    let (status, original_etag, receipt): (String, String, String) = sqlx::query_as(
        "SELECT status,expected_revision,receipt_json FROM client_outbox WHERE operation_id=?",
    )
    .bind(previous)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), original_etag.as_str()),
        ("conflict", "\"old-node\"")
    );
    assert!(receipt.contains("node_precondition_failed"));
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_delete_reissue_refuses_unacknowledged_or_damaged_current_412() {
    for (ack, corrupt) in [(0, false), (1, true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let (old, current, operation_id) = frozen_fixture(&path).await;
        let previous = add_current_412(&path, &current, ack, corrupt).await;
        let mut store = ClientStore::open(&path).await.unwrap();
        let choice = FrozenDeleteChoice::Apply {
            expected_node_etag: ETAG.into(),
        };
        assert!(
            store
                .resolve_frozen_delete(&old, &current, &operation_id, &choice)
                .await
                .is_err()
        );
        assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
        store.close().await.unwrap();
        let mut db = database(&path).await;
        let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delete_reconciliations")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(decisions, 0);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM client_outbox WHERE operation_id=? AND status='conflict'",
        )
        .bind(previous)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(count, 1);
        db.close().await.unwrap();
    }
}
