include!("content_conflict_choose_remote_guards.rs");

async fn committed_merge() -> (
    Rig,
    ClientStore,
    ContentConflictIntent,
    notryx_client_store::OutboxOperation,
) {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    request_and_finish_followup(&mut store, &rig, true).await;
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let merged = store
        .queue_content_conflict_merge(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap();
    let sent = store
        .prepare_outbox_matching(&rig.binding, &merged)
        .await
        .unwrap()
        .unwrap();
    store
        .apply_outbox_commit(
            &rig.binding,
            &notryx_client_store::OutboxCommit {
                operation_id: sent.operation_id.clone(),
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
                document_id: DOC.to_owned(),
                content_revision: "merged-commit-rev".to_owned(),
                content_hash: sent.payload_hash.clone(),
            },
        )
        .await
        .unwrap();
    store
        .confirm_receipt_ack(&rig.binding, &ack(&rig.binding, sent.operation_seq))
        .await
        .unwrap();
    (rig, store, intent, sent)
}

async fn preserved_rows(path: &Path, operation: &str, followup: &str, merged: &str) -> Vec<String> {
    let mut db = connect(path).await;
    let intent: String = sqlx::query_scalar("SELECT hex(base_bytes)||':'||hex(local_bytes)||':'||hex(remote_bytes)||':'||hex(merged_bytes)||':'||coalesce(merged_hash,'NULL') FROM content_conflict_intents WHERE operation_id=?")
        .bind(operation).fetch_one(&mut db).await.unwrap();
    let checkpoint: String = sqlx::query_scalar("SELECT c.generation||':'||hex(c.bytes)||':'||hex(b.bytes)||':'||b.revision FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id JOIN draft_bases b ON b.document_id=d.storage_id WHERE d.document_id=?")
        .bind(DOC).fetch_one(&mut db).await.unwrap();
    let mut result = vec![intent, checkpoint];
    for id in [operation, followup, merged] {
        let row: String = sqlx::query_scalar("SELECT operation_seq||':'||status||':'||hex(bytes)||':'||coalesce(expected_revision,'NULL')||':'||coalesce(committed_revision,'NULL') FROM client_outbox WHERE operation_id=?")
            .bind(id).fetch_one(&mut db).await.unwrap();
        result.push(row);
    }
    db.close().await.unwrap();
    result
}

async fn assert_corrupt_completion_rejected(case: &str) {
    let (rig, mut store, _intent, sent) = committed_merge().await;
    let mut db = connect(&rig.path).await;
    match case {
        "committed_revision" => {
            sqlx::query("UPDATE client_outbox SET committed_revision='forged-commit-rev' WHERE operation_id=?")
                .bind(&sent.operation_id).execute(&mut db).await.unwrap();
        }
        "status" => {
            sqlx::query("UPDATE client_outbox SET status='sent' WHERE operation_id=?")
                .bind(&sent.operation_id)
                .execute(&mut db)
                .await
                .unwrap();
        }
        "ack" => {
            sqlx::query("UPDATE outbox_streams SET ack_through=? WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active')")
                .bind((sent.operation_seq - 1) as i64).execute(&mut db).await.unwrap();
        }
        "confirmed" => {
            sqlx::query("UPDATE outbox_streams SET confirmed_through=? WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active')")
                .bind((sent.operation_seq + 1) as i64).execute(&mut db).await.unwrap();
        }
        "base_bytes" => {
            sqlx::query("UPDATE draft_bases SET bytes=? WHERE document_id=(SELECT storage_id FROM registered_documents WHERE document_id=?)")
                .bind(b"tampered base\n".as_slice()).bind(DOC).execute(&mut db).await.unwrap();
        }
        "base_revision" => {
            sqlx::query("UPDATE draft_bases SET revision='forged-base-rev' WHERE document_id=(SELECT storage_id FROM registered_documents WHERE document_id=?)")
                .bind(DOC).execute(&mut db).await.unwrap();
        }
        "checkpoint_bytes" => {
            sqlx::query("UPDATE draft_checkpoints SET bytes=? WHERE document_id=(SELECT storage_id FROM registered_documents WHERE document_id=?)")
                .bind(b"tampered checkpoint\n".as_slice()).bind(DOC).execute(&mut db).await.unwrap();
        }
        _ => panic!("unknown case"),
    }
    db.close().await.unwrap();
    let before = preserved_rows(&rig.path, &rig.operation, &rig.followup, &sent.operation_id).await;
    let result = store
        .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
        .await;
    let conflict = store.first_content_conflict(&rig.binding, DOC).await;
    let document = store.document_outbox_state(&rig.binding, DOC).await;
    let after = preserved_rows(&rig.path, &rig.operation, &rig.followup, &sent.operation_id).await;
    assert_eq!(before, after, "case={case}");
    assert!(result.is_err(), "case={case}, result={result:?}");
    assert!(
        conflict.is_err() || conflict.unwrap().is_some(),
        "case={case}"
    );
    assert!(
        document.is_err() || document.unwrap().has_conflict,
        "case={case}"
    );
}

macro_rules! corrupt_case {
    ($name:ident, $case:literal) => {
        #[tokio::test]
        async fn $name() {
            assert_corrupt_completion_rejected($case).await;
        }
    };
}

corrupt_case!(
    merged_completion_rejects_committed_revision_inconsistent_with_base,
    "committed_revision"
);
corrupt_case!(
    merged_completion_rejects_sent_status_after_confirmed_ack,
    "status"
);
corrupt_case!(merged_completion_rejects_local_ack_behind_confirmed, "ack");
corrupt_case!(
    merged_completion_rejects_confirmed_ack_ahead_of_local_ack,
    "confirmed"
);
corrupt_case!(merged_completion_rejects_wrong_base_bytes, "base_bytes");
corrupt_case!(
    merged_completion_rejects_wrong_base_revision,
    "base_revision"
);
corrupt_case!(
    merged_completion_rejects_wrong_checkpoint_at_merge_generation,
    "checkpoint_bytes"
);

#[tokio::test]
async fn merged_commit_is_pending_when_confirmed_ack_is_behind() {
    let (rig, mut store, _intent, sent) = committed_merge().await;
    let mut db = connect(&rig.path).await;
    sqlx::query("UPDATE outbox_streams SET confirmed_through=? WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active')")
        .bind((sent.operation_seq - 1) as i64).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(matches!(
        store
            .read_content_conflict_resolution_state(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        notryx_client_store::ContentConflictResolutionState::Pending { .. }
    ));
}
