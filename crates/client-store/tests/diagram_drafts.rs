use notryx_client_store::{
    ClientStore, DiagramDraftCheckpoint, OutboxCommit, OutboxConflict, OutboxOperation,
    OutboxStatus, RegistrationBinding, StoreError,
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use uuid::Uuid;

const NODE: &str = "019a1000-0000-7000-8000-000000000002";
const BASE: &str = "<mxGraphModel><root/></mxGraphModel>";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn checkpoint(expected_generation: u64, xml: &str) -> DiagramDraftCheckpoint {
    DiagramDraftCheckpoint {
        expected_generation,
        xml: xml.into(),
        base_xml: BASE.into(),
        base_revision: "original-revision".into(),
    }
}

async fn connect(path: &std::path::Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

fn receipt(item: &OutboxOperation, revision: &str) -> OutboxCommit {
    OutboxCommit {
        operation_id: item.operation_id.clone(),
        operation_seq: item.operation_seq,
        workspace_epoch: item.binding.workspace_epoch.clone(),
        document_id: item.document_id.clone(),
        content_revision: revision.into(),
        content_hash: item.payload_hash.clone(),
    }
}

#[tokio::test]
async fn diagram_queue_reopens_with_exact_payload_and_shares_fifo_with_markdown() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let markdown = store
        .open_registered_document(&registration, "markdown", b"base", "md-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            "markdown",
            &markdown.edit_session_id,
            1,
            b"edited",
        )
        .await
        .unwrap();
    let first = store
        .queue_checkpoint(&registration, "markdown", 1)
        .await
        .unwrap();
    let xml = "<mxfile>first</mxfile>";
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, xml))
        .await
        .unwrap();
    assert_eq!(
        store.queue_diagram_checkpoint(&registration, NODE, 2).await,
        Err(StoreError::GenerationMismatch)
    );
    let diagram = store
        .queue_diagram_checkpoint(&registration, NODE, 1)
        .await
        .unwrap();
    assert_eq!(diagram.operation_seq, first.operation_seq + 1);
    assert_eq!(diagram.bytes, xml.as_bytes());
    assert_eq!(
        diagram.expected_revision.as_deref(),
        Some("original-revision")
    );
    assert_eq!(
        store
            .queue_diagram_checkpoint(&registration, NODE, 1)
            .await
            .unwrap(),
        diagram
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .read_outbox_operation(&registration, &diagram.operation_id)
            .await
            .unwrap(),
        Some(diagram.clone())
    );
    assert_eq!(
        store.next_outbox(&registration).await.unwrap(),
        Some(first.clone())
    );
    store.prepare_outbox(&registration).await.unwrap();
    store
        .apply_outbox_commit(&registration, &receipt(&first, "md-saved"))
        .await
        .unwrap();
    assert_eq!(
        store.next_outbox(&registration).await.unwrap(),
        Some(diagram)
    );
    assert_eq!(store.list_registered_drafts().await.unwrap().len(), 1);
    assert!(
        store
            .open_registered_document(&registration, NODE, BASE.as_bytes(), "original-revision")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn diagram_late_ack_preserves_newer_xml_and_next_save_uses_committed_base() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let first_xml = "<mxfile>first</mxfile>";
    let newer_xml = "<mxfile>newer</mxfile>";
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, first_xml))
        .await
        .unwrap();
    let first = store
        .queue_diagram_checkpoint(&registration, NODE, 1)
        .await
        .unwrap();
    store.prepare_outbox(&registration).await.unwrap();
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(1, newer_xml))
        .await
        .unwrap();
    assert_eq!(
        store.queue_diagram_checkpoint(&registration, NODE, 2).await,
        Err(StoreError::OutboxBlocked)
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, &first.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        first_xml.as_bytes()
    );
    store
        .apply_outbox_commit(&registration, &receipt(&first, "saved-first"))
        .await
        .unwrap();
    store
        .apply_outbox_commit(&registration, &receipt(&first, "saved-first"))
        .await
        .unwrap();
    let draft = store
        .read_diagram_draft(&registration, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            draft.generation,
            draft.xml.as_str(),
            draft.base_xml.as_str(),
            draft.base_revision.as_str()
        ),
        (2, newer_xml, first_xml, "saved-first")
    );
    let second = store
        .queue_diagram_checkpoint(&registration, NODE, 2)
        .await
        .unwrap();
    assert_eq!(second.expected_revision.as_deref(), Some("saved-first"));
    assert_eq!(second.operation_seq, first.operation_seq + 1);
    store.prepare_outbox(&registration).await.unwrap();
    store
        .apply_outbox_commit(&registration, &receipt(&second, "saved-second"))
        .await
        .unwrap();
    let ack = serde_json::from_value(serde_json::json!({
        "workspace_epoch": registration.workspace_epoch,
        "device_stream_id": registration.device_stream_id,
        "last_seq": "2", "retired_through": "2",
    }))
    .unwrap();
    store
        .confirm_receipt_ack(&registration, &ack)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_diagram_draft(&registration, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.xml, newer_xml);
    assert_eq!(draft.base_xml, newer_xml);
    assert_eq!(draft.base_revision, "saved-second");
    assert_eq!(
        store
            .confirmed_receipt_through(&registration)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .queue_diagram_checkpoint(&registration, NODE, 2)
            .await
            .unwrap()
            .status,
        OutboxStatus::Committed
    );
    assert!(store.list_registered_drafts().await.unwrap().is_empty());
}

