use notryx_client_store::{
    ClientStore, DiagramDraftCheckpoint, OutboxCommit, OutboxConflict, OutboxOperation,
    RegistrationBinding, StoreError,
};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

const NODE: &str = "019a1000-0000-7000-8000-000000000002";
const BASE: &str = "<mxfile>base</mxfile>";
const LOCAL: &str = "<mxfile>local</mxfile>";
const REMOTE: &str = "<mxfile>remote</mxfile>";

fn hash(bytes: &[u8]) -> String {
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{digest}")
}

fn ack(binding: &RegistrationBinding, through: u64) -> notryx_protocol::AckResponse {
    serde_json::from_value(serde_json::json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "retired_through": through.to_string(), "last_seq": through.to_string(),
    }))
    .unwrap()
}

fn checkpoint(generation: u64, xml: &str, base: &str, revision: &str) -> DiagramDraftCheckpoint {
    DiagramDraftCheckpoint {
        expected_generation: generation,
        xml: xml.into(),
        base_xml: base.into(),
        base_revision: revision.into(),
    }
}

async fn connect(path: &std::path::Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

async fn conflict(
    path: &std::path::Path,
    png: bool,
) -> (ClientStore, RegistrationBinding, OutboxOperation) {
    let binding = RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    };
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(&binding).await.unwrap();
    store
        .save_diagram_draft(&binding, NODE, &checkpoint(0, LOCAL, BASE, "original-rev"))
        .await
        .unwrap();
    let operation = if png {
        store
            .queue_diagram_png(&binding, NODE, 1, b"\x89PNG\r\n\x1a\nlocal")
            .await
            .unwrap()
    } else {
        store
            .queue_diagram_checkpoint(&binding, NODE, 1)
            .await
            .unwrap()
    };
    store.prepare_outbox(&binding).await.unwrap();
    store
        .apply_outbox_conflict(
            &binding,
            &OutboxConflict {
                operation_id: operation.operation_id.clone(),
                operation_seq: operation.operation_seq,
                workspace_epoch: binding.workspace_epoch.clone(),
                current_revision: "remote-rev".into(),
                current_etag: "\"remote-rev\"".into(),
            },
        )
        .await
        .unwrap();
    let operation = store
        .read_outbox_operation(&binding, &operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    (store, binding, operation)
}

#[tokio::test]
async fn copied_conflict_reopens_chooses_remote_and_resumes_without_rewriting_receipt() {
    for png in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite");
        let (mut store, binding, old) = conflict(&path, png).await;
        let unqueued = "019a1000-0000-7000-8000-000000000003";
        store
            .save_diagram_draft(
                &binding,
                unqueued,
                &checkpoint(0, LOCAL, BASE, "original-rev"),
            )
            .await
            .unwrap();
        assert!(
            store
                .record_diagram_conflict_copy(&binding, unqueued, 1, &hash(b"unqueued export"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .first_content_conflict(&binding, NODE)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .complete_diagram_conflict_choose_remote(
                    &binding,
                    NODE,
                    1,
                    &old.operation_id,
                    REMOTE,
                    "remote-rev"
                )
                .await
                .is_err()
        );
        let proof = store
            .record_diagram_conflict_copy(&binding, NODE, 1, &hash(b"exported png"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(proof.operation_id, old.operation_id);
        assert_eq!(proof.generation, 1);
        assert_eq!(
            store
                .complete_diagram_conflict_choose_remote(
                    &binding,
                    NODE,
                    1,
                    &old.operation_id,
                    REMOTE,
                    "remote-rev"
                )
                .await,
            Err(StoreError::OutboxBlocked)
        );
        store
            .confirm_receipt_ack(&binding, &ack(&binding, old.operation_seq))
            .await
            .unwrap();
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .read_diagram_conflict_copy(&binding, NODE)
                .await
                .unwrap(),
            Some(proof)
        );
        // Closing an active label may checkpoint identical XML before the user's choice.
        store
            .save_diagram_draft(&binding, NODE, &checkpoint(1, LOCAL, BASE, "original-rev"))
            .await
            .unwrap();
        assert!(
            store
                .read_diagram_conflict_copy(&binding, NODE)
                .await
                .unwrap()
                .is_some()
        );
        let chosen = store
            .complete_diagram_conflict_choose_remote(
                &binding,
                NODE,
                2,
                &old.operation_id,
                REMOTE,
                "remote-rev",
            )
            .await
            .unwrap();
        assert_eq!(chosen.generation, 3);
        assert_eq!(chosen.xml, REMOTE);
        assert_eq!(chosen.base_xml, REMOTE);
        assert_eq!(chosen.base_revision, "remote-rev");
        assert!(
            store
                .read_diagram_conflict_copy(&binding, NODE)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .document_outbox_state(&binding, NODE)
                .await
                .unwrap()
                .has_conflict
        );
        assert!(
            store
                .first_content_conflict(&binding, NODE)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .read_outbox_operation(&binding, &old.operation_id)
                .await
                .unwrap(),
            Some(old.clone())
        );
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .complete_diagram_conflict_choose_remote(
                    &binding,
                    NODE,
                    2,
                    &old.operation_id,
                    REMOTE,
                    "remote-rev"
                )
                .await
                .unwrap(),
            chosen
        );
        store
            .save_diagram_draft(
                &binding,
                NODE,
                &checkpoint(3, "<mxfile>next</mxfile>", REMOTE, "remote-rev"),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .complete_diagram_conflict_choose_remote(
                    &binding,
                    NODE,
                    2,
                    &old.operation_id,
                    REMOTE,
                    "remote-rev"
                )
                .await,
            Err(StoreError::StaleGeneration)
        );
        let next = if !png {
            store
                .queue_diagram_png(&binding, NODE, 4, b"\x89PNG\r\n\x1a\nnext")
                .await
                .unwrap()
        } else {
            store
                .queue_diagram_checkpoint(&binding, NODE, 4)
                .await
                .unwrap()
        };
        assert_eq!(next.expected_revision.as_deref(), Some("remote-rev"));
        assert_ne!(next.operation_id, old.operation_id);
        assert_eq!(next.operation_seq, old.operation_seq + 1);
        store.prepare_outbox(&binding).await.unwrap();
        store
            .apply_outbox_commit(
                &binding,
                &OutboxCommit {
                    operation_id: next.operation_id.clone(),
                    operation_seq: next.operation_seq,
                    workspace_epoch: binding.workspace_epoch.clone(),
                    document_id: NODE.into(),
                    content_revision: "saved-next".into(),
                    content_hash: next.payload_hash,
                },
            )
            .await
            .unwrap();
        assert!(
            !store
                .document_outbox_state(&binding, NODE)
                .await
                .unwrap()
                .has_conflict
        );
        assert!(
            store
                .first_content_conflict(&binding, NODE)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .record_diagram_conflict_copy(&binding, NODE, 4, &hash(b"committed export"))
                .await
                .unwrap()
                .is_none()
        );
        store.close().await.unwrap();
        let mut db = connect(&path).await;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT xml FROM diagram_conflict_copies")
                .fetch_one(&mut db)
                .await
                .unwrap(),
            LOCAL
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT base_xml FROM diagram_conflict_copies")
                .fetch_one(&mut db)
                .await
                .unwrap(),
            BASE
        );
    }
}

#[tokio::test]
async fn new_input_and_registration_replacement_preserve_proof_and_block_stale_choice() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (mut store, binding, old) = conflict(&path, false).await;
    store
        .confirm_receipt_ack(&binding, &ack(&binding, old.operation_seq))
        .await
        .unwrap();
    assert_eq!(
        store
            .record_diagram_conflict_copy(&binding, NODE, 1, "bad hash")
            .await,
        Err(StoreError::InvalidRequest)
    );
    store
        .record_diagram_conflict_copy(&binding, NODE, 1, &hash(b"png"))
        .await
        .unwrap();
    store
        .save_diagram_draft(
            &binding,
            NODE,
            &checkpoint(1, "<mxfile>newer</mxfile>", BASE, "original-rev"),
        )
        .await
        .unwrap();
    let before = store.read_diagram_draft(&binding, NODE).await.unwrap();
    assert!(
        store
            .read_diagram_conflict_copy(&binding, NODE)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .complete_diagram_conflict_choose_remote(
                &binding,
                NODE,
                2,
                &old.operation_id,
                REMOTE,
                "remote-rev"
            )
            .await
            .is_err()
    );
    assert_eq!(
        store.read_diagram_draft(&binding, NODE).await.unwrap(),
        before
    );
    let other = RegistrationBinding {
        device_stream_id: Uuid::new_v4().to_string(),
        ..binding.clone()
    };
    store.activate_registration(&other).await.unwrap();
    assert!(
        store
            .complete_diagram_conflict_choose_remote(
                &binding,
                NODE,
                2,
                &old.operation_id,
                REMOTE,
                "remote-rev"
            )
            .await
            .is_err()
    );
    assert!(
        store
            .read_diagram_conflict_copy(&other, NODE)
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT xml FROM diagram_conflict_copies")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        LOCAL
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT xml FROM diagram_drafts")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "<mxfile>newer</mxfile>"
    );
}

