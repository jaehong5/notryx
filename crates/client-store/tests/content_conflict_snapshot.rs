use notryx_client_store::{ClientStore, OutboxConflict, RegistrationBinding};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const DOC: &str = "019a1000-0000-7000-8000-000000000002";
const OTHER: &str = "019a1000-0000-7000-8000-000000000003";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn prepare(store: &mut ClientStore, registration: &RegistrationBinding) -> (String, String) {
    let opened = store
        .open_registered_document(registration, DOC, b"original\r\n", "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            registration,
            DOC,
            &opened.edit_session_id,
            1,
            b"transmitted\n",
        )
        .await
        .unwrap();
    let queued = store.queue_checkpoint(registration, DOC, 1).await.unwrap();
    let sent = store.prepare_outbox(registration).await.unwrap().unwrap();
    assert_eq!(sent.operation_id, queued.operation_id);
    store
        .save_registered_checkpoint(
            registration,
            DOC,
            &opened.edit_session_id,
            2,
            b"newer draft\n",
        )
        .await
        .unwrap();
    store.queue_checkpoint(registration, DOC, 2).await.unwrap();
    store
        .apply_outbox_conflict(
            registration,
            &OutboxConflict {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                current_revision: "peer-rev".into(),
                current_etag: "\"peer-rev\"".into(),
            },
        )
        .await
        .unwrap();
    (opened.edit_session_id, sent.operation_id)
}

#[tokio::test]
async fn snapshot_separates_failed_payload_from_newest_checkpoint_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let (session, id) = prepare(&mut store, &registration).await;
    let result = store
        .inspect_content_conflict(&registration, DOC, &id)
        .await
        .unwrap();
    assert_eq!(result.binding, registration);
    assert_eq!(result.document_id, DOC);
    assert_eq!(result.operation_id, id);
    assert_eq!(result.operation_seq, 1);
    assert_eq!(result.expected_revision, "base-rev");
    assert_eq!(result.conflict_revision, "peer-rev");
    assert_eq!(result.conflict_etag, "\"peer-rev\"");
    assert_eq!(result.payload, b"transmitted\n");
    assert_eq!(result.generation, 2);
    assert_eq!(result.local, b"newer draft\n");
    assert_eq!(result.base, b"original\r\n");
    assert_eq!(result.base_revision, "base-rev");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    store
        .save_registered_checkpoint(&registration, DOC, &session, 3, b"latest draft\n")
        .await
        .unwrap();
    let newer = store
        .inspect_content_conflict(&registration, DOC, &id)
        .await
        .unwrap();
    assert_eq!(newer.generation, 3);
    assert_eq!(newer.local, b"latest draft\n");
    assert_eq!(newer.payload, b"transmitted\n");
    assert_eq!(newer.base, b"original\r\n");
    store
        .close_registered_document(&registration, DOC, &session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .inspect_content_conflict(&registration, DOC, &id)
            .await
            .unwrap(),
        newer
    );
    assert_eq!(
        reopened.receipt_ack_through(&registration).await.unwrap(),
        1
    );
    assert!(reopened.prepare_outbox(&registration).await.is_err());
}

#[tokio::test]
async fn snapshot_rejects_wrong_document_binding_and_nonconflict_without_consuming_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, DOC, b"base", "base-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, DOC, &opened.edit_session_id, 1, b"draft")
        .await
        .unwrap();
    let queued = store.queue_checkpoint(&registration, DOC, 1).await.unwrap();
    assert!(
        store
            .inspect_content_conflict(&registration, DOC, &queued.operation_id)
            .await
            .is_err()
    );
    store.prepare_outbox(&registration).await.unwrap().unwrap();
    assert!(
        store
            .inspect_content_conflict(&registration, DOC, &queued.operation_id)
            .await
            .is_err()
    );
    store
        .apply_outbox_conflict(
            &registration,
            &OutboxConflict {
                operation_id: queued.operation_id.clone(),
                operation_seq: queued.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                current_revision: "peer-rev".into(),
                current_etag: "\"peer-rev\"".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .inspect_content_conflict(&registration, OTHER, &queued.operation_id)
            .await
            .is_err()
    );
    assert!(
        store
            .inspect_content_conflict(&binding(), DOC, &queued.operation_id)
            .await
            .is_err()
    );
    assert!(
        store
            .inspect_content_conflict(&registration, DOC, &Uuid::now_v7().to_string())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .inspect_content_conflict(&registration, DOC, &queued.operation_id)
            .await
            .unwrap()
            .local,
        b"draft"
    );
    store
        .close_registered_document(&registration, DOC, &opened.edit_session_id)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = registration.workspace_id.clone();
    replacement.workspace_epoch = registration.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert!(
        store
            .inspect_content_conflict(&registration, DOC, &queued.operation_id)
            .await
            .is_err()
    );
    assert!(
        store
            .inspect_content_conflict(&replacement, DOC, &queued.operation_id)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, DOC)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"draft"
    );
}

