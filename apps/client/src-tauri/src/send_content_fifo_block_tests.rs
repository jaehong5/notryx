use crate::conflict_snapshot_native::test_support::snapshot::*;

use sqlx::Connection;

async fn followup(rig: &Rig) -> OutboxOperation {
    let mut state = rig.runtime.0.lock().await;
    let last = state.store.document_outbox_state(&rig.binding, &rig.document).await.unwrap().latest.unwrap();
    state.store.read_outbox_operation(&rig.binding, &last.operation_id).await.unwrap().unwrap()
}

fn block_value(result: &notryx_protocol::WorkspaceSendResult) -> serde_json::Value {
    serde_json::to_value(result).unwrap()["content_block"].clone()
}

async fn assert_conflict_preserved(rig: &Rig, next: &OutboxOperation) {
    let mut state = rig.runtime.0.lock().await;
    let original = state.store.read_outbox_operation(&rig.binding, &rig.operation.operation_id).await.unwrap().unwrap();
    let later = state.store.read_outbox_operation(&rig.binding, &next.operation_id).await.unwrap().unwrap();
    assert_eq!(original.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(original.operation_seq, rig.operation.operation_seq);
    assert_eq!(original.bytes, LOCAL);
    assert_eq!(later.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(later.operation_seq, next.operation_seq);
    assert_eq!(later.expected_revision, None);
    assert_eq!(later.bytes, b"newer local\n");
    let draft = state.store.read_registered_checkpoint(&rig.binding, &rig.document).await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, rig.initial.content_rev.as_ref().unwrap().as_str());
}

#[tokio::test]
async fn fifo_content_412_block_is_typed_without_ui_observing_original_receipt() {
    let rig = rig_with_followup(true).await;
    let next = followup(&rig).await;
    assert_eq!(next.expected_revision, None);
    let head = rig.session.server_info().await.unwrap().head_seq;
    let result = crate::send::send_one(rig.runtime.0.clone(), OWNER).await.unwrap();
    assert_eq!(result.status, notryx_protocol::WorkspaceSendStatus::NeedsReview);
    assert_eq!(crate::registration_binding(rig.session.binding()), rig.binding);
    assert_eq!(serde_json::to_value(&result).unwrap()["binding"], serde_json::to_value(crate::to_contract(rig.binding.clone())).unwrap());
    assert_eq!(block_value(&result), serde_json::json!({
        "conflict_operation_id":rig.operation.operation_id,
        "blocked_operation_id":next.operation_id,
        "blocked_seq":next.operation_seq.to_string(),
    }));
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
    assert_conflict_preserved(&rig, &next).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

async fn reorder_unsent_second_and_third(rig: &Rig, second_id: &str, third_id: &str) {
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(path)).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    sqlx::query("UPDATE client_outbox SET operation_seq=99 WHERE operation_id=? AND operation_seq=2").bind(third_id).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE client_outbox SET operation_seq=2 WHERE operation_id=? AND operation_seq=3").bind(second_id).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE client_outbox SET operation_seq=3 WHERE operation_id=? AND operation_seq=99").bind(third_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_preceding_content_or_delete_is_sent_before_classifying_conflict() {
    for delete in [false, true] {
        let rig = rig_with_followup(true).await;
        let initial_followup = followup(&rig).await;
        let node = rig.session.inventory(None).await.unwrap().nodes.into_iter().find(|node| node.name == "move.md").unwrap();
        let other_id = if delete {
            rig.runtime.0.lock().await.store.queue_delete(&rig.binding, node.node_id, &node.node_etag).await.unwrap().operation_id
        } else {
            let mut state = rig.runtime.0.lock().await;
            let id = node.node_id.to_string();
            let opened = state.store.open_registered_document(&rig.binding, &id, b"move source\n", node.content_rev.as_ref().unwrap().as_str()).await.unwrap();
            state.store.save_registered_checkpoint(&rig.binding, &id, &opened.edit_session_id, 1, b"other edit\n").await.unwrap();
            state.store.close_registered_document(&rig.binding, &id, &opened.edit_session_id).await.unwrap();
            state.store.queue_checkpoint(&rig.binding, &id, 1).await.unwrap().operation_id
        };
        reorder_unsent_second_and_third(&rig, &other_id, &initial_followup.operation_id).await;
        let first = crate::send::send_one(rig.runtime.0.clone(), OWNER).await.unwrap();
        assert_eq!(first.status, notryx_protocol::WorkspaceSendStatus::Committed);
        assert_eq!(first.operation_id.as_deref(), Some(other_id.as_str()));
        assert!(block_value(&first).is_null());
        let next = followup(&rig).await;
        assert_eq!(next.operation_seq, 3);
        let blocked = crate::send::send_one(rig.runtime.0.clone(), OWNER).await.unwrap();
        assert_eq!(blocked.status, notryx_protocol::WorkspaceSendStatus::NeedsReview);
        assert_eq!(block_value(&blocked), serde_json::json!({
            "conflict_operation_id":rig.operation.operation_id,
            "blocked_operation_id":next.operation_id,
            "blocked_seq":"3",
        }));
        assert_conflict_preserved(&rig, &next).await;
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn authentication_and_reconciliation_are_not_classified_as_content_conflict_block() {
    for reconciliation in [false, true] {
        let rig = rig_with_followup(true).await;
        let next = followup(&rig).await;
        if reconciliation {
            let path = rig._directory.path().join("conflict-snapshot.sqlite");
            let mut db = sqlx::SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(path)).await.unwrap();
            let mut tx = db.begin().await.unwrap();
            let id: i64 = sqlx::query_scalar("INSERT INTO client_registrations(server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,status) VALUES(?,?,?,?,?,'frozen') RETURNING id")
                .bind(&rig.binding.server_origin).bind(&rig.binding.workspace_id).bind(&rig.binding.workspace_epoch)
                .bind(Uuid::new_v4().to_string()).bind(Uuid::new_v4().to_string()).fetch_one(&mut *tx).await.unwrap();
            sqlx::query("INSERT INTO registered_documents(registration_id,document_id,storage_id) VALUES(?,?,'synthetic-frozen')").bind(id).bind(&rig.document).execute(&mut *tx).await.unwrap();
            sqlx::query("INSERT INTO draft_checkpoints(document_id,generation,format_version,bytes) VALUES('synthetic-frozen',1,1,X'66726f7a656e')").execute(&mut *tx).await.unwrap();
            sqlx::query("INSERT INTO draft_bases(document_id,bytes,revision) VALUES('synthetic-frozen',X'62617365','old-base')").execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            db.close().await.unwrap();
        } else {
            rig.runtime.0.lock().await.transmission_gate.block(&rig.binding, crate::send::TransmissionBlock::AuthRequired);
        }
        let result = crate::send::send_one(rig.runtime.0.clone(), OWNER).await;
        match result {
            Ok(result) => {
                assert!(block_value(&result).is_null());
                assert_ne!(result.status, notryx_protocol::WorkspaceSendStatus::Committed);
            }
            Err(error) => assert!(reconciliation && error.contains("ReconciliationRequired"), "{error}"),
        }
        assert_conflict_preserved(&rig, &next).await;
        assert_eq!(std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(), REMOTE);
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
