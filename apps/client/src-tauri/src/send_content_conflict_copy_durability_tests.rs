use crate::conflict_snapshot_native::test_support::send_copy_content::*;

use sqlx::Connection;

async fn copy_store_db(rig: &Rig) -> sqlx::SqliteConnection {
    sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(rig._directory.path().join("conflict-snapshot.sqlite")),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn copy_content_insert_or_link_failure_rolls_back_all_rows_and_sequence() {
    for fail_link in [false, true] {
        let rig = rig_with_followup(true).await;
        let (copy, _, _) = created_copy(&rig).await;
        let mut db = copy_store_db(&rig).await;
        let before: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=1")
                .fetch_one(&mut db)
                .await
                .unwrap();
        let trigger = if fail_link {
            "CREATE TRIGGER fail_copy_content BEFORE UPDATE OF content_operation_id ON content_conflict_copies WHEN NEW.content_operation_id IS NOT NULL BEGIN SELECT RAISE(ABORT, 'injected'); END"
        } else {
            "CREATE TRIGGER fail_copy_content BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='content' BEGIN SELECT RAISE(ABORT, 'injected'); END"
        };
        sqlx::query(trigger).execute(&mut db).await.unwrap();
        assert!(
            rig.runtime
                .0
                .lock()
                .await
                .store
                .queue_content_conflict_copy_content(
                    &rig.binding,
                    &rig.operation.operation_id,
                    copy.intent_version
                )
                .await
                .is_err()
        );
        let next: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=1")
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(next, before);
        let docs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM registered_documents WHERE registration_id=1 AND document_id=?",
        )
        .bind(&copy.node_id)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(docs, 0);
        let content: i64 = sqlx::query_scalar("SELECT count(*) FROM client_outbox WHERE registration_id=1 AND document_id=? AND operation_kind='content'").bind(&copy.node_id).fetch_one(&mut db).await.unwrap();
        assert_eq!(content, 0);
        let linked: Option<String> = sqlx::query_scalar(
            "SELECT content_operation_id FROM content_conflict_copies WHERE operation_id=?",
        )
        .bind(&rig.operation.operation_id)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert!(linked.is_none());
        sqlx::query("DROP TRIGGER fail_copy_content")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        preserve_original(&rig, &copy).await;
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
        assert_eq!(item.operation_seq, before as u64);
        assert_eq!(item.bytes, b"newer local\n");
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn copy_content_queue_reopens_with_exact_same_id_sequence_and_bytes() {
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
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let binding = rig.binding.clone();
    let operation_id = rig.operation.operation_id.clone();
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
    drop(rig.runtime);
    let mut reopened = ClientStore::open(&path).await.unwrap();
    let retry = reopened
        .queue_content_conflict_copy_content(&binding, &operation_id, copy.intent_version)
        .await
        .unwrap();
    assert_eq!(retry, item);
    assert_eq!(
        retry.expected_revision.as_deref(),
        Some(created.content_rev.as_ref().unwrap().as_str())
    );
    let draft = reopened
        .read_registered_checkpoint(&binding, &copy.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, b"");
    assert_eq!(
        reopened
            .read_content_conflict_copy(&binding, &operation_id)
            .await
            .unwrap(),
        Some(copy)
    );
}
