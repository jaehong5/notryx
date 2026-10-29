include!("conflict_snapshot_server_tests.rs");

use notryx_client_store::{
    ContentConflictChoice, ContentConflictIntent, ContentConflictIntentRequest,
    ContentConflictResolutionState, OutboxStatus,
};
use sqlx::Connection;

const MERGED: &[u8] = b"\xef\xbb\xbfmerged \xed\x95\x9c\xea\xb8\x80\n";
const LATER: &[u8] = b"\xef\xbb\xbfmerged \xed\x95\x9c\xea\xb8\x80\nlater edit\n";

async fn queue_approved_merge(rig: &Rig, version: u64) -> Result<OutboxOperation, String> {
    crate::conflict_snapshot_native::queue_merge_for_renderer(
        &rig.runtime,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.edit_session,
        &rig.operation.operation_id,
        version,
    )
    .await
}

async fn merged_intent(rig: &Rig) -> ContentConflictIntent {
    rig.runtime
        .0
        .lock()
        .await
        .store
        .save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 2, MERGED)
        .await
        .unwrap();
    let shown = prepare_conflict_snapshot(
        &rig.runtime,
        OWNER,
        &rig.document,
        &rig.edit_session,
        2,
        &rig.operation.operation_id,
    )
    .await
    .unwrap();
    assert_eq!(shown.local_bytes, MERGED);
    rig.runtime
        .0
        .lock()
        .await
        .store
        .record_content_conflict_intent(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &ContentConflictIntentRequest {
                generation: shown.generation,
                remote_revision: shown.remote_revision,
                remote_hash: rig.first_peer.content_hash.clone().unwrap(),
                remote_bytes: REMOTE.to_vec(),
                choice: ContentConflictChoice::SaveMerged {
                    bytes: MERGED.to_vec(),
                },
            },
        )
        .await
        .unwrap()
}

async fn assert_merge_queue_sources(rig: &Rig) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, MERGED);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        draft.base_revision,
        rig.initial.content_rev.as_ref().unwrap().as_str()
    );
    let page = state.store.list_outbox(&rig.binding, 0, 100).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    let original = state
        .store
        .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.status, OutboxStatus::Conflict);
    assert_eq!(original.bytes, LOCAL);
    assert_eq!(original.payload_hash, rig.operation.payload_hash);
    assert_eq!(original.expected_revision, rig.operation.expected_revision);
    assert_eq!(original.operation_seq, rig.operation.operation_seq);
}

async fn confirm_original_ack(rig: &Rig) {
    let ack = rig
        .session
        .acknowledge(rig.operation.operation_seq)
        .await
        .unwrap();
    rig.runtime
        .0
        .lock()
        .await
        .store
        .confirm_receipt_ack(&rig.binding, &ack)
        .await
        .unwrap();
}

#[tokio::test]
async fn merged_native_queue_waits_for_ack_retires_session_and_preserves_later_edit() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    let head_before = rig.session.server_info().await.unwrap().head_seq;
    assert!(queue_approved_merge(&rig, intent.version).await.is_err());
    assert_merge_queue_sources(&rig).await;
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .product_sessions
            .contains_key(&rig.edit_session)
    );
    confirm_original_ack(&rig).await;

    let queued = queue_approved_merge(&rig, intent.version).await.unwrap();
    assert_eq!(queued.document_id, rig.document);
    assert_ne!(queued.operation_id, rig.operation.operation_id);
    assert_eq!(queued.operation_seq, rig.operation.operation_seq + 1);
    assert_eq!(queued.generation, intent.generation + 1);
    assert_eq!(
        queued.expected_revision.as_deref(),
        Some(intent.remote_revision.as_str())
    );
    assert_eq!(queued.bytes, MERGED);
    assert_eq!(queued.status, OutboxStatus::Queued);
    assert_eq!(
        rig.session.server_info().await.unwrap().head_seq,
        head_before
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    {
        let mut state = rig.runtime.0.lock().await;
        assert!(!state.product_sessions.contains_key(&rig.edit_session));
        assert!(state.gate.check_session(&rig.edit_session).is_err());
        let retry = state
            .store
            .queue_content_conflict_merge(&rig.binding, &rig.operation.operation_id, intent.version)
            .await
            .unwrap();
        assert_eq!(retry, queued);
        assert!(matches!(
            state
                .store
                .read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            ContentConflictResolutionState::Pending { .. }
        ));
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, queued.generation);
        assert_eq!(draft.bytes, MERGED);
        assert_eq!(draft.base_bytes, REMOTE);
        assert_eq!(draft.base_revision, intent.remote_revision);
        assert!(
            state
                .store
                .save_registered_checkpoint(
                    &rig.binding,
                    &rig.document,
                    &rig.edit_session,
                    queued.generation + 1,
                    LATER
                )
                .await
                .is_err()
        );
    }

    let (status, opened) =
        crate::conflict_snapshot_native::reopen_conflict_checkpoint_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &rig.edit_session,
        )
        .await
        .unwrap();
    assert!(matches!(
        status,
        ContentConflictResolutionState::Pending { .. }
    ));
    assert_ne!(opened.edit_session_id, rig.edit_session);
    assert_eq!(opened.generation, queued.generation.to_string());
    let followup = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.document,
                &opened.edit_session_id,
                queued.generation + 1,
                LATER,
            )
            .await
            .unwrap();
        let followup = state
            .store
            .queue_checkpoint(&rig.binding, &rig.document, queued.generation + 1)
            .await
            .unwrap();
        assert_ne!(followup.operation_id, queued.operation_id);
        assert_eq!(followup.operation_seq, queued.operation_seq + 1);
        assert_eq!(followup.bytes, LATER);
        let stored = state
            .store
            .read_outbox_operation(&rig.binding, &queued.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.bytes, MERGED);
        assert_eq!(
            stored.expected_revision.as_deref(),
            Some(intent.remote_revision.as_str())
        );
        followup
    };
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(queued.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        MERGED
    );
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap()
            >= queued.operation_seq
    );
    assert!(
        matches!(state.store.read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id).await.unwrap(),
        ContentConflictResolutionState::Completed { generation, frozen: false } if generation == queued.generation)
    );
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, queued.generation + 1);
    assert_eq!(draft.bytes, LATER);
    let later = state
        .store
        .read_outbox_operation(&rig.binding, &followup.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(later.bytes, LATER);
    assert_eq!(later.status, OutboxStatus::Queued);
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        LOCAL
    );
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merged_native_queue_storage_failure_restores_session_and_all_sources() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let database = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&database),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_merge_link BEFORE INSERT ON content_conflict_merged_operations BEGIN SELECT RAISE(ABORT, 'synthetic queue failure'); END")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(queue_approved_merge(&rig, intent.version).await.is_err());
    assert_merge_queue_sources(&rig).await;
    {
        let state = rig.runtime.0.lock().await;
        let active = state.product_sessions.get(&rig.edit_session).unwrap();
        assert_eq!(active.document_id, rig.document);
        assert_eq!(active.binding, rig.binding);
        assert!(active.kind == ProductSessionKind::Active);
        assert!(state.gate.check_session(&rig.edit_session).is_ok());
    }
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(&database),
    )
    .await
    .unwrap();
    sqlx::raw_sql("DROP TRIGGER reject_merge_link")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let queued = queue_approved_merge(&rig, intent.version).await.unwrap();
    assert_eq!(queued.bytes, MERGED);
    assert_eq!(queued.operation_seq, rig.operation.operation_seq + 1);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
