include!("frozen_content_apply_contract_tests.rs");

use notryx_client_runtime::{CreateMutation, MoveMutation};
use notryx_protocol::{CreateNodeRequest, MoveNodeRequest, NodeKind};
use uuid::Uuid;

#[derive(Clone, Debug)]
struct CrossEpochTarget {
    node_id: String,
    parent_id: String,
    name: String,
    path: String,
}

#[derive(Clone, Debug)]
struct CrossEpochReview {
    inspected: notryx_protocol::WorkspaceFrozenContentInspection,
    current_target: CrossEpochTarget,
}

#[derive(Clone)]
struct CrossEpochApplyRequest {
    renderer_session_id: String,
    review: CrossEpochReview,
    same_node_is_not_identity: bool,
}

async fn review_cross_epoch_for_renderer(rig: &Rig) -> Result<CrossEpochReview, String> {
    let result = crate::frozen_content_native::inspect_frozen_content_cross_epoch_for_renderer(
        &rig.runtime,
        frozen_command_request(rig),
    )
    .await?;
    Ok(CrossEpochReview {
        inspected: result.inspected,
        current_target: CrossEpochTarget {
            node_id: result.current_target.node_id,
            parent_id: result.current_target.parent_id,
            name: result.current_target.name,
            path: result.current_target.path,
        },
    })
}

async fn apply_cross_epoch_for_renderer(
    runtime: &Runtime,
    request: CrossEpochApplyRequest,
) -> Result<FrozenContentApplyResult, String> {
    let target = request.review.current_target;
    crate::frozen_content_native::apply_frozen_content_cross_epoch_for_renderer(
        runtime,
        notryx_protocol::WorkspaceFrozenContentCrossEpochApplyRequest {
            renderer_session_id: request.renderer_session_id,
            inspected: notryx_protocol::WorkspaceFrozenContentCrossEpochInspection {
                inspected: request.review.inspected,
                current_target: notryx_protocol::WorkspaceFrozenContentCrossEpochTarget {
                    node_id: target.node_id,
                    parent_id: target.parent_id,
                    name: target.name,
                    path: target.path,
                },
            },
            same_node_is_not_identity: request.same_node_is_not_identity,
        },
    )
    .await
}

async fn cross_epoch_rig() -> Rig {
    let mut rig = single_operation_rig().await;
    let old_epoch = Uuid::new_v4().to_string();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(
            rig._directory
                .path()
                .join("frozen-content-inspection.sqlite"),
        ),
    )
    .await
    .unwrap();
    let changed = sqlx::query("UPDATE client_registrations SET workspace_epoch=? WHERE status='frozen' AND workspace_epoch=?")
        .bind(&old_epoch).bind(&rig.old.workspace_epoch).execute(&mut db).await.unwrap();
    assert_eq!(changed.rows_affected(), 1);
    db.close().await.unwrap();
    rig.old.workspace_epoch = old_epoch;
    rig.sent.binding = rig.old.clone();
    assert_ne!(rig.old.workspace_epoch, rig.current.workspace_epoch);
    rig
}

fn cross_epoch_request(review: CrossEpochReview) -> CrossEpochApplyRequest {
    CrossEpochApplyRequest {
        renderer_session_id: "owner".into(),
        review,
        same_node_is_not_identity: true,
    }
}

async fn epoch_peer(rig: &Rig) -> Session {
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: rig.server.local_addr(),
            admin_socket: rig._directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: rig._directory.path().join("notes"),
            state_root: rig._directory.path().join("server-state"),
            uploads: Default::default(),
        },
    };
    let pairing = server_test_support::pairing_request(&config).await.0;
    Session::pair(
        &format!("http://{}", rig.server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap()
}

async fn refresh_index(rig: &Rig) {
    let gate = rig.runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session)
        .await
        .unwrap();
}

async fn old_payload_and_receipt(rig: &Rig) -> (Vec<u8>, Option<String>) {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(
                rig._directory
                    .path()
                    .join("frozen-content-inspection.sqlite"),
            )
            .read_only(true),
    )
    .await
    .unwrap();
    let result =
        sqlx::query_as("SELECT bytes, receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(&rig.sent.operation_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    db.close().await.unwrap();
    result
}

