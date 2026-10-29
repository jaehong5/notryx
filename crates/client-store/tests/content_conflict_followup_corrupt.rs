include!("content_conflict_intent.rs");

async fn reject_inconsistent_followup_generation_and_bytes(damage: &str) {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    let ack: notryx_protocol::AckResponse = serde_json::from_value(json!({
        "workspace_epoch":rig.binding.workspace_epoch, "device_stream_id":rig.binding.device_stream_id,
        "last_seq":"1", "retired_through":"1"
    })).unwrap();
    store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    let shown = store
        .inspect_content_conflict_merge_followup(&rig.binding, &rig.operation, intent.version)
        .await
        .unwrap()
        .unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    match damage {
        "future_generation" => {
            sqlx::query("UPDATE client_outbox SET generation=3 WHERE operation_id=?")
                .bind(&rig.followup)
                .execute(&mut db)
                .await
                .unwrap();
        }
        "same_generation_other_bytes" => {
            let other = b"different queued bytes\n";
            sqlx::query("UPDATE client_outbox SET bytes=?,payload_len=?,payload_hash=? WHERE operation_id=?")
                .bind(other.as_slice()).bind(other.len() as i64).bind(digest(other)).bind(&rig.followup)
                .execute(&mut db).await.unwrap();
        }
        _ => unreachable!(),
    }
    let before: (i64, Vec<u8>, String, String, String) = sqlx::query_as(
        "SELECT generation,bytes,payload_hash,status,abandon_state FROM client_outbox WHERE operation_id=?",
    ).bind(&rig.followup).fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    assert!(
        store
            .request_content_conflict_merge_followup(
                &rig.binding,
                &rig.operation,
                intent.version,
                &shown.operation_id,
                shown.operation_seq,
                shown.generation,
                &shown.payload_hash
            )
            .await
            .is_err(),
        "{damage}"
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, LOCAL);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, "base-rev");
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    let after: (i64, Vec<u8>, String, String, String) = sqlx::query_as(
        "SELECT generation,bytes,payload_hash,status,abandon_state FROM client_outbox WHERE operation_id=?",
    ).bind(&rig.followup).fetch_one(&mut db).await.unwrap();
    assert_eq!(after, before, "{damage}");
    assert_eq!(after.4, "none");
    db.close().await.unwrap();
}

#[tokio::test]
async fn future_followup_generation_is_not_covered_by_approved_intent() {
    reject_inconsistent_followup_generation_and_bytes("future_generation").await;
}

#[tokio::test]
async fn same_generation_followup_bytes_must_match_approved_checkpoint() {
    reject_inconsistent_followup_generation_and_bytes("same_generation_other_bytes").await;
}
