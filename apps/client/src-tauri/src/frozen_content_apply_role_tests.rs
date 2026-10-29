include!("frozen_content_apply_contract_tests.rs");

use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceRole};
use notryx_server::admin;

async fn editor_rig() -> (Rig, String) {
    let mut rig = single_operation_rig().await;
    let socket = rig._directory.path().join("admin.sock");
    let AdminResponse::Ok {
        result: AdminResult::Principal { principal_id },
    } = admin::request(
        &socket,
        &AdminRequest::CreatePrincipal {
            role: WorkspaceRole::Editor,
        },
    )
    .await
    .unwrap()
    else {
        panic!("editor principal");
    };
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = admin::request(
        &socket,
        &AdminRequest::IssuePairing {
            principal_id: principal_id.clone(),
        },
    )
    .await
    .unwrap()
    else {
        panic!("editor pairing");
    };
    let session = Arc::new(
        Session::pair(
            &format!("http://{}", rig.server.local_addr()),
            workspace_id,
            &pairing_secret,
        )
        .await
        .unwrap(),
    );
    let current = registration_binding(session.binding());
    {
        let mut state = rig.runtime.0.lock().await;
        let pair = state.gate.begin_pair("owner").unwrap();
        state.store.activate_registration(&current).await.unwrap();
        state.gate.commit_pair(&pair).unwrap();
        state.session = Some(session.clone());
    }
    let snapshot = rig.runtime.0.lock().await.gate.snapshot("owner").unwrap();
    bootstrap_metadata(&rig.runtime, &snapshot, &session)
        .await
        .unwrap();
    rig.current = current;
    rig.session = session;
    (rig, principal_id)
}

async fn change_role(rig: &Rig, principal_id: &str, role: WorkspaceRole) {
    assert!(matches!(
        admin::request(
            &rig._directory.path().join("admin.sock"),
            &AdminRequest::SetRole {
                principal_id: principal_id.into(),
                role,
            }
        )
        .await
        .unwrap(),
        AdminResponse::Ok { .. }
    ));
}

#[tokio::test]
async fn frozen_content_apply_role_denial_preserves_originals_and_creates_no_operation() {
    let (rig, principal) = editor_rig().await;
    let before = command_sources(&rig).await;
    let request = inspected_request(&rig).await;
    change_role(&rig, &principal, WorkspaceRole::Reader).await;
    assert_eq!(
        apply_frozen_content_for_renderer(&rig.runtime, request)
            .await
            .unwrap_err(),
        "write_forbidden"
    );
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_apply_role_failure_does_not_create_new_decision() {
    let rig = single_operation_rig().await;
    let before = command_sources(&rig).await;
    let request = inspected_request(&rig).await;
    rig.server.shutdown().await.unwrap();
    assert_eq!(
        apply_frozen_content_for_renderer(&rig.runtime, request)
            .await
            .unwrap_err(),
        "Network"
    );
    let mut state = rig.runtime.0.lock().await;
    assert!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap()
            == before.0
    );
    assert!(state.store.list_outbox(&rig.old, 0, 100).await.unwrap() == before.1);
    assert!(state.store.list_outbox(&rig.current, 0, 100).await.unwrap() == before.2);
}

#[tokio::test]
async fn frozen_content_apply_role_already_saved_decision_can_be_read_after_demotion() {
    let (rig, principal) = editor_rig().await;
    let request = inspected_request(&rig).await;
    let decision = apply_frozen_content_for_renderer(&rig.runtime, request.clone())
        .await
        .unwrap();
    let after = command_sources(&rig).await;
    change_role(&rig, &principal, WorkspaceRole::Reader).await;
    let repeated = apply_frozen_content_for_renderer(&rig.runtime, request)
        .await
        .unwrap();
    assert_eq!(repeated, decision);
    assert!(command_sources(&rig).await == after);
    rig.server.shutdown().await.unwrap();
}
