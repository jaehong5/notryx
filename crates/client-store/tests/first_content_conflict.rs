use notryx_client_store::{
    ClientStore, OutboxConflict, OutboxFailure, OutboxFailureKind, OutboxStatus,
    RegistrationBinding,
};
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

async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn conflict_with_followup(
    store: &mut ClientStore,
    binding: &RegistrationBinding,
) -> (String, String, String) {
    let session = store
        .open_registered_document(binding, DOC, b"base\n", "base-rev")
        .await
        .unwrap()
        .edit_session_id;
    store
        .save_registered_checkpoint(binding, DOC, &session, 1, b"sent\n")
        .await
        .unwrap();
    let failed = store.queue_checkpoint(binding, DOC, 1).await.unwrap();
    assert!(
        store
            .first_content_conflict(binding, DOC)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .prepare_outbox(binding)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        failed.operation_id
    );
    assert!(
        store
            .first_content_conflict(binding, DOC)
            .await
            .unwrap()
            .is_none()
    );
    store
        .save_registered_checkpoint(binding, DOC, &session, 2, b"later draft\n")
        .await
        .unwrap();
    let later = store.queue_checkpoint(binding, DOC, 2).await.unwrap();
    store
        .apply_outbox_conflict(
            binding,
            &OutboxConflict {
                operation_id: failed.operation_id.clone(),
                operation_seq: failed.operation_seq,
                workspace_epoch: binding.workspace_epoch.clone(),
                current_revision: "peer-rev".into(),
                current_etag: "\"peer-rev\"".into(),
            },
        )
        .await
        .unwrap();
    (session, failed.operation_id, later.operation_id)
}

#[tokio::test]
async fn finds_first_412_before_newer_queued_edit_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    assert!(
        store
            .first_content_conflict(&registration, DOC)
            .await
            .unwrap()
            .is_none()
    );
    let (session, conflict, later) = conflict_with_followup(&mut store, &registration).await;
    let state = store
        .document_outbox_state(&registration, DOC)
        .await
        .unwrap();
    assert_eq!(state.latest.unwrap().operation_id, later);
    assert_eq!(
        store
            .first_content_conflict(&registration, DOC)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        conflict
    );
    assert_eq!(
        store
            .first_content_conflict(&registration, DOC)
            .await
            .unwrap()
            .unwrap()
            .operation_seq,
        1
    );
    assert!(
        store
            .first_content_conflict(&registration, OTHER)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, DOC)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"later draft\n"
    );
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    store
        .close_registered_document(&registration, DOC, &session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .first_content_conflict(&registration, DOC)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        conflict
    );
    assert!(reopened.prepare_outbox(&registration).await.is_err());
    assert_eq!(
        reopened
            .read_registered_checkpoint(&registration, DOC)
            .await
            .unwrap()
            .unwrap()
            .base_bytes,
        b"base\n"
    );
}

#[tokio::test]
async fn terminal_deleted_failure_on_other_document_is_not_a_412_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store
        .open_registered_document(&registration, OTHER, b"base other", "other-base")
        .await
        .unwrap()
        .edit_session_id;
    store
        .save_registered_checkpoint(&registration, OTHER, &session, 1, b"other edit")
        .await
        .unwrap();
    let deleted = store
        .queue_checkpoint(&registration, OTHER, 1)
        .await
        .unwrap();
    store.prepare_outbox(&registration).await.unwrap().unwrap();
    store
        .apply_outbox_failure(
            &registration,
            &OutboxFailure {
                operation_id: deleted.operation_id.clone(),
                operation_seq: deleted.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                kind: OutboxFailureKind::NodeDeleted,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .first_content_conflict(&registration, OTHER)
            .await
            .unwrap()
            .is_none()
    );
    let (_, conflict, _) = conflict_with_followup(&mut store, &registration).await;
    let first = store
        .first_content_conflict(&registration, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.operation_id, conflict);
    assert_eq!(first.operation_seq, 2);
    assert!(
        store
            .first_content_conflict(&registration, OTHER)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, &deleted.operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        OutboxStatus::Failed
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, OTHER)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"other edit"
    );
}

#[tokio::test]
async fn stale_registration_is_rejected_and_new_registration_does_not_attach_old_412() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let (session, conflict, _) = conflict_with_followup(&mut store, &registration).await;
    store
        .close_registered_document(&registration, DOC, &session)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = registration.workspace_id.clone();
    replacement.workspace_epoch = registration.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert!(
        store
            .first_content_conflict(&registration, DOC)
            .await
            .is_err()
    );
    assert!(
        store
            .first_content_conflict(&replacement, DOC)
            .await
            .unwrap()
            .is_none()
    );
    let original = store
        .read_outbox_operation(&registration, &conflict)
        .await
        .unwrap()
        .unwrap();
    assert!(original.frozen);
    assert_eq!(original.status, OutboxStatus::Conflict);
    assert_eq!(original.bytes, b"sent\n");
    let draft = store
        .read_registered_checkpoint(&registration, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
}