#[tokio::test]
async fn diagram_conflict_and_registration_change_preserve_exact_draft_and_request() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, "<mxfile>mine</mxfile>"))
        .await
        .unwrap();
    let queued = store
        .queue_diagram_checkpoint(&registration, NODE, 1)
        .await
        .unwrap();
    let before = store.read_diagram_draft(&registration, NODE).await.unwrap();
    store.prepare_outbox(&registration).await.unwrap();
    store
        .apply_outbox_conflict(
            &registration,
            &OutboxConflict {
                operation_id: queued.operation_id.clone(),
                operation_seq: queued.operation_seq,
                workspace_epoch: registration.workspace_epoch.clone(),
                current_revision: "peer-revision".into(),
                current_etag: "\"peer-revision\"".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.queue_diagram_checkpoint(&registration, NODE, 1).await,
        Err(StoreError::OutboxBlocked)
    );
    assert_eq!(
        store.read_diagram_draft(&registration, NODE).await.unwrap(),
        before
    );
    let next = RegistrationBinding {
        device_stream_id: Uuid::new_v4().to_string(),
        ..registration.clone()
    };
    store.activate_registration(&next).await.unwrap();
    store
        .save_diagram_draft(&next, NODE, &checkpoint(0, BASE))
        .await
        .unwrap();
    assert_eq!(
        store.queue_diagram_checkpoint(&next, NODE, 1).await,
        Err(StoreError::ReconciliationRequired)
    );
    let retained = store
        .read_outbox_operation(&registration, &queued.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.bytes, queued.bytes);
    assert_eq!(retained.status, OutboxStatus::Conflict);
    assert!(retained.frozen);
}

#[tokio::test]
async fn diagram_queue_rejects_existing_markdown_checkpoint_without_overwriting_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let opened = store
        .open_registered_document(&registration, NODE, b"base", "md-rev")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, NODE, &opened.edit_session_id, 1, b"markdown")
        .await
        .unwrap();
    store
        .close_registered_document(&registration, NODE, &opened.edit_session_id)
        .await
        .unwrap();
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, BASE))
        .await
        .unwrap();
    assert_eq!(
        store.queue_diagram_checkpoint(&registration, NODE, 1).await,
        Err(StoreError::OutboxBlocked)
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, NODE)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"markdown"
    );
    assert!(store.next_outbox(&registration).await.unwrap().is_none());
}

