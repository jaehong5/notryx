use crate::conflict_snapshot_native::test_support::copy_retry::*;

#[tokio::test]
async fn failed_copy_requires_new_confirmation_and_retries_with_new_ids_without_losing_failure() {
    let rig = rig_with_followup(true).await;
    let old = failed_copy(&rig).await;
    let old_create = old["create_operation_id"].as_str().unwrap();
    let version = old["version"].as_str().unwrap();
    let request = retry_request(&rig, version, old_create, "retry-copy.md");
    let result = retry_copy(&rig, request.clone()).await.unwrap();
    assert_eq!(result["status"], "pending");
    assert_eq!(result["version"], version);
    assert_ne!(result["node_id"], old["node_id"]);
    assert_ne!(result["create_operation_id"], old["create_operation_id"]);
    assert_eq!(retry_copy(&rig, request).await.unwrap(), result);
    let mut state = rig.runtime.0.lock().await;
    let entries = state
        .store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation.operation_id, old_create);
    assert_eq!(
        entries[0].operation.status,
        notryx_client_store::OutboxStatus::Failed
    );
    assert!(matches!(
        entries[0].receipt,
        Some(OperationReceipt::DestinationExists { .. })
    ));
    let copy = state
        .store
        .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.node_id, result["node_id"]);
    assert_eq!(copy.create_operation_id, result["create_operation_id"]);
    assert_eq!(copy.name, "retry-copy.md");
    assert_eq!(copy.local_bytes, b"newer local\n");
    let original = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.bytes, b"newer local\n");
    assert_eq!(original.base_bytes, BASE);
    drop(state);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    assert!(
        !rig.config
            .workspace
            .notes_root
            .join("retry-copy.md")
            .exists()
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_retry_rejects_unfinished_or_wrong_operation_without_losing_confirmation() {
    let rig = rig_with_followup(true).await;
    let original = confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    let version = original["version"].as_str().unwrap();
    let id = original["create_operation_id"].as_str().unwrap();
    assert!(
        retry_copy(&rig, retry_request(&rig, version, id, "retry-copy.md"))
            .await
            .is_err()
    );
    assert!(
        retry_copy(
            &rig,
            retry_request(&rig, version, &Uuid::now_v7().to_string(), "retry-copy.md")
        )
        .await
        .is_err()
    );
    assert_copy_confirm_preserves_sources(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_copy_reconfirmation_rolls_back_new_ids_and_keeps_failure_receipt() {
    use sqlx::Connection;
    let rig = rig_with_followup(true).await;
    let old = failed_copy(&rig).await;
    let version = old["version"].as_str().unwrap();
    let id = old["create_operation_id"].as_str().unwrap();
    let request = retry_request(&rig, version, id, "retry-copy.md");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(rig._directory.path().join("conflict-snapshot.sqlite")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_copy_reconfirmation BEFORE UPDATE OF node_id ON content_conflict_copies BEGIN SELECT RAISE(ABORT,'injected'); END").execute(&mut db).await.unwrap();
    assert!(retry_copy(&rig, request.clone()).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    let copy = state
        .store
        .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.node_id, old["node_id"]);
    assert_eq!(copy.create_operation_id, id);
    let entries = state
        .store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        entries[0].receipt,
        Some(OperationReceipt::DestinationExists { .. })
    ));
    assert_eq!(copy.local_bytes, b"newer local\n");
    drop(state);
    sqlx::query("DROP TRIGGER fail_copy_reconfirmation")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert_eq!(
        retry_copy(&rig, request).await.unwrap()["status"],
        "pending"
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    assert!(
        !rig.config
            .workspace
            .notes_root
            .join("retry-copy.md")
            .exists()
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
