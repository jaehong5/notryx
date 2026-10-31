use crate::conflict_snapshot_native::test_support::snapshot::*;

use sqlx::Connection;

#[tokio::test]
async fn pending_lookup_rejects_missing_history_and_preserves_conflict_evidence() {
    for remove_all in [false, true] {
        let rig = rig().await;
        let shown = prepare(&rig).await.unwrap();
        let request = notryx_client_store::ContentConflictIntentRequest {
            generation: shown.generation,
            remote_revision: shown.remote_revision.clone(),
            remote_hash: rig.first_peer.content_hash.clone().unwrap(),
            remote_bytes: shown.remote_bytes.clone(),
            choice: notryx_client_store::ContentConflictChoice::SaveMerged {
                bytes: b"manual merge\n".to_vec(),
            },
        };
        let (current, before_draft, before_operation) = {
            let mut state = rig.runtime.0.lock().await;
            let first = state
                .store
                .record_content_conflict_intent(
                    &rig.binding,
                    &rig.document,
                    &rig.operation.operation_id,
                    &request,
                )
                .await
                .unwrap();
            let mut next = request.clone();
            next.choice = notryx_client_store::ContentConflictChoice::ChooseRemote;
            let second = state
                .store
                .replace_pending_content_conflict_intent(
                    &rig.binding,
                    &rig.document,
                    &rig.operation.operation_id,
                    &first,
                    &next,
                )
                .await
                .unwrap();
            next.choice = notryx_client_store::ContentConflictChoice::KeepLocalCopy;
            let current = state
                .store
                .replace_pending_content_conflict_intent(
                    &rig.binding,
                    &rig.document,
                    &rig.operation.operation_id,
                    &second,
                    &next,
                )
                .await
                .unwrap();
            assert_eq!(current.version, 2);
            assert_eq!(
                state
                    .store
                    .read_content_conflict_intent_history(
                        &rig.binding,
                        &rig.operation.operation_id,
                        0,
                    )
                    .await
                    .unwrap(),
                Some(first)
            );
            assert_eq!(
                state
                    .store
                    .read_content_conflict_intent_history(
                        &rig.binding,
                        &rig.operation.operation_id,
                        1,
                    )
                    .await
                    .unwrap(),
                Some(second)
            );
            let draft = state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap();
            let operation = state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap();
            (current, draft, operation)
        };
        let path = rig._directory.path().join("conflict-snapshot.sqlite");
        let mut db = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        let removed = if remove_all {
            sqlx::query("DELETE FROM content_conflict_intent_history WHERE operation_id=?")
                .bind(&rig.operation.operation_id)
                .execute(&mut db)
                .await
                .unwrap()
                .rows_affected()
        } else {
            sqlx::query("DELETE FROM content_conflict_intent_history WHERE operation_id=? AND history_seq=2")
                .bind(&rig.operation.operation_id).execute(&mut db).await.unwrap().rows_affected()
        };
        assert_eq!(removed, if remove_all { 2 } else { 1 });
        db.close().await.unwrap();

        let result = crate::conflict_snapshot_native::read_pending_content_conflict_intent(
            &rig.runtime,
            OWNER,
            &rig.document,
            &rig.edit_session,
            &rig.operation.operation_id,
        )
        .await;
        assert!(result.is_err(), "remove_all={remove_all}");
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            Some(current)
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap(),
            before_draft
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            before_operation
        );
        assert!(state.product_sessions.contains_key(&rig.edit_session));
        assert!(
            state
                .store
                .dirty_registered_session_snapshot(&rig.binding, &rig.document, &rig.edit_session)
                .await
                .unwrap()
                .is_some()
        );
        state.gate.check_session(&rig.edit_session).unwrap();
        drop(state);
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
            REMOTE
        );
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