#[tokio::test]
async fn failed_completion_rolls_back_draft_base_and_proof() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (mut store, binding, old) = conflict(&path, false).await;
    store
        .confirm_receipt_ack(&binding, &ack(&binding, old.operation_seq))
        .await
        .unwrap();
    let proof = store
        .record_diagram_conflict_copy(&binding, NODE, 1, &hash(b"png"))
        .await
        .unwrap();
    let before = store.read_diagram_draft(&binding, NODE).await.unwrap();
    let mut db = connect(&path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_completion BEFORE UPDATE ON diagram_conflict_copies BEGIN SELECT RAISE(ABORT,'injected failure'); END;").execute(&mut db).await.unwrap();
    assert_eq!(
        store
            .complete_diagram_conflict_choose_remote(
                &binding,
                NODE,
                1,
                &old.operation_id,
                REMOTE,
                "remote-rev"
            )
            .await,
        Err(StoreError::Storage)
    );
    assert_eq!(
        store.read_diagram_draft(&binding, NODE).await.unwrap(),
        before
    );
    assert_eq!(
        store
            .read_diagram_conflict_copy(&binding, NODE)
            .await
            .unwrap(),
        proof
    );
    assert!(
        store
            .document_outbox_state(&binding, NODE)
            .await
            .unwrap()
            .has_conflict
    );
    assert_eq!(
        store
            .read_outbox_operation(&binding, &old.operation_id)
            .await
            .unwrap(),
        Some(old)
    );
}

