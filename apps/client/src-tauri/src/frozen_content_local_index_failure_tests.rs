include!("frozen_content_local_review_gate_tests.rs");

#[derive(Debug)]
struct IndexFailureReview {
    local: LocalDismissReview,
    remote_index_error: bool,
}

async fn local_review_with_index_diagnostic(rig: &Rig) -> Result<IndexFailureReview, String> {
    let local = local_review(rig, &rig.sent.operation_id).await?;
    let remote_index_error = local.inspected.remote_index_error;
    Ok(IndexFailureReview {
        local,
        remote_index_error,
    })
}

async fn damage_active_index(rig: &Rig) {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(
            rig._directory
                .path()
                .join("frozen-content-inspection.sqlite"),
        ),
    )
    .await
    .unwrap();
    let update = sqlx::query("UPDATE remote_nodes SET state='synthetic invalid metadata' WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active') AND node_id=?")
        .bind(&rig.document).execute(&mut db).await.unwrap();
    assert_eq!(update.rows_affected(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn frozen_content_local_index_failure_contract_reports_invalid_index_and_preserves_readable_sources()
 {
    for blocked in [false, true] {
        let rig = single_operation_rig().await;
        let before = command_sources(&rig).await;
        let evidence = saved_operation_evidence(&rig).await;
        damage_active_index(&rig).await;
        if blocked {
            rig.runtime
                .0
                .lock()
                .await
                .transmission_gate
                .block(&rig.current, TransmissionBlock::ResyncRequired);
        }
        let result = local_review_with_index_diagnostic(&rig).await.unwrap();
        assert!(result.remote_index_error);
        assert_eq!(result.local.remote_evidence, LocalRemoteEvidence::Unknown);
        assert_local_review_sources(&result.local, &rig, &rig.sent);
        let error = local_dismiss_for_renderer(&rig.runtime, "owner", result.local)
            .await
            .unwrap_err();
        assert!(matches!(
            error.as_str(),
            "stale_sync" | "resync_required" | "UnsupportedFormat"
        ));
        assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
        assert!(command_sources(&rig).await == before);
        assert_eq!(saved_operation_evidence(&rig).await, evidence);
        rig.server.shutdown().await.unwrap();
    }
}
