include!("frozen_document_apply.rs");

use sha2::{Digest, Sha256};

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

#[tokio::test]
async fn frozen_document_apply_corruption_rejects_impossible_checkpoint_operation_relationship() {
    for mode in ["generation", "bytes"] {
        let mut rig = apply_rig().await;
        let mut db =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
                .await
                .unwrap();
        if mode == "generation" {
            sqlx::query("UPDATE client_outbox SET generation=2 WHERE operation_id=?")
                .bind(&rig.approval.old_operation_id)
                .execute(&mut db)
                .await
                .unwrap();
        } else {
            let changed = b"different old payload";
            rig.approval.old_operation_hash = digest(changed);
            sqlx::query("UPDATE client_outbox SET bytes=?,payload_len=?,payload_hash=? WHERE operation_id=?")
                .bind(changed.as_slice()).bind(changed.len() as i64).bind(&rig.approval.old_operation_hash)
                .bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        let before = snapshot(&rig.path).await;
        assert!(
            apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
                .await
                .is_err(),
            "mode {mode}"
        );
        assert_eq!(snapshot(&rig.path).await, before);
        assert!(
            rig.store
                .next_pending_outbox(&rig.approval.current)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn frozen_document_apply_corruption_keeps_barrier_when_reviewed_operation_changes() {
    for mode in [
        "generation",
        "seq",
        "expected",
        "dependency",
        "length",
        "retired",
    ] {
        let mut rig = apply_rig().await;
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&rig.path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        match mode {
            "generation" => sqlx::query("UPDATE client_outbox SET generation=2 WHERE operation_id=?").bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
            "seq" => sqlx::query("UPDATE client_outbox SET operation_seq=99 WHERE operation_id=?").bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
            "expected" => sqlx::query("UPDATE client_outbox SET expected_revision='unexpected-revision' WHERE operation_id=?").bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
            "dependency" => sqlx::query("UPDATE client_outbox SET dependency=? WHERE operation_id=?").bind(&rig.other_operation_id).bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
            "length" => sqlx::query("UPDATE client_outbox SET payload_len=payload_len+1 WHERE operation_id=?").bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
            "retired" => sqlx::query("UPDATE client_outbox SET payload_retired=1 WHERE operation_id=?").bind(&rig.approval.old_operation_id).execute(&mut db).await.unwrap(),
            _ => unreachable!(),
        };
        db.close().await.unwrap();
        let before = snapshot(&rig.path).await;
        let result = rig.store.prepare_outbox(&rig.approval.current).await;
        assert!(
            matches!(result, Err(StoreError::ReconciliationRequired)),
            "mode {mode}: {result:?}"
        );
        assert_eq!(snapshot(&rig.path).await, before);
    }
}