async fn nested_target(rig: &Rig, peer: &Session) -> (NodeState, NodeState) {
    let node = peer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.document)
        .unwrap();
    let root = node.parent_id.unwrap();
    let folder_id: notryx_domain::NodeId = Uuid::now_v7().to_string().parse().unwrap();
    let created = peer
        .create(&CreateMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            request: CreateNodeRequest {
                node_id: folder_id,
                parent_id: root,
                name: "folder".into(),
                kind: NodeKind::Folder,
                upload_id: None,
            },
        })
        .await
        .unwrap();
    let OperationReceipt::Created { node: folder, .. } = created else {
        panic!("synthetic folder create")
    };
    let moved = peer
        .move_node(&MoveMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: node.node_id,
            expected_meta_etag: node.meta_etag.clone(),
            request: MoveNodeRequest {
                expected_parent_id: root,
                parent_id: folder_id,
                name: node.name.clone(),
            },
        })
        .await
        .unwrap();
    let OperationReceipt::Moved { node, .. } = moved else {
        panic!("synthetic document move")
    };
    refresh_index(rig).await;
    (folder, node)
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_contract_requires_explicit_target_and_preserves_old_data()
{
    let rig = cross_epoch_rig().await;
    let before = command_sources(&rig).await;
    let payload_receipt = old_payload_and_receipt(&rig).await;
    assert!(payload_receipt.1.is_none());
    let old_operation = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_outbox_operation(&rig.old, &rig.sent.operation_id)
        .await
        .unwrap()
        .unwrap();
    let marks = stream_watermarks(
        &rig._directory
            .path()
            .join("frozen-content-inspection.sqlite"),
    )
    .await;
    let review = review_cross_epoch_for_renderer(&rig).await.unwrap();
    assert_eq!(
        review.inspected.old_binding.workspace_epoch,
        rig.old.workspace_epoch
    );
    assert_eq!(
        review.inspected.current_binding.workspace_epoch,
        rig.current.workspace_epoch
    );
    assert_ne!(
        review.inspected.old_binding.workspace_epoch,
        review.inspected.current_binding.workspace_epoch
    );
    assert_eq!(review.current_target.node_id, rig.document);
    assert_eq!(review.current_target.path, "/move.md");
    assert_eq!(review.current_target.name, "move.md");
    let node = rig
        .session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.document)
        .unwrap();
    assert_eq!(
        review.current_target.parent_id,
        node.parent_id.unwrap().to_string()
    );
    assert!(review.inspected.outcome_unknown);
    assert_eq!(
        decoded(
            &review.inspected.local_bytes_base64,
            &review.inspected.local_hash
        ),
        LOCAL
    );
    assert_eq!(
        decoded(
            &review.inspected.remote_bytes_base64,
            &review.inspected.remote_hash
        ),
        SENT
    );
    let mut no_confirmation = cross_epoch_request(review.clone());
    no_confirmation.same_node_is_not_identity = false;
    assert!(
        apply_cross_epoch_for_renderer(&rig.runtime, no_confirmation)
            .await
            .is_err()
    );
    assert!(command_sources(&rig).await == before);
    let result = apply_cross_epoch_for_renderer(&rig.runtime, cross_epoch_request(review.clone()))
        .await
        .unwrap();
    let after = command_sources(&rig).await;
    assert_eq!(after.0, before.0);
    assert_eq!(after.1, before.1);
    assert_eq!(after.2.entries.len(), 1);
    let (queued, original) = {
        let mut state = rig.runtime.0.lock().await;
        (
            state
                .store
                .read_outbox_operation(&rig.current, &result.operation_id)
                .await
                .unwrap()
                .unwrap(),
            state
                .store
                .read_outbox_operation(&rig.old, &rig.sent.operation_id)
                .await
                .unwrap()
                .unwrap(),
        )
    };
    assert_eq!(queued.bytes, LOCAL);
    assert_eq!(queued.status, OutboxStatus::Queued);
    assert_eq!(
        queued.expected_revision.as_deref(),
        Some(review.inspected.remote_revision.as_str())
    );
    assert!(original == old_operation);
    assert_eq!(old_payload_and_receipt(&rig).await, payload_receipt);
    assert_eq!(
        stream_watermarks(
            &rig._directory
                .path()
                .join("frozen-content-inspection.sqlite")
        )
        .await[0],
        marks[0]
    );
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
    rig.runtime.0.lock().await.session = None;
    let repeated = apply_cross_epoch_for_renderer(&rig.runtime, cross_epoch_request(review))
        .await
        .unwrap();
    assert_eq!(repeated, result);
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_contract_changed_ancestor_path_requires_new_confirmation()
{
    let rig = cross_epoch_rig().await;
    let peer = epoch_peer(&rig).await;
    let (folder, document) = nested_target(&rig, &peer).await;
    let before = command_sources(&rig).await;
    let review = review_cross_epoch_for_renderer(&rig).await.unwrap();
    assert_eq!(review.current_target.path, "/folder/move.md");
    assert_eq!(review.current_target.node_id, rig.document);
    assert_eq!(review.current_target.parent_id, folder.node_id.to_string());
    assert_eq!(review.current_target.name, "move.md");
    let moved = peer
        .move_node(&MoveMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 3,
            node_id: folder.node_id,
            expected_meta_etag: folder.meta_etag.clone(),
            request: MoveNodeRequest {
                expected_parent_id: folder.parent_id.unwrap(),
                parent_id: folder.parent_id.unwrap(),
                name: "renamed".into(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(moved, OperationReceipt::Moved { .. }));
    let after = peer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == document.node_id)
        .unwrap();
    assert_eq!(after.content_rev, document.content_rev);
    assert_eq!(after.meta_rev, document.meta_rev);
    refresh_index(&rig).await;
    let error = apply_cross_epoch_for_renderer(&rig.runtime, cross_epoch_request(review))
        .await
        .unwrap_err();
    assert_eq!(error, "stale_inspection");
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/renamed/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_contract_rejects_changed_target_epochs_or_snapshot() {
    let rig = cross_epoch_rig().await;
    let before = command_sources(&rig).await;
    let payload_receipt = old_payload_and_receipt(&rig).await;
    let other_node = rig
        .session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "example.md")
        .unwrap()
        .node_id
        .to_string();
    let review = review_cross_epoch_for_renderer(&rig).await.unwrap();
    let mut variants = Vec::new();
    let mut value = review.clone();
    value.current_target.path = "/other/move.md".into();
    variants.push((value, "stale_inspection"));
    let mut value = review.clone();
    value.current_target.node_id = other_node;
    variants.push((value, "InvalidRequest"));
    let mut value = review.clone();
    value.current_target.parent_id = Uuid::now_v7().to_string();
    variants.push((value, "stale_inspection"));
    let mut value = review.clone();
    value.current_target.name = "wrong.md".into();
    variants.push((value, "stale_inspection"));
    let mut value = review.clone();
    value.inspected.old_binding.workspace_epoch = Uuid::new_v4().to_string();
    variants.push((value, "StaleRegistration"));
    let mut value = review.clone();
    value.inspected.current_binding.workspace_epoch = Uuid::new_v4().to_string();
    variants.push((value, "stale_registration"));
    let mut value = review.clone();
    value.inspected.old_binding.workspace_id = Uuid::new_v4().to_string();
    variants.push((value, "unsupported_registration"));
    let mut value = review.clone();
    value.inspected.local_bytes_base64 = STANDARD.encode(b"different local\n");
    value.inspected.local_hash = content_hash(b"different local\n");
    variants.push((value, "stale_inspection"));
    let mut value = review.clone();
    value.inspected.remote_bytes_base64 = STANDARD.encode(b"different remote\n");
    value.inspected.remote_hash = content_hash(b"different remote\n");
    variants.push((value, "stale_inspection"));
    for (index, (value, expected)) in variants.into_iter().enumerate() {
        let error = apply_cross_epoch_for_renderer(&rig.runtime, cross_epoch_request(value))
            .await
            .unwrap_err();
        assert_eq!(error, expected, "variant {index}");
    }
    assert!(command_sources(&rig).await == before);
    assert_eq!(old_payload_and_receipt(&rig).await, payload_receipt);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_contract_put_follows_node_after_later_ancestor_move() {
    let rig = cross_epoch_rig().await;
    let peer = epoch_peer(&rig).await;
    let (folder, document) = nested_target(&rig, &peer).await;
    let before = command_sources(&rig).await;
    let review = review_cross_epoch_for_renderer(&rig).await.unwrap();
    assert_eq!(review.current_target.path, "/folder/move.md");
    let result = apply_cross_epoch_for_renderer(&rig.runtime, cross_epoch_request(review))
        .await
        .unwrap();
    let moved = peer
        .move_node(&MoveMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 3,
            node_id: folder.node_id,
            expected_meta_etag: folder.meta_etag.clone(),
            request: MoveNodeRequest {
                expected_parent_id: folder.parent_id.unwrap(),
                parent_id: folder.parent_id.unwrap(),
                name: "after-approval".into(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(moved, OperationReceipt::Moved { .. }));
    let current = peer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == document.node_id)
        .unwrap();
    assert_eq!(current.content_rev, document.content_rev);
    let sent = crate::send::send_one(rig.runtime.0.clone(), "owner")
        .await
        .unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Committed);
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(result.operation_id.as_str())
    );
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/after-approval/move.md")).unwrap(),
        LOCAL
    );
    let after = command_sources(&rig).await;
    assert_eq!(after.0, before.0);
    assert_eq!(after.1, before.1);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_contract_changed_remote_bytes_requires_recomparison() {
    let rig = cross_epoch_rig().await;
    let before = command_sources(&rig).await;
    let review = review_cross_epoch_for_renderer(&rig).await.unwrap();
    set_remote_bytes(&rig, b"another current epoch content\n").await;
    assert_eq!(
        apply_cross_epoch_for_renderer(&rig.runtime, cross_epoch_request(review))
            .await
            .unwrap_err(),
        "stale_inspection"
    );
    assert!(command_sources(&rig).await == before);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        b"another current epoch content\n"
    );
    rig.server.shutdown().await.unwrap();
}
