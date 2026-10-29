use notryx_client_store::{ClientStore, RecoveryOperationKind, RecoveryStore, RegistrationBinding};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

#[tokio::test]
async fn restore_operation_is_recoverable_without_a_draft_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drafts.db");
    let registration = RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    };
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    store.close().await.unwrap();
    let payload = br#"{"expected_meta_rev":"meta-1","parent_id":"019a1000-0000-7000-8000-000000000001","name":"restored.md"}"#;
    let digest = Sha256::digest(payload);
    let hash = format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id, registration_id, operation_seq, operation_kind, document_id, bytes, payload_hash, status, payload_len) VALUES ('restore-1', 1, 1, 'restore', '019a1000-0000-7000-8000-000000000002', ?, ?, 'queued', ?)")
        .bind(payload.as_slice()).bind(hash).bind(payload.len() as i64).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let page = recovery.operations(None).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].operation_id, "restore-1");
    assert_eq!(page.items[0].generation, None);
    assert_eq!(page.items[0].kind, Some(RecoveryOperationKind::Restore));
    assert!(!page.items[0].metadata_error);
    assert_eq!(
        recovery.operation_bytes("restore-1").await.unwrap(),
        payload
    );
}
