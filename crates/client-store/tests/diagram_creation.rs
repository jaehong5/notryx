use notryx_client_store::{
    ClientStore, CreateOutboxOperation, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::{CreateNodeRequest, OperationReceipt};
use serde_json::{Value, json};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const FOLDER: &str = "019a1000-0000-7000-8000-000000000002";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn request() -> CreateNodeRequest {
    serde_json::from_value(
        json!({"node_id":FOLDER,"parent_id":ROOT,"name":"new.drawio.png","kind":"file"}),
    )
    .unwrap()
}
fn created(b: &RegistrationBinding, op: &CreateOutboxOperation) -> Value {
    json!({"status":"created","operation_id":op.operation_id,"operation_seq":op.operation_seq.to_string(),"workspace_epoch":b.workspace_epoch,
        "node":{"node_id":FOLDER,"parent_id":ROOT,"name":"new.drawio.png","kind":"file","deleted":false,
            "content_rev":"content-9","content_etag":"\"content-9\"","meta_rev":"meta-9","meta_etag":"\"meta-9\"","node_etag":"\"node-9\"",
            "content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"9"},"commit_seq":"9"})
}
async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}
async fn fixture(path: &Path, b: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(b).await.unwrap();
    store.close().await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"7","last_change_seq":"7"});
    let mut conn = db(path).await;
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'7')")
        .bind(ROOT)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,1)",
    )
    .bind(ROOT)
    .bind(root.to_string())
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}

fn upload() -> notryx_protocol::UploadResponse {
    notryx_protocol::UploadResponse {
        upload_id: Uuid::new_v4().to_string(),
        content_hash: notryx_protocol::content_hash(b"synthetic PNG"),
        size: "13".into(),
        expires_at: "2000000000".into(),
    }
}
fn uploaded_request(upload: &notryx_protocol::UploadResponse) -> CreateNodeRequest {
    let mut value = request();
    value.upload_id = Some(upload.upload_id.parse().unwrap());
    value
}
fn uploaded_receipt(
    b: &RegistrationBinding,
    op: &CreateOutboxOperation,
    upload: &notryx_protocol::UploadResponse,
) -> OperationReceipt {
    let mut value = created(b, op);
    value["node"]["content_hash"] = json!(upload.content_hash);
    value["node"]["size"] = json!(upload.size);
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn diagram_creation_preserves_upload_identity_and_exact_receipt_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let upload = upload();
    let request = uploaded_request(&upload);
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_diagram_create(&b, &request, &upload)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Create(sent) =
        store.prepare_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("create")
    };
    assert_eq!(sent.request, queued.request);
    assert_eq!(sent.operation_id, queued.operation_id);
    let receipt = uploaded_receipt(&b, &sent, &upload);
    for field in ["content_hash", "size"] {
        let mut bad = serde_json::to_value(&receipt).unwrap();
        bad["node"][field] = if field == "size" {
            json!("14")
        } else {
            json!(notryx_protocol::content_hash(b"different PNG"))
        };
        assert_eq!(
            store
                .apply_create_receipt(&b, &serde_json::from_value(bad).unwrap())
                .await,
            Err(StoreError::InvalidReceipt)
        );
        assert_eq!(store.receipt_ack_through(&b).await.unwrap(), 0);
    }
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    let page = store.list_create_operations(&b, 0, 100).await.unwrap();
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&receipt));
    assert_eq!(page.entries[0].operation.status, OutboxStatus::Committed);
    assert_eq!(store.receipt_ack_through(&b).await.unwrap(), 1);
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    assert!(store.next_pending_outbox(&b).await.unwrap().is_none());
}

#[tokio::test]
async fn unavailable_initial_upload_abandons_same_request_without_allocating_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let upload = upload();
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .queue_diagram_create(&b, &uploaded_request(&upload), &upload)
        .await
        .unwrap();
    let PendingOutboxOperation::Create(sent) =
        store.prepare_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("create")
    };
    store
        .abandon_unavailable_diagram_upload(&b, &sent)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Create(abandoning) =
        store.prepare_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("create")
    };
    assert_eq!(abandoning.status, OutboxStatus::Abandoning);
    assert_eq!(abandoning.request, sent.request);
    assert_eq!(abandoning.operation_id, sent.operation_id);
    let receipt = serde_json::from_value(json!({"status":"abandoned", "operation_id":sent.operation_id, "operation_seq":"1", "workspace_epoch": b.workspace_epoch})).unwrap();
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    assert!(store.next_pending_outbox(&b).await.unwrap().is_none());
    assert_eq!(
        store
            .list_create_operations(&b, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
}

#[tokio::test]
async fn a_late_created_receipt_wins_over_upload_cleanup_and_old_registration_stays_frozen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let upload = upload();
    let mut store = ClientStore::open(&path).await.unwrap();
    store
        .queue_diagram_create(&b, &uploaded_request(&upload), &upload)
        .await
        .unwrap();
    let PendingOutboxOperation::Create(sent) =
        store.prepare_pending_outbox(&b).await.unwrap().unwrap()
    else {
        panic!("create")
    };
    store
        .abandon_unavailable_diagram_upload(&b, &sent)
        .await
        .unwrap();
    let receipt = uploaded_receipt(&b, &sent, &upload);
    store.apply_create_receipt(&b, &receipt).await.unwrap();
    let mut new_binding = b.clone();
    new_binding.device_id = Uuid::new_v4().to_string();
    new_binding.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&new_binding).await.unwrap();
    assert!(
        store
            .abandon_unavailable_diagram_upload(&b, &sent)
            .await
            .is_err()
    );
    let page = store.list_create_operations(&b, 0, 100).await.unwrap();
    assert!(page.entries[0].frozen);
    assert_eq!(page.entries[0].receipt.as_ref(), Some(&receipt));
}

