include!("frozen_move.rs");

type OutboxSnapshotRow = (String, i64, Vec<u8>, String, Option<String>);

#[tokio::test]
async fn frozen_terminal_move_and_later_move_can_be_dismissed_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let old = binding();
    fixture(&path, &old).await;
    let mut store = ClientStore::open(&path).await.unwrap();
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
    let first = store
        .queue_move(&old, TARGET.parse().unwrap(), META_ETAG, &move_request())
        .await
        .unwrap();
    store.prepare_pending_outbox(&old).await.unwrap();
    let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(json!({
        "status":"destination_exists", "operation_id":first.operation_id,
        "operation_seq":"1", "workspace_epoch":old.workspace_epoch
    }))
    .unwrap();
    store.apply_move_receipt(&old, &receipt).await.unwrap();
    let mut next_request = move_request();
    next_request.name = "other.md".into();
    let second = store
        .queue_move(&old, TARGET.parse().unwrap(), META_ETAG, &next_request)
        .await
        .unwrap();
    assert_eq!((first.operation_seq, second.operation_seq), (1, 2));
    let mut current = binding();
    current.workspace_id = old.workspace_id.clone();
    current.workspace_epoch = old.workspace_epoch.clone();
    store.activate_registration(&current).await.unwrap();
    store.close().await.unwrap();
    let mut db = database(&path).await;
    sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
    let original: Vec<OutboxSnapshotRow> = sqlx::query_as("SELECT operation_id,operation_seq,bytes,status,receipt_json FROM client_outbox WHERE registration_id=1 ORDER BY operation_seq").fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .list_frozen_moves(&current, None, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    assert!(matches!(
        store
            .resolve_frozen_move(
                &old,
                &current,
                &second.operation_id,
                &FrozenMoveChoice::Dismiss
            )
            .await,
        Err(StoreError::ReconciliationRequired)
    ));
    assert!(matches!(
        store
            .resolve_frozen_move(
                &old,
                &current,
                &first.operation_id,
                &apply_choice(META_ETAG)
            )
            .await,
        Err(StoreError::ReconciliationRequired)
    ));
    store
        .resolve_frozen_move(
            &old,
            &current,
            &first.operation_id,
            &FrozenMoveChoice::Dismiss,
        )
        .await
        .unwrap();
    store
        .resolve_frozen_move(
            &old,
            &current,
            &first.operation_id,
            &FrozenMoveChoice::Dismiss,
        )
        .await
        .unwrap();
    store
        .resolve_frozen_move(
            &old,
            &current,
            &second.operation_id,
            &FrozenMoveChoice::Dismiss,
        )
        .await
        .unwrap();
    store
        .resolve_frozen_move(
            &old,
            &current,
            &second.operation_id,
            &FrozenMoveChoice::Dismiss,
        )
        .await
        .unwrap();
    assert!(
        store
            .list_frozen_moves(&current, None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
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
    let after: Vec<OutboxSnapshotRow> = sqlx::query_as("SELECT operation_id,operation_seq,bytes,status,receipt_json FROM client_outbox WHERE registration_id=1 ORDER BY operation_seq").fetch_all(&mut db).await.unwrap();
    assert_eq!(after, original);
    let decisions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM move_reconciliations WHERE decision='dismissed'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(decisions, 2);
    db.close().await.unwrap();
}

#[tokio::test]
async fn resolved_frozen_delete_does_not_block_later_frozen_move() {
    for apply in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let old = binding();
        fixture(&path, &old).await;
        let mut store = ClientStore::open(&path).await.unwrap();
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
        let deletion = store
            .queue_delete(&old, TARGET.parse().unwrap(), "\"node-1\"")
            .await
            .unwrap();
        store.prepare_pending_outbox(&old).await.unwrap();
        let receipt: notryx_protocol::OperationReceipt = serde_json::from_value(json!({
            "status":"node_precondition_failed", "operation_id":deletion.operation_id,
            "operation_seq":"1", "workspace_epoch":old.workspace_epoch, "current_node_etag":"\"node-2\""
        })).unwrap();
        store.apply_delete_receipt(&old, &receipt).await.unwrap();
        let movement = store
            .queue_move(&old, TARGET.parse().unwrap(), META_ETAG, &move_request())
            .await
            .unwrap();
        let mut current = binding();
        current.workspace_id = old.workspace_id.clone();
        current.workspace_epoch = old.workspace_epoch.clone();
        store.activate_registration(&current).await.unwrap();
        store.close().await.unwrap();
        let mut db = database(&path).await;
        sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
        let original: Vec<OutboxSnapshotRow> = sqlx::query_as("SELECT operation_id,operation_seq,bytes,status,receipt_json FROM client_outbox WHERE registration_id=1 ORDER BY operation_seq").fetch_all(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        let choice = if apply {
            apply_choice(META_ETAG)
        } else {
            FrozenMoveChoice::Dismiss
        };
        assert!(matches!(
            store
                .resolve_frozen_move(&old, &current, &movement.operation_id, &choice)
                .await,
            Err(StoreError::ReconciliationRequired)
        ));
        store
            .resolve_frozen_delete(
                &old,
                &current,
                &deletion.operation_id,
                &notryx_client_store::FrozenDeleteChoice::Dismiss,
            )
            .await
            .unwrap();
        store
            .resolve_frozen_move(&old, &current, &movement.operation_id, &choice)
            .await
            .unwrap();
        if apply {
            let Some(PendingOutboxOperation::Move(new)) =
                store.next_pending_outbox(&current).await.unwrap()
            else {
                panic!("expected new move")
            };
            assert_ne!(new.operation_id, movement.operation_id);
            assert_eq!(new.request, move_request());
        } else {
            assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
        }
        let draft = store
            .read_registered_checkpoint(&old, OTHER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"other draft");
        assert_eq!(draft.base_bytes, b"other base");
        store.close().await.unwrap();
        let mut db = database(&path).await;
        let after: Vec<OutboxSnapshotRow> = sqlx::query_as("SELECT operation_id,operation_seq,bytes,status,receipt_json FROM client_outbox WHERE registration_id=1 ORDER BY operation_seq").fetch_all(&mut db).await.unwrap();
        assert_eq!(after, original);
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn resolved_frozen_create_does_not_block_later_frozen_move() {
    for apply in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let old = binding();
        fixture(&path, &old).await;
        let mut db = database(&path).await;
        let target_state: String = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=1 AND node_id=?",
        )
        .bind(TARGET)
        .fetch_one(&mut db)
        .await
        .unwrap();
        sqlx::query("DELETE FROM remote_nodes WHERE registration_id=1 AND node_id=?")
            .bind(TARGET)
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
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
        let create: notryx_protocol::CreateNodeRequest = serde_json::from_value(
            json!({"node_id":TARGET,"parent_id":ROOT,"name":"original.md","kind":"file"}),
        )
        .unwrap();
        let creation = store.queue_create(&old, &create).await.unwrap();
        store.prepare_pending_outbox(&old).await.unwrap();
        store.close().await.unwrap();
        let mut db = database(&path).await;
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)").bind(TARGET).bind(&target_state).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        let movement = store
            .queue_move(&old, TARGET.parse().unwrap(), META_ETAG, &move_request())
            .await
            .unwrap();
        let mut current = binding();
        current.workspace_id = old.workspace_id.clone();
        current.workspace_epoch = old.workspace_epoch.clone();
        store.activate_registration(&current).await.unwrap();
        store.close().await.unwrap();
        let mut db = database(&path).await;
        sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
        let original: Vec<OutboxSnapshotRow> = sqlx::query_as("SELECT operation_id,operation_seq,bytes,status,receipt_json FROM client_outbox WHERE registration_id=1 ORDER BY operation_seq").fetch_all(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        let choice = if apply {
            apply_choice(META_ETAG)
        } else {
            FrozenMoveChoice::Dismiss
        };
        assert!(matches!(
            store
                .resolve_frozen_move(&old, &current, &movement.operation_id, &choice)
                .await,
            Err(StoreError::ReconciliationRequired)
        ));
        store
            .resolve_frozen_create(
                &old,
                &current,
                &creation.operation_id,
                &notryx_client_store::FrozenCreateChoice::Dismiss,
            )
            .await
            .unwrap();
        store
            .resolve_frozen_move(&old, &current, &movement.operation_id, &choice)
            .await
            .unwrap();
        if apply {
            let Some(PendingOutboxOperation::Move(new)) =
                store.next_pending_outbox(&current).await.unwrap()
            else {
                panic!("expected new move")
            };
            assert_ne!(new.operation_id, movement.operation_id);
            assert_eq!(new.request, move_request());
        } else {
            assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
        }
        let draft = store
            .read_registered_checkpoint(&old, OTHER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"other draft");
        assert_eq!(draft.base_bytes, b"other base");
        store.close().await.unwrap();
        let mut db = database(&path).await;
        let after: Vec<OutboxSnapshotRow> = sqlx::query_as("SELECT operation_id,operation_seq,bytes,status,receipt_json FROM client_outbox WHERE registration_id=1 ORDER BY operation_seq").fetch_all(&mut db).await.unwrap();
        assert_eq!(after, original);
        db.close().await.unwrap();
    }
}
