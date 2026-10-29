include!("conflict_copy_retry_native_tests.rs");

use sqlx::Connection;

#[tokio::test]
async fn retry_rejects_corrupt_previous_target_in_attempt_history() {
    for field in ["old_parent_id", "old_name", "old_node_id"] {
        let rig = rig_with_followup(true).await;
        let old = failed_copy(&rig).await;
        let old_id = old["create_operation_id"].as_str().unwrap();
        let request = retry_request(
            &rig,
            old["version"].as_str().unwrap(),
            old_id,
            "retry-copy.md",
        );
        let saved = retry_copy(&rig, request.clone()).await.unwrap();
        assert_eq!(retry_copy(&rig, request.clone()).await.unwrap(), saved);
        let path = rig._directory.path().join("conflict-snapshot.sqlite");
        let mut db = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
        )
        .await
        .unwrap();
        match field {
            "old_parent_id" => sqlx::query("UPDATE content_conflict_copy_attempts SET old_parent_id='wrong' WHERE old_create_operation_id=?").bind(old_id).execute(&mut db).await.unwrap(),
            "old_name" => sqlx::query("UPDATE content_conflict_copy_attempts SET old_name='wrong.md' WHERE old_create_operation_id=?").bind(old_id).execute(&mut db).await.unwrap(),
            "old_node_id" => sqlx::query("UPDATE content_conflict_copy_attempts SET old_node_id='wrong' WHERE old_create_operation_id=?").bind(old_id).execute(&mut db).await.unwrap(),
            _ => unreachable!(),
        };
        assert!(retry_copy(&rig, request).await.is_err(), "{field}");
        let mut state = rig.runtime.0.lock().await;
        let copy = state
            .store
            .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(copy.node_id, saved["node_id"]);
        assert_eq!(copy.create_operation_id, saved["create_operation_id"]);
        assert_eq!(copy.local_bytes, b"newer local\n");
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
        drop(state);
        db.close().await.unwrap();
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
            REMOTE
        );
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