#[tokio::test]
async fn damaged_ack_receipt_digest_or_base_cannot_produce_a_snapshot_or_mutate_draft() {
    for damage in [
        "ack",
        "next_seq",
        "revision",
        "same_revision",
        "etag",
        "payload",
        "digest",
        "base",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.db");
        let registration = binding();
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&registration).await.unwrap();
        let (session, id) = prepare(&mut store, &registration).await;
        store
            .close_registered_document(&registration, DOC, &session)
            .await
            .unwrap();
        store.close().await.unwrap();
        let mut db = connect(&path).await;
        let command = match damage {
            "ack" => "UPDATE outbox_streams SET ack_through=0 WHERE registration_id=1",
            "next_seq" => "UPDATE outbox_streams SET next_seq=1 WHERE registration_id=1",
            "revision" => {
                "UPDATE client_outbox SET conflict_revision='base-rev' WHERE operation_id=?"
            }
            "same_revision" => {
                "UPDATE client_outbox SET conflict_revision='base-rev', conflict_etag='\"base-rev\"' WHERE operation_id=?"
            }
            "etag" => "UPDATE client_outbox SET conflict_etag='\"wrong\"' WHERE operation_id=?",
            "payload" => "UPDATE client_outbox SET bytes=X'686f7374696c65' WHERE operation_id=?",
            "digest" => "UPDATE client_outbox SET payload_hash='sha256:bad' WHERE operation_id=?",
            "base" => {
                "UPDATE draft_bases SET revision='different-base' WHERE document_id=(SELECT storage_id FROM client_outbox WHERE operation_id=?)"
            }
            _ => unreachable!(),
        };
        let mut query = sqlx::query(command);
        if damage != "ack" && damage != "next_seq" {
            query = query.bind(&id);
        }
        query.execute(&mut db).await.unwrap();
        let draft_before: Vec<u8> = sqlx::query_scalar("SELECT c.bytes FROM draft_checkpoints c JOIN registered_documents d ON c.document_id=d.storage_id WHERE d.document_id=?")
            .bind(DOC).fetch_one(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert!(
            store
                .inspect_content_conflict(&registration, DOC, &id)
                .await
                .is_err(),
            "{damage}"
        );
        store.close().await.unwrap();
        let mut db = connect(&path).await;
        let draft_after: Vec<u8> = sqlx::query_scalar("SELECT c.bytes FROM draft_checkpoints c JOIN registered_documents d ON c.document_id=d.storage_id WHERE d.document_id=?")
            .bind(DOC).fetch_one(&mut db).await.unwrap();
        assert_eq!(draft_before, draft_after, "{damage}");
        assert_eq!(draft_after, b"newer draft\n");
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM client_outbox WHERE registration_id=1")
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(count, 2);
    }
}

#[tokio::test]
async fn later_remote_tombstone_does_not_discard_or_relabel_the_412_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let (session, id) = prepare(&mut store, &registration).await;
    store
        .close_registered_document(&registration, DOC, &session)
        .await
        .unwrap();
    let before = store
        .inspect_content_conflict(&registration, DOC, &id)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    let root = "019a1000-0000-7000-8000-000000000001";
    let root_state = serde_json::json!({"node_id":root,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"1","last_change_seq":"2"});
    let deleted = serde_json::json!({"node_id":DOC,"parent_id":root,"name":"target.md","kind":"file","deleted":true,"content_rev":"peer-rev","content_etag":"\"peer-rev\"","meta_rev":"meta-2","meta_etag":"\"meta-2\"","node_etag":"\"node-2\"","content_hash":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":"0","children_generation":"0","last_change_seq":"2"});
    sqlx::query("INSERT INTO remote_index(registration_id,root_id,cursor) VALUES(1,?,'2')")
        .bind(root)
        .execute(&mut db)
        .await
        .unwrap();
    for (node, state) in [(root, root_state), (DOC, deleted)] {
        sqlx::query(
            "INSERT INTO remote_nodes(registration_id,node_id,state,content_ready) VALUES(1,?,?,0)",
        )
        .bind(node)
        .bind(state.to_string())
        .execute(&mut db)
        .await
        .unwrap();
    }
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .inspect_content_conflict(&registration, DOC, &id)
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, DOC)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"newer draft\n"
    );
    assert!(store.prepare_outbox(&registration).await.is_err());
}