#[tokio::test]
async fn pending_followup_blocks_choice_without_modifying_the_saved_copy() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (mut store, binding, old) = conflict(&path, false).await;
    store
        .confirm_receipt_ack(&binding, &ack(&binding, old.operation_seq))
        .await
        .unwrap();
    store
        .record_diagram_conflict_copy(&binding, NODE, 1, &hash(b"png"))
        .await
        .unwrap();
    let before = store.read_diagram_draft(&binding, NODE).await.unwrap();
    let mut db = connect(&path).await;
    sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,payload_len) SELECT ?,registration_id,operation_seq+1,document_id,storage_id,generation,expected_revision,bytes,payload_hash,'queued',payload_len FROM client_outbox WHERE operation_id=?")
        .bind(Uuid::now_v7().to_string()).bind(&old.operation_id).execute(&mut db).await.unwrap();
    sqlx::query("UPDATE outbox_streams SET next_seq=next_seq+1")
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        store
            .complete_diagram_conflict_choose_remote(
                &binding,
                NODE,
                1,
                &old.operation_id,
                REMOTE,
                "remote-rev"
            )
            .await,
        Err(StoreError::OutboxBlocked)
    );
    assert_eq!(
        store.read_diagram_draft(&binding, NODE).await.unwrap(),
        before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM diagram_conflict_copies WHERE completed_generation IS NULL"
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn newer_conflict_requires_its_own_copy_and_preserves_previous_completion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let (mut store, binding, old) = conflict(&path, false).await;
    store
        .confirm_receipt_ack(&binding, &ack(&binding, old.operation_seq))
        .await
        .unwrap();
    let proof = store
        .record_diagram_conflict_copy(&binding, NODE, 1, &hash(b"png"))
        .await
        .unwrap();
    assert_eq!(
        store
            .record_diagram_conflict_copy(&binding, NODE, 1, &hash(b"another export of same XML"))
            .await
            .unwrap(),
        proof
    );
    store
        .complete_diagram_conflict_choose_remote(
            &binding,
            NODE,
            1,
            &old.operation_id,
            REMOTE,
            "remote-rev",
        )
        .await
        .unwrap();
    assert!(
        store
            .record_diagram_conflict_copy(&binding, NODE, 2, &hash(b"clean export"))
            .await
            .unwrap()
            .is_none()
    );
    store
        .save_diagram_draft(
            &binding,
            NODE,
            &checkpoint(2, "<mxfile>another edit</mxfile>", REMOTE, "remote-rev"),
        )
        .await
        .unwrap();
    let next = store
        .queue_diagram_checkpoint(&binding, NODE, 3)
        .await
        .unwrap();
    store.prepare_outbox(&binding).await.unwrap();
    store
        .apply_outbox_conflict(
            &binding,
            &OutboxConflict {
                operation_id: next.operation_id.clone(),
                operation_seq: next.operation_seq,
                workspace_epoch: binding.workspace_epoch.clone(),
                current_revision: "remote-newer".into(),
                current_etag: "\"remote-newer\"".into(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&binding, &ack(&binding, next.operation_seq))
        .await
        .unwrap();
    assert_eq!(
        store
            .first_content_conflict(&binding, NODE)
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        next.operation_id
    );
    assert!(
        store
            .read_diagram_conflict_copy(&binding, NODE)
            .await
            .unwrap()
            .is_none()
    );
    store
        .record_diagram_conflict_copy(&binding, NODE, 3, &hash(b"second copy"))
        .await
        .unwrap();
    store
        .complete_diagram_conflict_choose_remote(
            &binding,
            NODE,
            3,
            &next.operation_id,
            "<mxfile>new remote</mxfile>",
            "remote-newer",
        )
        .await
        .unwrap();
    assert!(
        store
            .first_content_conflict(&binding, NODE)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .document_outbox_state(&binding, NODE)
            .await
            .unwrap()
            .has_conflict
    );
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM diagram_conflict_copies WHERE completed_generation IS NOT NULL"
        )
        .fetch_one(&mut db)
        .await
        .unwrap(),
        2
    );
}