#[tokio::test]
async fn checkpoints_survive_reopen_and_preserve_the_original_base() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    assert_eq!(
        store.read_diagram_draft(&registration, NODE).await.unwrap(),
        None
    );
    let xml = "<mxGraphModel><root><mxCell value=\"한글\"/></root></mxGraphModel>";
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(0, xml))
            .await,
        Ok(1)
    );
    assert!(store.list_registered_drafts().await.unwrap().is_empty());
    store.close().await.unwrap();

    let mut store = ClientStore::open(&path).await.unwrap();
    let restored = store
        .read_diagram_draft(&registration, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            restored.generation,
            restored.xml.as_str(),
            restored.base_xml.as_str(),
            restored.base_revision.as_str()
        ),
        (1, xml, BASE, "original-revision")
    );
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(0, BASE))
            .await,
        Err(StoreError::StaleGeneration)
    );
    let mut changed_base = checkpoint(1, xml);
    changed_base.base_xml = "<mxfile/>".into();
    changed_base.base_revision = "new-server-revision".into();
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &changed_base)
            .await,
        Err(StoreError::InvalidRequest)
    );
    assert_eq!(
        store
            .read_diagram_draft(&registration, NODE)
            .await
            .unwrap()
            .unwrap(),
        restored
    );
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(1, BASE))
            .await,
        Ok(2)
    );
    let next = store
        .read_diagram_draft(&registration, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            next.generation,
            next.base_xml.as_str(),
            next.base_revision.as_str()
        ),
        (2, BASE, "original-revision")
    );
}

#[tokio::test]
async fn registration_replacement_isolates_but_does_not_delete_existing_diagrams() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let old = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    store
        .save_diagram_draft(&old, NODE, &checkpoint(0, BASE))
        .await
        .unwrap();
    let next = RegistrationBinding {
        workspace_epoch: Uuid::new_v4().to_string(),
        ..old.clone()
    };
    store.activate_registration(&next).await.unwrap();
    assert_eq!(
        store.read_diagram_draft(&old, NODE).await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(
        store
            .save_diagram_draft(&old, NODE, &checkpoint(1, BASE))
            .await,
        Err(StoreError::StaleRegistration)
    );
    assert_eq!(store.read_diagram_draft(&next, NODE).await.unwrap(), None);
    assert_eq!(
        store
            .save_diagram_draft(&next, NODE, &checkpoint(0, "<mxfile/>"))
            .await,
        Ok(1)
    );
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    let retained: String = sqlx::query_scalar("SELECT d.xml FROM diagram_drafts d JOIN client_registrations r ON r.id=d.registration_id WHERE r.workspace_epoch=?")
        .bind(&old.workspace_epoch).fetch_one(&mut db).await.unwrap();
    assert_eq!(retained, BASE);
}

#[tokio::test]
async fn v24_upgrade_preserves_existing_drafts_and_credential_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    store
        .set_credential_expiry(&registration, 100, 300)
        .await
        .unwrap();
    let document = store
        .open_registered_document(&registration, NODE, b"markdown base", "markdown-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            NODE,
            &document.edit_session_id,
            1,
            b"markdown draft",
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    sqlx::raw_sql("DROP TABLE IF EXISTS diagram_save_snapshots; DROP TABLE IF EXISTS diagram_drafts; PRAGMA user_version=24;")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE diagram_drafts(sentinel TEXT); INSERT INTO diagram_drafts VALUES('preserve-me');")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(ClientStore::open(&path).await.is_err());
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        24
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sentinel FROM diagram_drafts")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "preserve-me"
    );
    sqlx::query("DROP TABLE diagram_drafts")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store.credential_expiry(&registration).await.unwrap(),
        Some((100, 300))
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, NODE)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"markdown draft"
    );
    assert_eq!(
        store
            .save_diagram_draft(&registration, NODE, &checkpoint(0, BASE))
            .await,
        Ok(1)
    );
    store.close().await.unwrap();
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        26
    );
}

