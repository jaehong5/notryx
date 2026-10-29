include!("content_conflict_copy_intent.rs");

async fn queue_copy(
    store: &mut ClientStore,
    rig: &Rig,
    version: u64,
) -> Result<notryx_client_store::CreateOutboxOperation, StoreError> {
    store
        .queue_content_conflict_copy(&rig.binding, &rig.operation, version)
        .await
}

#[tokio::test]
async fn copy_create_uses_confirmed_ids_and_fifo_once_even_after_restart() {
    let (rig, mut store) = rig().await;
    let copy = record_copy(&mut store, &rig, &copy_request(&rig), ROOT, COPY_NAME)
        .await
        .unwrap();
    let before = next_sequence(&rig).await;
    let queued = queue_copy(&mut store, &rig, copy.intent_version)
        .await
        .unwrap();
    assert_eq!(queued.operation_id, copy.create_operation_id);
    assert_eq!(queued.operation_seq, before as u64);
    assert_eq!(queued.binding, rig.binding);
    assert_eq!(queued.request.node_id.to_string(), copy.node_id);
    assert_eq!(queued.request.parent_id.to_string(), ROOT);
    assert_eq!(queued.request.name, COPY_NAME);
    assert_eq!(queued.request.kind, notryx_protocol::NodeKind::File);
    assert!(queued.request.upload_id.is_none());
    assert_eq!(queued.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(
        queue_copy(&mut store, &rig, copy.intent_version)
            .await
            .unwrap(),
        queued
    );
    assert_eq!(next_sequence(&rig).await, before + 1);
    assert_eq!(
        store
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    let creates = store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].operation, queued);
    assert!(creates[0].receipt.is_none());
    assert_originals(&mut store, &rig).await;
    assert_eq!(
        store
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(copy.clone())
    );
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(
        queue_copy(&mut reopened, &rig, copy.intent_version)
            .await
            .unwrap(),
        queued
    );
    assert_eq!(next_sequence(&rig).await, before + 1);
    let creates = reopened
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].operation, queued);
    assert_originals(&mut reopened, &rig).await;
    assert_eq!(
        reopened
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(copy)
    );
}

#[tokio::test]
async fn copy_create_rejects_missing_confirmation_and_wrong_version() {
    let (rig, mut store) = rig().await;
    let before = next_sequence(&rig).await;
    assert!(queue_copy(&mut store, &rig, 0).await.is_err());
    let copy = record_copy(&mut store, &rig, &copy_request(&rig), ROOT, COPY_NAME)
        .await
        .unwrap();
    assert!(
        queue_copy(&mut store, &rig, copy.intent_version + 1)
            .await
            .is_err()
    );
    assert_eq!(next_sequence(&rig).await, before);
    assert_eq!(
        store
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    assert_originals(&mut store, &rig).await;
}

#[tokio::test]
async fn copy_create_insert_failure_rolls_back_sequence_and_keeps_confirmed_bytes() {
    let (rig, mut store) = rig().await;
    let copy = record_copy(&mut store, &rig, &copy_request(&rig), ROOT, COPY_NAME)
        .await
        .unwrap();
    let before = next_sequence(&rig).await;
    let mut db = connect(&rig.path).await;
    sqlx::query("CREATE TRIGGER fail_copy_create BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='create' BEGIN SELECT RAISE(ABORT, 'injected create failure'); END").execute(&mut db).await.unwrap();
    assert!(
        queue_copy(&mut store, &rig, copy.intent_version)
            .await
            .is_err()
    );
    assert_eq!(next_sequence(&rig).await, before);
    assert_eq!(
        store
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    assert_eq!(
        store
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(copy.clone())
    );
    assert_originals(&mut store, &rig).await;
    sqlx::query("DROP TRIGGER fail_copy_create")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    let queued = queue_copy(&mut reopened, &rig, copy.intent_version)
        .await
        .unwrap();
    assert_eq!(queued.operation_id, copy.create_operation_id);
    assert_eq!(queued.operation_seq, before as u64);
    assert_originals(&mut reopened, &rig).await;
}

#[tokio::test]
async fn copy_create_rejects_operation_id_collision_without_overwriting_existing_payload() {
    let (rig, mut store) = rig().await;
    let copy = record_copy(&mut store, &rig, &copy_request(&rig), ROOT, COPY_NAME)
        .await
        .unwrap();
    let before = next_sequence(&rig).await;
    let mut db = connect(&rig.path).await;
    sqlx::query("UPDATE content_conflict_copies SET create_operation_id=? WHERE operation_id=?")
        .bind(&rig.followup)
        .bind(&rig.operation)
        .execute(&mut db)
        .await
        .unwrap();
    assert!(
        queue_copy(&mut store, &rig, copy.intent_version)
            .await
            .is_err()
    );
    assert_eq!(next_sequence(&rig).await, before);
    assert_eq!(
        store
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    assert_originals(&mut store, &rig).await;
    db.close().await.unwrap();
}
