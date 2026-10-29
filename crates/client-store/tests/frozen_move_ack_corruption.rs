include!("frozen_move.rs");

async fn failed_move_with_replaced_registration(
    path: &Path,
) -> (RegistrationBinding, RegistrationBinding, String) {
    let old = binding();
    fixture(path, &old).await;
    let mut store = ClientStore::open(path).await.unwrap();
    let edit = store
        .open_registered_document(&old, OTHER, b"other base", "rev-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, OTHER, &edit.edit_session_id, 1, b"other draft")
        .await
        .unwrap();
    store
        .close_registered_document(&old, OTHER, &edit.edit_session_id)
        .await
        .unwrap();
    let movement = store
        .queue_move(&old, TARGET.parse().unwrap(), META_ETAG, &move_request())
        .await
        .unwrap();
    store.prepare_pending_outbox(&old).await.unwrap();
    let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(json!({
        "status":"destination_exists", "operation_id":movement.operation_id,
        "operation_seq":"1", "workspace_epoch":old.workspace_epoch
    }))
    .unwrap();
    store.apply_move_receipt(&old, &receipt).await.unwrap();
    assert_eq!(store.receipt_ack_through(&old).await.unwrap(), 1);
    let mut current = binding();
    current.workspace_id = old.workspace_id.clone();
    current.workspace_epoch = old.workspace_epoch.clone();
    store.activate_registration(&current).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(path).await;
    sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    (old, current, movement.operation_id)
}

#[tokio::test]
async fn frozen_failed_move_list_rejects_receipt_ahead_of_local_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let (_old, current, id) = failed_move_with_replaced_registration(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let valid = store.list_frozen_moves(&current, None, 100).await.unwrap();
    assert_eq!(valid.entries[0].operation.operation_id, id);
    store.close().await.unwrap();
    let mut db = database(&path).await;
    sqlx::query("UPDATE outbox_streams SET ack_through=0 WHERE registration_id=1")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(store.list_frozen_moves(&current, None, 100).await.is_err());
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    store.close().await.unwrap();
}

#[tokio::test]
async fn frozen_failed_move_choice_rejects_receipt_ahead_of_local_ack_and_preserves_data() {
    for choice in [FrozenMoveChoice::Dismiss, apply_choice(META_ETAG)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let (old, current, id) = failed_move_with_replaced_registration(&path).await;
        let mut db = database(&path).await;
        let original: (Vec<u8>, String, String) = sqlx::query_as(
            "SELECT bytes,status,receipt_json FROM client_outbox WHERE operation_id=?",
        )
        .bind(&id)
        .fetch_one(&mut db)
        .await
        .unwrap();
        sqlx::query("UPDATE outbox_streams SET ack_through=0 WHERE registration_id=1")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert!(
            store
                .resolve_frozen_move(&old, &current, &id, &choice)
                .await
                .is_err()
        );
        assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
        let draft = store
            .read_registered_checkpoint(&old, OTHER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"other draft");
        assert_eq!(draft.base_bytes, b"other base");
        store.close().await.unwrap();
        let mut db = database(&path).await;
        let after: (Vec<u8>, String, String) = sqlx::query_as(
            "SELECT bytes,status,receipt_json FROM client_outbox WHERE operation_id=?",
        )
        .bind(&id)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(after, original);
        let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM move_reconciliations")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(decisions, 0);
        db.close().await.unwrap();
    }
}