#[tokio::test]
async fn invalid_checkpoints_leave_the_previous_draft_intact() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, BASE))
        .await
        .unwrap();
    let original = store.read_diagram_draft(&registration, NODE).await.unwrap();
    for input in [
        checkpoint(1, ""),
        checkpoint(1, &"x".repeat(5 * 1024 * 1024 + 1)),
        checkpoint(u64::MAX, BASE),
    ] {
        assert!(
            store
                .save_diagram_draft(&registration, NODE, &input)
                .await
                .is_err()
        );
    }
    assert!(
        store
            .save_diagram_draft(&registration, "bad-node", &checkpoint(0, BASE))
            .await
            .is_err()
    );
    assert_eq!(
        store.read_diagram_draft(&registration, NODE).await.unwrap(),
        original
    );
}

#[tokio::test]
async fn png_request_survives_reopen_and_late_ack_keeps_newer_xml() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("client.sqlite");
    let registration = binding();
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&registration).await.unwrap();
    let xml = "<mxfile>PNG snapshot</mxfile>";
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, xml))
        .await
        .unwrap();
    let mut png = vec![23; 5 * 1024 * 1024 + 1];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let queued = store
        .queue_diagram_png(&registration, NODE, 1, &png)
        .await
        .unwrap();
    assert_eq!(queued.bytes, png);
    assert_eq!(
        store
            .read_diagram_operation_xml(&registration, &queued.operation_id)
            .await
            .unwrap()
            .as_deref(),
        Some(xml)
    );
    assert_eq!(
        store
            .document_outbox_state(&registration, NODE)
            .await
            .unwrap()
            .latest
            .unwrap()
            .payload_len,
        png.len()
    );
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    assert_eq!(
        store
            .read_outbox_operation(&registration, &queued.operation_id)
            .await
            .unwrap(),
        Some(queued.clone())
    );
    store.prepare_outbox(&registration).await.unwrap();
    let newer = "<mxfile>later local edit</mxfile>";
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(1, newer))
        .await
        .unwrap();
    store
        .apply_outbox_commit(&registration, &receipt(&queued, "png-saved"))
        .await
        .unwrap();
    let draft = store
        .read_diagram_draft(&registration, NODE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            draft.generation,
            draft.xml.as_str(),
            draft.base_xml.as_str(),
            draft.base_revision.as_str()
        ),
        (2, newer, xml, "png-saved")
    );
    let second = store
        .queue_diagram_png(&registration, NODE, 2, &png)
        .await
        .unwrap();
    assert_eq!(second.expected_revision.as_deref(), Some("png-saved"));
    assert_ne!(second.operation_id, queued.operation_id);
    let replacement = RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..registration.clone()
    };
    store.activate_registration(&replacement).await.unwrap();
    let frozen = store
        .frozen_document_review(&replacement, &registration, NODE, 0, 20)
        .await
        .unwrap();
    assert_eq!(frozen.operations.len(), 1);
    assert_eq!(frozen.operations[0].operation_id, second.operation_id);
    store.close().await.unwrap();
    let mut recovery = notryx_client_store::RecoveryStore::open(&path)
        .await
        .unwrap();
    assert_eq!(
        recovery
            .operation_bytes(&second.operation_id)
            .await
            .unwrap(),
        png
    );
}

#[tokio::test]
async fn png_reexport_reuses_xml_snapshot_and_enforces_twenty_five_mib_limit() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let mut store = ClientStore::open(directory.path().join("client.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let xml = "<mxfile>same drawing</mxfile>";
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(0, xml))
        .await
        .unwrap();
    let mut png = vec![11; 25 * 1024 * 1024];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let queued = store
        .queue_diagram_png(&registration, NODE, 1, &png)
        .await
        .unwrap();
    store
        .save_diagram_draft(&registration, NODE, &checkpoint(1, xml))
        .await
        .unwrap();
    let different_encoding = b"\x89PNG\r\n\x1a\nreencoded";
    assert_eq!(
        store
            .queue_diagram_png(&registration, NODE, 2, different_encoding)
            .await
            .unwrap(),
        queued
    );
    png.push(0);
    assert!(
        store
            .queue_diagram_png(&registration, NODE, 2, &png)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .read_outbox_operation(&registration, &queued.operation_id)
            .await
            .unwrap(),
        Some(queued)
    );
}
