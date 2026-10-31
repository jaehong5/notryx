include!("frozen_content_inspection_tests.rs");

use crate::frozen_content_native::inspect_frozen_content_command_for_renderer;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use notryx_protocol::{
    NodeState, WorkspaceFrozenContentInspectRequest, WorkspaceOutboxStatus, content_hash,
};
use std::time::Duration;

const COMMAND_UNIMPLEMENTED: &str = "frozen_content_command_unimplemented";

fn frozen_command_request(rig: &Rig) -> WorkspaceFrozenContentInspectRequest {
    WorkspaceFrozenContentInspectRequest {
        renderer_session_id: "owner".into(),
        current_binding: crate::to_contract(rig.current.clone()),
        old_binding: crate::to_contract(rig.old.clone()),
        operation_id: rig.sent.operation_id.clone(),
    }
}

async fn command_sources(
    rig: &Rig,
) -> (
    notryx_client_store::PersistedDraft,
    notryx_client_store::OutboxPage,
    notryx_client_store::OutboxPage,
) {
    let mut state = rig.runtime.0.lock().await;
    (
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap(),
        state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        state.store.list_outbox(&rig.current, 0, 100).await.unwrap(),
    )
}

fn decoded(bytes: &str, hash: &str) -> Vec<u8> {
    let decoded = STANDARD.decode(bytes).unwrap();
    assert_eq!(STANDARD.encode(&decoded), bytes);
    assert_eq!(content_hash(&decoded), hash);
    decoded
}

