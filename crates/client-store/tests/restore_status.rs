use notryx_client_store::{ClientStore, OutboxStatus, RegistrationBinding, StoreError};
use notryx_protocol::{OperationReceipt, RestoreNodeRequest};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn request() -> RestoreNodeRequest {
    serde_json::from_value(serde_json::json!({"expected_meta_rev":"meta-1", "parent_id":"019a1000-0000-7000-8000-000000000001", "name":"restored.md"})).unwrap()
}

#[tokio::test]
async fn restore_status_survives_receipt_restart_and_reregistration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let operation = store
        .queue_restore(
            &registration,
            "019a1000-0000-7000-8000-000000000002".parse().unwrap(),
            &request(),
        )
        .await
        .unwrap();
    let first = store
        .list_restore_operations(&registration, 0, 100)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.entries[0].operation, operation);
    assert!(first.entries[0].receipt.is_none());
    assert!(!first.entries[0].frozen);
    store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let receipt: OperationReceipt = serde_json::from_value(serde_json::json!({"status":"history_expired", "operation_id":operation.operation_id, "operation_seq":"1", "workspace_epoch":registration.workspace_epoch})).unwrap();
    store
        .apply_restore_receipt(&registration, &receipt)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let page = store
        .list_restore_operations(&registration, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.entries[0].operation.status, OutboxStatus::Failed);
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&receipt));
    let mut replacement = binding();
    replacement.workspace_id = registration.workspace_id.clone();
    store.activate_registration(&replacement).await.unwrap();
    let frozen = store
        .list_restore_operations(&registration, 0, 100)
        .await
        .unwrap();
    assert!(frozen.entries[0].frozen);
    assert_eq!(frozen.entries[0].receipt.as_ref(), Some(&receipt));
    assert!(
        store
            .list_restore_operations(&replacement, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn restore_status_uses_bounded_sequence_keysets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    for node in [
        "019a1000-0000-7000-8000-000000000002",
        "019a1000-0000-7000-8000-000000000003",
    ] {
        store
            .queue_restore(&registration, node.parse().unwrap(), &request())
            .await
            .unwrap();
    }
    assert!(matches!(
        store.list_restore_operations(&registration, 0, 0).await,
        Err(StoreError::InvalidRequest)
    ));
    assert!(matches!(
        store.list_restore_operations(&registration, 0, 101).await,
        Err(StoreError::InvalidRequest)
    ));
    let first = store
        .list_restore_operations(&registration, 0, 1)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.entries[0].operation.operation_seq, 1);
    assert_eq!(first.next_after_seq, Some(1));
    let second = store
        .list_restore_operations(&registration, 1, 1)
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].operation.operation_seq, 2);
    assert_eq!(second.next_after_seq, None);
    store.close().await.unwrap();
}

#[tokio::test]
async fn corrupted_restore_receipt_is_not_reported_as_a_valid_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let operation = store
        .queue_restore(
            &registration,
            "019a1000-0000-7000-8000-000000000002".parse().unwrap(),
            &request(),
        )
        .await
        .unwrap();
    store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap();
    let receipt: OperationReceipt = serde_json::from_value(serde_json::json!({"status":"history_expired", "operation_id":operation.operation_id, "operation_seq":"1", "workspace_epoch":registration.workspace_epoch})).unwrap();
    store
        .apply_restore_receipt(&registration, &receipt)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("UPDATE client_outbox SET receipt_json='{}' WHERE operation_seq=1")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store.list_restore_operations(&registration, 0, 100).await,
        Err(StoreError::UnsupportedFormat)
    ));
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    store.close().await.unwrap();
}
