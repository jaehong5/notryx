include!("send_content_conflict_copy_content_tests.rs");

use notryx_client_store::ContentConflictCopyContentState;
use sqlx::Connection;

async fn status(
    rig: &Rig,
    version: u64,
) -> Result<ContentConflictCopyContentState, notryx_client_store::StoreError> {
    rig.runtime
        .0
        .lock()
        .await
        .store
        .read_content_conflict_copy_content_state(
            &rig.binding,
            &rig.operation.operation_id,
            version,
        )
        .await
}

#[tokio::test]
async fn copy_status_requires_valid_committed_evidence_and_confirmed_ack() {
    let rig = rig_with_followup(true).await;
    let (copy, create, _) = created_copy(&rig).await;
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::NotQueued
    );
    let item = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_content_conflict_copy_content(
            &rig.binding,
            &rig.operation.operation_id,
            copy.intent_version,
        )
        .await
        .unwrap();
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::Pending {
            operation_id: item.operation_id.clone()
        }
    );
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::Committed
    );
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::Saved {
            operation_id: item.operation_id.clone()
        }
    );
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE outbox_streams SET confirmed_through=? WHERE registration_id=1")
        .bind(create.operation_seq as i64)
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::Pending {
            operation_id: item.operation_id.clone()
        }
    );
    sqlx::query("UPDATE outbox_streams SET confirmed_through=? WHERE registration_id=1")
        .bind(item.operation_seq as i64)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE client_outbox SET committed_revision=NULL WHERE operation_id=?")
        .bind(&item.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    assert!(status(&rig, copy.intent_version).await.is_err());
    db.close().await.unwrap();
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_status_reports_competing_peer_edit_without_claiming_saved() {
    let rig = rig_with_followup(true).await;
    let (copy, _, created) = created_copy(&rig).await;
    let item = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .queue_content_conflict_copy_content(
            &rig.binding,
            &rig.operation.operation_id,
            copy.intent_version,
        )
        .await
        .unwrap();
    let peer = rig
        .peer
        .put_content(&ContentMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: created.node_id,
            expected_revision: created.content_rev.unwrap(),
            bytes: b"peer copy edit\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Committed { .. }));
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::Conflict
    );
    assert_eq!(
        status(&rig, copy.intent_version).await.unwrap(),
        ContentConflictCopyContentState::NeedsReview {
            operation_id: item.operation_id
        }
    );
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
