use notryx_client_store::{
    ClientStore, OutboxCommit, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::MoveNodeRequest;
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const TARGET: &str = "019a1000-0000-7000-8000-000000000002";
const DESTINATION: &str = "019a1000-0000-7000-8000-000000000003";
const OTHER: &str = "019a1000-0000-7000-8000-000000000004";
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

fn move_request() -> MoveNodeRequest {
    MoveNodeRequest {
        expected_parent_id: ROOT.parse().unwrap(),
        parent_id: DESTINATION.parse().unwrap(),
        name: "renamed.md".into(),
    }
}

async fn database(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn fixture(path: &Path, registration: &RegistrationBinding) {
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(registration).await.unwrap();
    store.close().await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"2","last_change_seq":"1"});
    let target = json!({"node_id":TARGET,"parent_id":ROOT,"name":"original.md","kind":"file","deleted":false,"content_rev":"content-1","content_etag":"\"content-1\"","meta_rev":"meta-1","meta_etag":META_ETAG,"node_etag":"\"node-1\"","content_hash":"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac","size":"5","children_generation":"0","last_change_seq":"1"});
    let destination = json!({"node_id":DESTINATION,"parent_id":ROOT,"name":"destination","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"destination-meta","meta_etag":"\"destination-meta\"","node_etag":"\"destination-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    let mut db = database(path).await;
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for (id, node) in [(ROOT, root), (TARGET, target), (DESTINATION, destination)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(id).bind(node.to_string()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
}

async fn remote_snapshot(path: &Path) -> (Vec<(String, String, i64)>, String) {
    let mut db = database(path).await;
    let nodes = sqlx::query_as("SELECT node_id,state,content_ready FROM remote_nodes WHERE registration_id=1 ORDER BY node_id").fetch_all(&mut db).await.unwrap();
    let cursor = sqlx::query_scalar("SELECT cursor FROM remote_index WHERE registration_id=1")
        .fetch_one(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    (nodes, cursor)
}

#[tokio::test]
async fn content_then_move_retains_fifo_precondition_and_newer_draft_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let original_tree = remote_snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let edit = store
        .open_registered_document(&registration, TARGET, b"base\n", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            TARGET,
            &edit.edit_session_id,
            1,
            b"sent edit\n",
        )
        .await
        .unwrap();
    let content = store
        .queue_checkpoint(&registration, TARGET, 1)
        .await
        .unwrap();
    let request = move_request();
    let movement = store
        .queue_move(&registration, TARGET.parse().unwrap(), META_ETAG, &request)
        .await
        .unwrap();
    assert_eq!(content.operation_seq, 1);
    assert_eq!(movement.operation_seq, 2);
    assert_eq!(movement.node_id.to_string(), TARGET);
    assert_eq!(movement.expected_meta_etag, META_ETAG);
    assert_eq!(movement.request, request);
    assert_eq!(movement.status, OutboxStatus::Queued);
    assert_eq!(
        Uuid::parse_str(&movement.operation_id)
            .unwrap()
            .get_version_num(),
        7
    );
    store
        .save_registered_checkpoint(
            &registration,
            TARGET,
            &edit.edit_session_id,
            2,
            b"newer edit\n",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, TARGET, &edit.edit_session_id)
        .await
        .unwrap();
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, original_tree);

    let mut store = ClientStore::open(&path).await.unwrap();
    let PendingOutboxOperation::Content(first) = store
        .prepare_pending_outbox(&registration)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("content must lead FIFO")
    };
    assert_eq!(first.operation_id, content.operation_id);
    assert_eq!(first.bytes, b"sent edit\n");
    store
        .apply_outbox_commit(
            &registration,
            &OutboxCommit {
                operation_id: first.operation_id.clone(),
                operation_seq: first.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                document_id: TARGET.into(),
                content_revision: "content-2".into(),
                content_hash: first.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    let Some(PendingOutboxOperation::Move(next)) =
        store.next_pending_outbox(&registration).await.unwrap()
    else {
        panic!("move must follow committed content")
    };
    assert_eq!(next, movement);
    let draft = store
        .read_registered_checkpoint(&registration, TARGET)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer edit\n");
    assert_eq!(draft.base_bytes, b"sent edit\n");
    assert_eq!(draft.base_revision, "content-2");
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, original_tree);
}

#[tokio::test]
async fn stale_move_conditions_do_not_consume_sequence_or_change_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let original_tree = remote_snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let request = move_request();
    assert!(
        store
            .queue_move(
                &registration,
                TARGET.parse().unwrap(),
                "\"old-meta\"",
                &request
            )
            .await
            .is_err()
    );
    let wrong_parent = MoveNodeRequest {
        expected_parent_id: DESTINATION.parse().unwrap(),
        ..request.clone()
    };
    assert!(
        store
            .queue_move(
                &registration,
                TARGET.parse().unwrap(),
                META_ETAG,
                &wrong_parent
            )
            .await
            .is_err()
    );
    let missing_parent = MoveNodeRequest {
        parent_id: OTHER.parse().unwrap(),
        ..request.clone()
    };
    assert!(
        store
            .queue_move(
                &registration,
                TARGET.parse().unwrap(),
                META_ETAG,
                &missing_parent
            )
            .await
            .is_err()
    );
    assert!(
        store
            .next_pending_outbox(&registration)
            .await
            .unwrap()
            .is_none()
    );
    let valid = store
        .queue_move(&registration, TARGET.parse().unwrap(), META_ETAG, &request)
        .await
        .unwrap();
    assert_eq!(valid.operation_seq, 1);
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, original_tree);
}

#[tokio::test]
async fn move_is_frozen_on_reregistration_and_other_draft_survives() {
    for sent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite3");
        let original = binding();
        fixture(&path, &original).await;
        let mut store = ClientStore::open(&path).await.unwrap();
        let movement = store
            .queue_move(
                &original,
                TARGET.parse().unwrap(),
                META_ETAG,
                &move_request(),
            )
            .await
            .unwrap();
        if sent {
            store.prepare_pending_outbox(&original).await.unwrap();
        }
        let edit = store
            .open_registered_document(&original, OTHER, b"other base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&original, OTHER, &edit.edit_session_id, 1, b"other draft")
            .await
            .unwrap();
        store
            .close_registered_document(&original, OTHER, &edit.edit_session_id)
            .await
            .unwrap();
        store.close().await.unwrap();
        let mut db = database(&path).await;
        let before: (i64, String, Vec<u8>, String, String) = sqlx::query_as("SELECT operation_seq,COALESCE(expected_revision,''),bytes,payload_hash,status FROM client_outbox WHERE operation_id=?")
            .bind(&movement.operation_id).fetch_one(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        let mut replacement = binding();
        replacement.workspace_id = original.workspace_id.clone();
        replacement.workspace_epoch = original.workspace_epoch.clone();
        store.activate_registration(&replacement).await.unwrap();
        assert!(matches!(
            store.next_pending_outbox(&original).await,
            Err(StoreError::StaleRegistration)
        ));
        assert!(
            store
                .next_pending_outbox(&replacement)
                .await
                .unwrap()
                .is_none()
        );
        let draft = store
            .read_registered_checkpoint(&original, OTHER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"other draft");
        assert_eq!(draft.base_bytes, b"other base");
        store.close().await.unwrap();
        let mut db = database(&path).await;
        let after: (i64, String, Vec<u8>, String, String) = sqlx::query_as("SELECT operation_seq,COALESCE(expected_revision,''),bytes,payload_hash,status FROM client_outbox WHERE operation_id=?")
            .bind(&movement.operation_id).fetch_one(&mut db).await.unwrap();
        assert_eq!(before, after);
        let status: String = sqlx::query_scalar("SELECT r.status FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id=?")
            .bind(&movement.operation_id).fetch_one(&mut db).await.unwrap();
        assert_eq!(status, "frozen");
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn move_queue_write_failure_keeps_original_data_and_does_not_consume_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite3");
    let registration = binding();
    fixture(&path, &registration).await;
    let original_tree = remote_snapshot(&path).await;
    let mut db = database(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_move BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='move' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(matches!(
        store
            .queue_move(
                &registration,
                TARGET.parse().unwrap(),
                META_ETAG,
                &move_request()
            )
            .await,
        Err(StoreError::Storage)
    ));
    assert!(
        store
            .next_pending_outbox(&registration)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    assert_eq!(remote_snapshot(&path).await, original_tree);
    let mut db = database(&path).await;
    let allocated: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox_streams WHERE registration_id=1 AND next_seq>1",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(allocated, 0);
    sqlx::query("DROP TRIGGER reject_move")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let movement = store
        .queue_move(
            &registration,
            TARGET.parse().unwrap(),
            META_ETAG,
            &move_request(),
        )
        .await
        .unwrap();
    assert_eq!(movement.operation_seq, 1);
    store.close().await.unwrap();
}
