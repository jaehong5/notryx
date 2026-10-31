use crate::conflict_snapshot_native::test_support::copy_retry::*;

use sqlx::Connection;

async fn failure_status(rig: &Rig) -> serde_json::Value {
    let request = notryx_protocol::WorkspaceConflictResolutionRequest {
        renderer_session_id: OWNER.into(),
        binding: crate::to_contract(rig.binding.clone()),
        document_id: rig.document.clone(),
        operation_id: rig.operation.operation_id.clone(),
    };
    serde_json::to_value(
        crate::conflict_snapshot_native::copy_status_for_renderer(&rig.runtime, request)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn copy_status_reports_name_collision_only_from_verified_terminal_receipt() {
    let rig = rig_with_followup(true).await;
    let confirmed = failed_copy(&rig).await;
    let current = failure_status(&rig).await;
    assert_eq!(current["copy"]["create_status"], "needs_review");
    assert_eq!(current["copy"]["create_failure"], "destination_exists");
    assert_eq!(
        current["copy"]["create_operation_id"],
        confirmed["create_operation_id"]
    );
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
    )
    .await
    .unwrap();
    let operation = confirmed["create_operation_id"].as_str().unwrap();
    let raw: String =
        sqlx::query_scalar("SELECT receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(operation)
            .fetch_one(&mut db)
            .await
            .unwrap();
    let mut receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
    receipt["status"] = serde_json::json!("not_found");
    sqlx::query(
        "UPDATE client_outbox SET terminal_kind='not_found',receipt_json=? WHERE operation_id=?",
    )
    .bind(receipt.to_string())
    .bind(operation)
    .execute(&mut db)
    .await
    .unwrap();
    let changed = failure_status(&rig).await;
    assert_eq!(changed["copy"]["create_failure"], "other");
    sqlx::query("UPDATE client_outbox SET receipt_json=? WHERE operation_id=?")
        .bind(raw)
        .bind(operation)
        .execute(&mut db)
        .await
        .unwrap();
    let request = notryx_protocol::WorkspaceConflictResolutionRequest {
        renderer_session_id: OWNER.into(),
        binding: crate::to_contract(rig.binding.clone()),
        document_id: rig.document.clone(),
        operation_id: rig.operation.operation_id.clone(),
    };
    assert!(
        crate::conflict_snapshot_native::copy_status_for_renderer(&rig.runtime, request)
            .await
            .is_err()
    );
    db.close().await.unwrap();
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
