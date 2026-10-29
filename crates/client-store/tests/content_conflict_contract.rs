use notryx_client_store::{
    ClientStore, OutboxConflict, OutboxStatus, RegistrationBinding, StoreError,
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example.invalid".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

#[tokio::test]
async fn equal_current_revision_never_advances_ack_or_discards_content() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.sqlite3");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "revision-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            "doc",
            &opened.edit_session_id,
            1,
            b"sent payload",
        )
        .await
        .unwrap();
    store
        .queue_checkpoint(&registration, "doc", 1)
        .await
        .unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            "doc",
            &opened.edit_session_id,
            2,
            b"newer local draft",
        )
        .await
        .unwrap();
    let newer = store
        .queue_checkpoint(&registration, "doc", 2)
        .await
        .unwrap();
    let bad = OutboxConflict {
        operation_id: sent.operation_id.clone(),
        operation_seq: sent.operation_seq,
        workspace_epoch: registration.workspace_epoch.clone(),
        current_revision: "revision-a".into(),
        current_etag: "\"revision-a\"".into(),
    };
    assert_eq!(
        store.apply_outbox_conflict(&registration, &bad).await,
        Err(StoreError::InvalidReceipt)
    );
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    let unchanged = store
        .read_outbox_operation(&registration, &sent.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged, sent);
    assert_eq!(
        store
            .read_outbox_operation(&registration, &newer.operation_id)
            .await
            .unwrap()
            .unwrap(),
        newer
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            draft.bytes.as_slice(),
            draft.base_bytes.as_slice(),
            draft.base_revision.as_str()
        ),
        (
            b"newer local draft".as_slice(),
            b"base".as_slice(),
            "revision-a"
        )
    );
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    assert_eq!(
        store.prepare_outbox(&registration).await.unwrap().unwrap(),
        sent
    );
    let good = OutboxConflict {
        current_revision: "revision-b".into(),
        current_etag: "\"revision-b\"".into(),
        ..bad
    };
    store
        .apply_outbox_conflict(&registration, &good)
        .await
        .unwrap();
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert_eq!(
        store
            .read_outbox_operation(&registration, &sent.operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        OutboxStatus::Conflict
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn repeated_conflict_rejects_corrupted_equal_revision_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.sqlite3");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "revision-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    store
        .queue_checkpoint(&registration, "doc", 1)
        .await
        .unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    let valid = OutboxConflict {
        operation_id: sent.operation_id.clone(),
        operation_seq: sent.operation_seq,
        workspace_epoch: registration.workspace_epoch.clone(),
        current_revision: "revision-b".into(),
        current_etag: "\"revision-b\"".into(),
    };
    store
        .apply_outbox_conflict(&registration, &valid)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE client_outbox SET conflict_revision=?, conflict_etag=? WHERE operation_id=?",
    )
    .bind("revision-a")
    .bind("\"revision-a\"")
    .bind(&sent.operation_id)
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let corrupted = OutboxConflict {
        current_revision: "revision-a".into(),
        current_etag: "\"revision-a\"".into(),
        ..valid
    };
    assert_eq!(
        store.apply_outbox_conflict(&registration, &corrupted).await,
        Err(StoreError::InvalidReceipt)
    );
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "revision-a");
    store.close().await.unwrap();
}

#[tokio::test]
async fn damaged_sent_precondition_is_not_accepted_or_acknowledged() {
    for damaged in ["bad\"etag", ""] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drafts.sqlite3");
        let registration = binding();
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&registration).await.unwrap();
        let opened = store
            .open_registered_document(&registration, "doc", b"base", "revision-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"draft")
            .await
            .unwrap();
        store
            .queue_checkpoint(&registration, "doc", 1)
            .await
            .unwrap();
        let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::query("UPDATE client_outbox SET expected_revision=? WHERE operation_id=?")
            .bind(damaged)
            .bind(&sent.operation_id)
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        let receipt = OutboxConflict {
            operation_id: sent.operation_id.clone(),
            operation_seq: sent.operation_seq,
            workspace_epoch: registration.workspace_epoch.clone(),
            current_revision: "revision-b".into(),
            current_etag: "\"revision-b\"".into(),
        };
        assert_eq!(
            store.apply_outbox_conflict(&registration, &receipt).await,
            Err(StoreError::InvalidReceipt)
        );
        assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
        let draft = store
            .read_registered_checkpoint(&registration, "doc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"draft");
        assert_eq!(draft.base_bytes, b"base");
        assert_eq!(draft.base_revision, "revision-a");
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let row: (String, String, Vec<u8>) = sqlx::query_as(
            "SELECT status,expected_revision,bytes FROM client_outbox WHERE operation_id=?",
        )
        .bind(&sent.operation_id)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(row, ("sent".into(), damaged.into(), b"draft".to_vec()));
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_conflict_requires_locally_recorded_ack() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.sqlite3");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, "doc", b"base", "revision-a")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, "doc", &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    store
        .queue_checkpoint(&registration, "doc", 1)
        .await
        .unwrap();
    let sent = store.prepare_outbox(&registration).await.unwrap().unwrap();
    let receipt = OutboxConflict {
        operation_id: sent.operation_id.clone(),
        operation_seq: sent.operation_seq,
        workspace_epoch: registration.workspace_epoch.clone(),
        current_revision: "revision-b".into(),
        current_etag: "\"revision-b\"".into(),
    };
    store
        .apply_outbox_conflict(&registration, &receipt)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("UPDATE outbox_streams SET ack_through=0")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.apply_outbox_conflict(&registration, &receipt).await,
        Err(StoreError::InvalidReceipt)
    );
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 0);
    let draft = store
        .read_registered_checkpoint(&registration, "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "revision-a");
    store.close().await.unwrap();
}
