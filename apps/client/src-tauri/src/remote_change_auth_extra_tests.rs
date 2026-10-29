use crate::{
    ProductSession, ProductSessionKind, registration_binding,
    remote_change_dirty::inspect_dirty_remote,
    send::TransmissionBlock,
    sync_bootstrap::{bootstrap_metadata, tests::setup},
};
use notryx_client_runtime::Error as NetworkError;
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
use notryx_server::admin;

#[tokio::test]
async fn dirty_inspection_of_revoked_device_blocks_auth_and_preserves_local_state() {
    let (directory, server, runtime, session, document) = setup().await;
    let binding = registration_binding(session.binding());
    let (snapshot, edit_session, draft_before, outbox_before, payload_before) = {
        let mut state = runtime.0.lock().await;
        let (opened, active) = state
            .store
            .try_open_registered_draft(&binding, &document)
            .await
            .unwrap()
            .expect("synthetic draft must exist");
        assert!(active);
        state
            .gate
            .open_session(opened.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            opened.edit_session_id.clone(),
            ProductSession {
                document_id: document.clone(),
                binding: binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let snapshot = state.gate.snapshot("owner").unwrap();
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .expect("synthetic checkpoint must exist");
        let outbox = state.store.list_outbox(&binding, 0, 100).await.unwrap();
        assert!(!outbox.entries.is_empty());
        let payload = state
            .store
            .read_outbox_operation(&binding, &outbox.entries[0].operation_id)
            .await
            .unwrap()
            .expect("synthetic outbox operation must exist");
        (snapshot, opened.edit_session_id, draft, outbox, payload)
    };
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();

    let revoke = admin::request(
        &directory.path().join("admin.sock"),
        &AdminRequest::RevokeDevice {
            device_id: binding.device_id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        revoke,
        AdminResponse::Ok {
            result: AdminResult::Done {}
        }
    ));
    assert!(matches!(
        session.server_info().await,
        Err(NetworkError::Unauthorized)
    ));

    let _ = inspect_dirty_remote(&runtime, "owner", &document, &edit_session, 1).await;
    {
        let mut state = runtime.0.lock().await;
        let draft_after = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .expect("synthetic checkpoint must remain");
        let outbox_after = state.store.list_outbox(&binding, 0, 100).await.unwrap();
        let payload_after = state
            .store
            .read_outbox_operation(&binding, &payload_before.operation_id)
            .await
            .unwrap()
            .expect("synthetic outbox operation must remain");
        assert!(draft_after == draft_before, "checkpoint or base changed");
        assert!(outbox_after == outbox_before, "outbox changed");
        assert!(payload_after == payload_before, "outbox payload changed");
        assert!(
            state
                .transmission_gate
                .has_block(&binding, TransmissionBlock::AuthRequired),
            "revoked device was not blocked for authentication"
        );
    }
    server.shutdown().await.unwrap();
}
