use notryx_client_store::{
    ClientStore, OutboxStatus, PendingOutboxOperation, RegistrationBinding, StoreError,
};
use notryx_protocol::{CreateNodeRequest, RestoreNodeRequest};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const OLD_NODE: &str = "019a1000-0000-7000-8000-000000000002";
const INDEPENDENT: &str = "019a1000-0000-7000-8000-000000000003";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn replacement(original: &RegistrationBinding) -> RegistrationBinding {
    RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..original.clone()
    }
}
fn create(node: &str, name: &str) -> CreateNodeRequest {
    serde_json::from_value(json!({"node_id":node,"parent_id":ROOT,"name":name,"kind":"folder"}))
        .unwrap()
}
fn restore() -> RestoreNodeRequest {
    serde_json::from_value(
        json!({"expected_meta_rev":"old-meta","parent_id":ROOT,"name":"restored"}),
    )
    .unwrap()
}
async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}
async fn insert_root(path: &Path, binding: &RegistrationBinding) {
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    let mut conn = db(path).await;
    let registration_id: i64 = sqlx::query_scalar("SELECT id FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?")
        .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch).bind(&binding.device_id).bind(&binding.device_stream_id).fetch_one(&mut conn).await.unwrap();
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (?,?,'1')")
        .bind(registration_id)
        .bind(ROOT)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (?,?,?,1)",
    )
    .bind(registration_id)
    .bind(ROOT)
    .bind(root.to_string())
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}
async fn payload(path: &Path, operation_id: &str) -> (Vec<u8>, String, i64, String) {
    let mut conn = db(path).await;
    let result = sqlx::query_as(
        "SELECT bytes,payload_hash,operation_seq,status FROM client_outbox WHERE operation_id=?",
    )
    .bind(operation_id)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    result
}

#[tokio::test]
async fn frozen_create_blocks_same_workspace_node_until_review_without_losing_any_request() {
    for was_sent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite");
        let original = binding();
        let next = replacement(&original);
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&original).await.unwrap();
        insert_root(&path, &original).await;
        let queued = store
            .queue_create(&original, &create(OLD_NODE, "same name"))
            .await
            .unwrap();
        let expected = if was_sent {
            let PendingOutboxOperation::Create(sent) = store
                .prepare_pending_outbox(&original)
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("create must be first")
            };
            assert_eq!(sent.operation_id, queued.operation_id);
            sent
        } else {
            queued
        };
        let original_payload = payload(&path, &expected.operation_id).await;
        store.activate_registration(&next).await.unwrap();
        insert_root(&path, &next).await;

        assert!(matches!(
            store
                .queue_restore(&next, OLD_NODE.parse().unwrap(), &restore())
                .await,
            Err(StoreError::ReconciliationRequired)
        ));
        let editor = store
            .open_registered_document(&next, OLD_NODE, b"base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&next, OLD_NODE, &editor.edit_session_id, 1, b"local draft")
            .await
            .unwrap();
        assert!(matches!(
            store.queue_checkpoint(&next, OLD_NODE, 1).await,
            Err(StoreError::ReconciliationRequired)
        ));
        let draft = store
            .read_registered_checkpoint(&next, OLD_NODE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert_eq!(draft.base_bytes, b"base");
        assert!(
            store
                .queue_create(&next, &create(OLD_NODE, "same name"))
                .await
                .is_err()
        );

        let independent = store
            .queue_create(&next, &create(INDEPENDENT, "independent"))
            .await
            .unwrap();
        assert_eq!(independent.operation_seq, 1);
        let PendingOutboxOperation::Create(pending) =
            store.next_pending_outbox(&next).await.unwrap().unwrap()
        else {
            panic!("independent create must remain in new FIFO")
        };
        assert_eq!(pending, independent);
        let old = store
            .list_create_operations(&original, 0, 100)
            .await
            .unwrap();
        assert_eq!(old.entries.len(), 1);
        assert_eq!(old.entries[0].operation, expected);
        assert!(old.entries[0].frozen);
        assert!(old.entries[0].receipt.is_none());
        assert_eq!(
            payload(&path, &expected.operation_id).await,
            original_payload
        );
        assert!(matches!(
            store.next_pending_outbox(&original).await,
            Err(StoreError::StaleRegistration)
        ));
        store.close().await.unwrap();

        let mut store = ClientStore::open(&path).await.unwrap();
        let restored_old = store
            .list_create_operations(&original, 0, 100)
            .await
            .unwrap();
        assert_eq!(restored_old, old);
        assert_eq!(
            store.next_pending_outbox(&next).await.unwrap(),
            Some(PendingOutboxOperation::Create(independent))
        );
        assert_eq!(
            store
                .read_registered_checkpoint(&next, OLD_NODE)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"local draft"
        );
        assert_eq!(
            payload(&path, &expected.operation_id).await,
            original_payload
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn frozen_create_does_not_block_same_name_in_different_workspace() {
    for was_sent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite");
        let original = binding();
        let other = binding();
        assert_ne!(original.workspace_id, other.workspace_id);
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&original).await.unwrap();
        insert_root(&path, &original).await;
        let old = store
            .queue_create(&original, &create(OLD_NODE, "same name"))
            .await
            .unwrap();
        if was_sent {
            let PendingOutboxOperation::Create(sent) = store
                .prepare_pending_outbox(&original)
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("create must be first")
            };
            assert_eq!(sent.status, OutboxStatus::Sent);
        }
        let before = payload(&path, &old.operation_id).await;
        store.activate_registration(&other).await.unwrap();
        insert_root(&path, &other).await;
        let fresh = store
            .queue_create(&other, &create(INDEPENDENT, "same name"))
            .await
            .unwrap();
        assert_eq!(fresh.operation_seq, 1);
        assert_eq!(fresh.binding, other);
        assert_eq!(
            store.next_pending_outbox(&other).await.unwrap(),
            Some(PendingOutboxOperation::Create(fresh))
        );
        let editor = store
            .open_registered_document(&other, OLD_NODE, b"other base", "rev-b")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &other,
                OLD_NODE,
                &editor.edit_session_id,
                1,
                b"other draft",
            )
            .await
            .unwrap();
        let other_edit = store.queue_checkpoint(&other, OLD_NODE, 1).await.unwrap();
        assert_eq!(other_edit.operation_seq, 2);
        assert_eq!(other_edit.bytes, b"other draft");
        assert_eq!(payload(&path, &old.operation_id).await, before);
        assert!(
            store
                .list_create_operations(&original, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .frozen
        );
        store.close().await.unwrap();
    }
}