#[tokio::test]
async fn damaged_412_evidence_is_an_error_not_missing_or_a_different_result() {
    for damage in [
        "ack",
        "next_seq",
        "etag",
        "equal_revision",
        "payload",
        "status",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.db");
        let registration = binding();
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&registration).await.unwrap();
        let (session, conflict, later) = conflict_with_followup(&mut store, &registration).await;
        store
            .close_registered_document(&registration, DOC, &session)
            .await
            .unwrap();
        store.close().await.unwrap();
        let mut connection = db(&path).await;
        let sql = match damage {
            "ack" => "UPDATE outbox_streams SET ack_through=0 WHERE registration_id=1",
            "next_seq" => "UPDATE outbox_streams SET next_seq=ack_through WHERE registration_id=1",
            "etag" => "UPDATE client_outbox SET conflict_etag='\"other\"' WHERE operation_id=?",
            "equal_revision" => {
                "UPDATE client_outbox SET conflict_revision='base-rev',conflict_etag='\"base-rev\"' WHERE operation_id=?"
            }
            "payload" => "UPDATE client_outbox SET bytes=X'686f7374696c65' WHERE operation_id=?",
            "status" => "UPDATE client_outbox SET abandon_state='requested' WHERE operation_id=?",
            _ => unreachable!(),
        };
        let mut query = sqlx::query(sql);
        if !matches!(damage, "ack" | "next_seq") {
            query = query.bind(&conflict);
        }
        query.execute(&mut connection).await.unwrap();
        connection.close().await.unwrap();
        let mut reopened = ClientStore::open(&path).await.unwrap();
        assert!(
            reopened
                .first_content_conflict(&registration, DOC)
                .await
                .is_err(),
            "{damage}"
        );
        let draft = reopened
            .read_registered_checkpoint(&registration, DOC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"later draft\n", "{damage}");
        assert_eq!(draft.base_bytes, b"base\n", "{damage}");
        assert_eq!(
            reopened
                .read_outbox_operation(&registration, &later)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"later draft\n",
            "{damage}"
        );
    }
}

async fn retained_412_evidence_with_inconsistent_status_is_an_error(damage: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let (session, conflict, later) = conflict_with_followup(&mut store, &registration).await;
    store
        .close_registered_document(&registration, DOC, &session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut connection = db(&path).await;
    let sql = match damage {
        "terminal" => "UPDATE client_outbox SET terminal_kind='node_deleted' WHERE operation_id=?",
        "queued" => "UPDATE client_outbox SET status='queued' WHERE operation_id=?",
        "sent" => "UPDATE client_outbox SET status='sent' WHERE operation_id=?",
        _ => unreachable!(),
    };
    sqlx::query(sql)
        .bind(&conflict)
        .execute(&mut connection)
        .await
        .unwrap();
    let before: (String, Option<String>, Option<String>, Option<String>, Vec<u8>, String) = sqlx::query_as(
        "SELECT status,terminal_kind,conflict_revision,conflict_etag,bytes,payload_hash FROM client_outbox WHERE operation_id=?",
    ).bind(&conflict).fetch_one(&mut connection).await.unwrap();
    assert_eq!(before.2.as_deref(), Some("peer-rev"));
    assert_eq!(before.3.as_deref(), Some("\"peer-rev\""));
    connection.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert!(
        store
            .first_content_conflict(&registration, DOC)
            .await
            .is_err(),
        "{damage}"
    );
    let draft = store
        .read_registered_checkpoint(&registration, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later draft\n");
    assert_eq!(draft.base_bytes, b"base\n");
    assert_eq!(draft.base_revision, "base-rev");
    assert_eq!(store.receipt_ack_through(&registration).await.unwrap(), 1);
    assert_eq!(
        store
            .read_outbox_operation(&registration, &later)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"later draft\n"
    );
    store.close().await.unwrap();
    let mut connection = db(&path).await;
    let after: (String, Option<String>, Option<String>, Option<String>, Vec<u8>, String) = sqlx::query_as(
        "SELECT status,terminal_kind,conflict_revision,conflict_etag,bytes,payload_hash FROM client_outbox WHERE operation_id=?",
    ).bind(&conflict).fetch_one(&mut connection).await.unwrap();
    assert_eq!(after, before, "{damage}");
    connection.close().await.unwrap();
}

#[tokio::test]
async fn retained_412_evidence_with_terminal_failure_is_not_ignored() {
    retained_412_evidence_with_inconsistent_status_is_an_error("terminal").await;
}

#[tokio::test]
async fn retained_412_evidence_with_queued_status_is_not_ignored() {
    retained_412_evidence_with_inconsistent_status_is_an_error("queued").await;
}

#[tokio::test]
async fn retained_412_evidence_with_sent_status_is_not_ignored() {
    retained_412_evidence_with_inconsistent_status_is_an_error("sent").await;
}