#[tokio::test]
async fn failed_upload_metadata_insert_does_not_consume_an_operation_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let mut conn = db(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_diagram BEFORE INSERT ON diagram_creations BEGIN SELECT RAISE(ABORT, 'injected failure'); END;")
        .execute(&mut conn).await.unwrap();
    let upload = upload();
    let request = uploaded_request(&upload);
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(
        store
            .queue_diagram_create(&b, &request, &upload)
            .await
            .is_err()
    );
    assert!(store.next_pending_outbox(&b).await.unwrap().is_none());
    sqlx::query("DROP TRIGGER reject_diagram")
        .execute(&mut conn)
        .await
        .unwrap();
    let queued = store
        .queue_diagram_create(&b, &request, &upload)
        .await
        .unwrap();
    assert_eq!(queued.operation_seq, 1);
}

#[tokio::test]
async fn frozen_diagram_can_be_reviewed_and_dismissed_but_not_recreated_as_an_empty_file() {
    use notryx_client_store::FrozenCreateChoice;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let b = binding();
    fixture(&path, &b).await;
    let upload = upload();
    let request = uploaded_request(&upload);
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_diagram_create(&b, &request, &upload)
        .await
        .unwrap();
    let mut current = b.clone();
    current.device_id = Uuid::new_v4().to_string();
    current.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&current).await.unwrap();
    let page = store
        .list_frozen_creates(&current, None, 100)
        .await
        .unwrap();
    assert_eq!(page.entries[0].operation.request, request);
    assert_eq!(
        store
            .resolve_frozen_create(
                &b,
                &current,
                &queued.operation_id,
                &FrozenCreateChoice::Apply {
                    parent_id: ROOT.parse().unwrap(),
                    name: "new.drawio.png".into()
                }
            )
            .await,
        Err(StoreError::InvalidRequest)
    );
    store
        .resolve_frozen_create(
            &b,
            &current,
            &queued.operation_id,
            &FrozenCreateChoice::Dismiss,
        )
        .await
        .unwrap();
    assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
    let saved = store.list_create_operations(&b, 0, 100).await.unwrap();
    assert_eq!(saved.entries[0].operation.request, request);
}

#[tokio::test]
async fn v28_upgrade_preserves_existing_requests_and_rolls_back_on_table_collision() {
    for collision in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let b = binding();
        fixture(&path, &b).await;
        let mut store = ClientStore::open(&path).await.unwrap();
        let mut legacy = request();
        legacy.name = "legacy.md".into();
        let original = store.queue_create(&b, &legacy).await.unwrap();
        store.close().await.unwrap();
        let mut conn = db(&path).await;
        sqlx::raw_sql("DROP TABLE IF EXISTS diagram_creations; PRAGMA user_version=27;")
            .execute(&mut conn)
            .await
            .unwrap();
        if collision {
            sqlx::raw_sql("CREATE TABLE diagram_creations(sentinel TEXT); INSERT INTO diagram_creations VALUES ('preserve');").execute(&mut conn).await.unwrap();
        }
        conn.close().await.unwrap();
        let reopened = ClientStore::open(&path).await;
        if collision {
            assert!(reopened.is_err());
            let mut conn = db(&path).await;
            let version: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(version, 27);
            let sentinel: String = sqlx::query_scalar("SELECT sentinel FROM diagram_creations")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(sentinel, "preserve");
        } else {
            let mut store = reopened.unwrap();
            let PendingOutboxOperation::Create(saved) =
                store.next_pending_outbox(&b).await.unwrap().unwrap()
            else {
                panic!("create")
            };
            assert_eq!(saved, original);
            store.close().await.unwrap();
            let mut conn = db(&path).await;
            let version: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(version, 28);
        }
    }
}
