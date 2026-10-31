use crate::conflict_snapshot_native::test_support::copy_retry::*;

use sqlx::Connection;

#[tokio::test]
async fn retry_rejects_current_destination_that_disagrees_with_saved_attempt() {
    for field in ["parent", "name"] {
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
            "parent" => {
                sqlx::query("UPDATE content_conflict_copies SET parent_id=? WHERE operation_id=?")
                    .bind(&rig.document)
                    .bind(&rig.operation.operation_id)
                    .execute(&mut db)
                    .await
                    .unwrap()
            }
            "name" => sqlx::query(
                "UPDATE content_conflict_copies SET name='different.md' WHERE operation_id=?",
            )
            .bind(&rig.operation.operation_id)
            .execute(&mut db)
            .await
            .unwrap(),
            _ => unreachable!(),
        };
        assert!(retry_copy(&rig, request).await.is_err(), "{field}");
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"newer local\n");
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            LOCAL
        );
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