async fn set_remote_bytes(rig: &Rig, bytes: &[u8]) -> NodeState {
    std::fs::write(rig._directory.path().join("notes/move.md"), bytes).unwrap();
    let expected_hash = content_hash(bytes);
    let until = tokio::time::Instant::now() + Duration::from_secs(60);
    let node = loop {
        assert!(
            tokio::time::Instant::now() < until,
            "external file change was not observed"
        );
        if let Ok(inventory) = rig.session.inventory(None).await
            && let Some(node) = inventory
                .nodes
                .into_iter()
                .find(|node| node.node_id.to_string() == rig.document)
            && node.content_hash.as_deref() == Some(expected_hash.as_str())
            && node.size.as_deref() == Some(bytes.len().to_string().as_str())
        {
            break node;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let snapshot = rig.runtime.0.lock().await.gate.snapshot("owner").unwrap();
    // A published revision does not mean the server has finished its object rechecks.
    tokio::time::timeout_at(until, async {
        loop {
            match crate::sync_catchup::catch_up_metadata(&rig.runtime, &snapshot, &rig.session)
                .await
            {
                Err(error) if error == "server_not_ready" => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                result => break result,
            }
        }
    })
    .await
    .expect("server did not settle after the external synthetic change")
    .unwrap();
    node
}

#[tokio::test]
async fn frozen_content_command_returns_exact_separate_versions_from_real_server() {
    let rig = rig().await;
    let before = command_sources(&rig).await;
    let remote = rig
        .session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.document)
        .unwrap();
    let result =
        inspect_frozen_content_command_for_renderer(&rig.runtime, frozen_command_request(&rig))
            .await
            .unwrap();
    assert_eq!(
        result.current_binding,
        crate::to_contract(rig.current.clone())
    );
    assert_eq!(result.old_binding, crate::to_contract(rig.old.clone()));
    assert_eq!(result.document_id, rig.document);
    assert_eq!(result.operation_id, rig.sent.operation_id);
    assert_eq!(result.operation_seq, rig.sent.operation_seq.to_string());
    assert_eq!(result.operation_status, WorkspaceOutboxStatus::Sent);
    assert_eq!(result.operation_generation, "1");
    assert_eq!(result.local_generation, "2");
    assert!(result.outcome_unknown);
    assert_eq!(
        result.expected_revision.as_deref(),
        Some(rig.base_revision.as_str())
    );
    assert_eq!(result.base_revision, rig.base_revision);
    assert_eq!(decoded(&result.base_bytes_base64, &result.base_hash), BASE);
    assert_eq!(
        decoded(&result.operation_bytes_base64, &result.operation_hash),
        SENT
    );
    assert_eq!(
        decoded(&result.local_bytes_base64, &result.local_hash),
        LOCAL
    );
    assert_eq!(
        decoded(&result.remote_bytes_base64, &result.remote_hash),
        SENT
    );
    assert_eq!(result.remote_revision, remote.content_rev.unwrap().as_str());
    let mut next_request = frozen_command_request(&rig);
    next_request.operation_id = before.1.entries[1].operation_id.clone();
    let queued = inspect_frozen_content_command_for_renderer(&rig.runtime, next_request)
        .await
        .unwrap();
    assert_eq!(queued.operation_status, WorkspaceOutboxStatus::Queued);
    assert!(!queued.outcome_unknown);
    assert_eq!(queued.operation_generation, "2");
    assert_eq!(queued.expected_revision, None);
    assert_eq!(
        decoded(&queued.operation_bytes_base64, &queued.operation_hash),
        LOCAL
    );
    assert_eq!(before.1.entries[0].status, OutboxStatus::Sent);
    assert!(before.1.entries[0].frozen);
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_command_rejects_untrusted_owner_binding_and_operation_without_mutation() {
    let rig = rig().await;
    let before = command_sources(&rig).await;
    let valid = frozen_command_request(&rig);
    let result = inspect_frozen_content_command_for_renderer(&rig.runtime, valid.clone())
        .await
        .unwrap();
    assert_eq!(
        decoded(&result.local_bytes_base64, &result.local_hash),
        LOCAL
    );
    let mut invalid = Vec::new();
    let mut request = valid.clone();
    request.renderer_session_id = "other-owner".into();
    invalid.push(request);
    let mut request = valid.clone();
    request.current_binding = crate::to_contract(rig.old.clone());
    invalid.push(request);
    let mut request = valid.clone();
    request.old_binding = crate::to_contract(rig.current.clone());
    invalid.push(request);
    let mut request = valid.clone();
    request.operation_id = uuid::Uuid::now_v7().to_string();
    invalid.push(request);
    let mut request = valid;
    request.operation_id = "not-a-uuid".into();
    invalid.push(request);
    for request in invalid {
        let result = inspect_frozen_content_command_for_renderer(&rig.runtime, request).await;
        assert!(matches!(result, Err(ref error) if error != COMMAND_UNIMPLEMENTED));
        assert!(command_sources(&rig).await == before);
    }
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_command_preserves_boundary_and_non_utf8_remote_bytes() {
    let rig = rig().await;
    let before = command_sources(&rig).await;
    for bytes in [
        vec![b'a'; 5 * 1024 * 1024],
        vec![b'b'; 5 * 1024 * 1024 + 1],
        vec![0xff, 0xfe, 0x80, b'\n'],
        vec![b'c'; 25 * 1024 * 1024],
    ] {
        let node = set_remote_bytes(&rig, &bytes).await;
        // The watcher can start another object check after publishing the new bytes.
        let result = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match inspect_frozen_content_command_for_renderer(
                    &rig.runtime,
                    frozen_command_request(&rig),
                )
                .await
                {
                    Ok(result) => break result,
                    Err(error) if error == "resync_required" => {
                        let snapshot = rig.runtime.0.lock().await.gate.snapshot("owner").unwrap();
                        match crate::sync_catchup::catch_up_metadata(
                            &rig.runtime,
                            &snapshot,
                            &rig.session,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(error) if error == "server_not_ready" => {}
                            Err(error) => {
                                panic!("catch-up failed for {} bytes: {error}", bytes.len())
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(error) => panic!("inspection failed for {} bytes: {error}", bytes.len()),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("inspection did not settle for {} bytes", bytes.len()));
        assert_eq!(
            decoded(&result.remote_bytes_base64, &result.remote_hash),
            bytes
        );
        assert_eq!(result.remote_revision, node.content_rev.unwrap().as_str());
        assert_eq!(
            decoded(&result.operation_bytes_base64, &result.operation_hash),
            SENT
        );
        assert_eq!(
            decoded(&result.local_bytes_base64, &result.local_hash),
            LOCAL
        );
        assert!(command_sources(&rig).await == before);
    }
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_command_handles_synthetic_over_25_mib_metadata_without_touching_sources() {
    let rig = rig().await;
    let before = command_sources(&rig).await;
    let valid =
        inspect_frozen_content_command_for_renderer(&rig.runtime, frozen_command_request(&rig))
            .await
            .unwrap();
    assert_eq!(decoded(&valid.local_bytes_base64, &valid.local_hash), LOCAL);
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(
            rig._directory
                .path()
                .join("frozen-content-inspection.sqlite"),
        ),
    )
    .await
    .unwrap();
    let row: String = sqlx::query_scalar("SELECT n.state FROM remote_nodes n JOIN client_registrations r ON r.id=n.registration_id WHERE r.status='active' AND n.node_id=?")
        .bind(&rig.document)
        .fetch_one(&mut db)
        .await
        .unwrap();
    let mut node: NodeState = serde_json::from_str(&row).unwrap();
    node.size = Some((25 * 1024 * 1024 + 1).to_string());
    let changed = serde_json::to_string(&node).unwrap();
    let updated = sqlx::query("UPDATE remote_nodes SET state=? WHERE registration_id=(SELECT id FROM client_registrations WHERE status='active') AND node_id=?")
        .bind(changed)
        .bind(&rig.document)
        .execute(&mut db)
        .await
        .unwrap();
    assert_eq!(updated.rows_affected(), 1);
    db.close().await.unwrap();
    let active = {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .sync_index(&rig.current)
            .await
            .unwrap()
            .unwrap()
            .1
    };
    assert_eq!(
        active
            .into_iter()
            .find(|item| item.node_id == node.node_id)
            .unwrap()
            .size,
        node.size
    );
    let result =
        inspect_frozen_content_command_for_renderer(&rig.runtime, frozen_command_request(&rig))
            .await;
    match result {
        Err(error) => assert!(
            error
                .to_ascii_lowercase()
                .replace('_', "")
                .contains("toolarge")
        ),
        Ok(verified) => {
            assert_eq!(
                decoded(&verified.remote_bytes_base64, &verified.remote_hash),
                SENT
            );
            assert_eq!(
                decoded(&verified.local_bytes_base64, &verified.local_hash),
                LOCAL
            );
            assert_eq!(
                decoded(&verified.operation_bytes_base64, &verified.operation_hash),
                SENT
            );
        }
    }
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}
