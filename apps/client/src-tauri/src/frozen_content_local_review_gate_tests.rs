include!("frozen_content_local_dismiss_contract_tests.rs");

use crate::send::TransmissionBlock;

#[tokio::test]
async fn frozen_content_local_review_gate_contract_existing_sync_or_auth_block_keeps_local_sources_readable()
 {
    for block in [
        TransmissionBlock::InitialSync,
        TransmissionBlock::ResyncRequired,
        TransmissionBlock::AuthRequired,
    ] {
        let rig = single_operation_rig().await;
        let before = command_sources(&rig).await;
        {
            let mut state = rig.runtime.0.lock().await;
            state.transmission_gate.block(&rig.current, block);
        }
        let review = local_review(&rig, &rig.sent.operation_id).await.unwrap();
        assert_local_review_sources(&review, &rig, &rig.sent);
        assert_eq!(review.remote_evidence, LocalRemoteEvidence::Unknown);
        assert_eq!(
            local_dismiss_for_renderer(&rig.runtime, "owner", review)
                .await
                .unwrap_err(),
            "stale_sync"
        );
        assert_eq!(dismiss_status(&rig, "owner").await.unwrap(), None);
        assert!(command_sources(&rig).await == before);
        rig.server.shutdown().await.unwrap();
    }
}
