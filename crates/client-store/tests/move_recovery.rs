use notryx_client_store::{ClientStore, RecoveryOperationKind, RecoveryStore, RegistrationBinding};
use notryx_protocol::MoveNodeRequest;
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const DESTINATION: &str = "019a1000-0000-7000-8000-000000000003";
const META_ETAG: &str = "\"meta-1\"";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

#[tokio::test]
async fn frozen_move_recovery_identifies_request_and_preserves_payload_and_condition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let original = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&original).await.unwrap();
    store.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"2","last_change_seq":"1"});
    let target = json!({"node_id":TARGET,"parent_id":ROOT,"name":"original.md","kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":"meta-1","meta_etag":META_ETAG,"node_etag":"\"node-1\"","content_hash":"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac","size":"5","children_generation":"0","last_change_seq":"1"});
    let destination = json!({"node_id":DESTINATION,"parent_id":ROOT,"name":"destination","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"destination-meta","meta_etag":"\"destination-meta\"","node_etag":"\"destination-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    for (id, node) in [(ROOT, root), (TARGET, target), (DESTINATION, destination)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(id).bind(node.to_string()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    let request = MoveNodeRequest {
        expected_parent_id: ROOT.parse().unwrap(),
        parent_id: DESTINATION.parse().unwrap(),
        name: "renamed.md".into(),
    };
    let mut store = ClientStore::open(&path).await.unwrap();
    let queued = store
        .queue_move(&original, TARGET.parse().unwrap(), META_ETAG, &request)
        .await
        .unwrap();
    store.prepare_pending_outbox(&original).await.unwrap();
    let mut replacement = binding();
    replacement.workspace_id = original.workspace_id.clone();
    replacement.workspace_epoch = original.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    store.close().await.unwrap();

    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let page = recovery.operations(None).await.unwrap();
    let item = page
        .items
        .iter()
        .find(|item| item.operation_id == queued.operation_id)
        .unwrap();
    assert_eq!(item.document_id, TARGET);
    assert_eq!(item.registration.as_ref(), Some(&original));
    assert_eq!(item.registration_status.as_deref(), Some("frozen"));
    assert_eq!(item.sequence, Some(queued.operation_seq));
    assert_eq!(item.generation, None);
    assert_eq!(item.kind, Some(RecoveryOperationKind::Move));
    assert_eq!(item.expected_meta_etag.as_deref(), Some(META_ETAG));
    assert!(!item.metadata_error);
    let bytes = recovery
        .operation_bytes(&queued.operation_id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<MoveNodeRequest>(&bytes).unwrap(),
        request
    );
    drop(recovery);
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let (stored_etag, stored_bytes): (String, Vec<u8>) =
        sqlx::query_as("SELECT expected_revision, bytes FROM client_outbox WHERE operation_id=?")
            .bind(&queued.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(stored_etag, META_ETAG);
    assert_eq!(stored_bytes, bytes);
    sqlx::query("UPDATE client_outbox SET expected_revision='invalid' WHERE operation_id=?")
        .bind(&queued.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut recovery = RecoveryStore::open(&path).await.unwrap();
    let damaged = recovery.operations(None).await.unwrap();
    let item = damaged
        .items
        .iter()
        .find(|item| item.operation_id == queued.operation_id)
        .unwrap();
    assert_eq!(item.kind, Some(RecoveryOperationKind::Move));
    assert!(item.metadata_error);
    assert_eq!(item.expected_meta_etag, None);
    assert_eq!(
        recovery
            .operation_bytes(&queued.operation_id)
            .await
            .unwrap(),
        bytes
    );
    let snapshot = dir.path().join("recovery.sqlite3");
    recovery.export_snapshot(&snapshot).await.unwrap();
    drop(recovery);
    let mut saved = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(snapshot)
            .read_only(true),
    )
    .await
    .unwrap();
    let (saved_condition, saved_bytes): (String, Vec<u8>) =
        sqlx::query_as("SELECT expected_revision, bytes FROM client_outbox WHERE operation_id=?")
            .bind(&queued.operation_id)
            .fetch_one(&mut saved)
            .await
            .unwrap();
    assert_eq!(saved_condition, "invalid");
    assert_eq!(saved_bytes, bytes);
    saved.close().await.unwrap();
}
