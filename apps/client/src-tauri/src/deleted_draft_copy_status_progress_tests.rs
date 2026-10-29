include!("deleted_draft_copy_recovery_tests.rs");

type StatusRequest = notryx_protocol::WorkspaceDeletedDraftCopyStatusRequest;
type ProgressRequest = notryx_protocol::WorkspaceDeletedDraftCopyProgressRequest;

fn status_request(rig: &DeletedCopyRig) -> StatusRequest {
    serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.document,
    }))
    .unwrap()
}

fn progress_request(rig: &DeletedCopyRig) -> ProgressRequest {
    serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.document,
        "node_id": rig.intent.node_id,
        "create_operation_id": rig.intent.create_operation_id,
    }))
    .unwrap()
}

async fn deleted_copy_status(rig: &DeletedCopyRig) -> serde_json::Value {
    let status =
        crate::remote_deleted_copy_native::status_for_renderer(&rig.runtime, status_request(rig))
            .await
            .unwrap()
            .unwrap();
    serde_json::to_value(status).unwrap()
}

async fn deleted_copy_progress(rig: &DeletedCopyRig) -> serde_json::Value {
    let progress = crate::remote_deleted_copy_native::progress_for_renderer(
        &rig.runtime,
        progress_request(rig),
    )
    .await
    .unwrap();
    serde_json::to_value(progress).unwrap()
}

#[tokio::test]
async fn deleted_copy_status_and_progress_resume_after_restart_until_content_ack() {
    let rig = deleted_copy_rig().await;
    let initial = deleted_copy_status(&rig).await;
    assert_eq!(
        initial["binding"],
        serde_json::to_value(crate::to_contract(rig.binding.clone())).unwrap()
    );
    assert_eq!(initial["document_id"], rig.document);
    assert_eq!(initial["node_id"], rig.intent.node_id);
    assert_eq!(
        initial["create_operation_id"],
        rig.intent.create_operation_id
    );
    assert_eq!(initial["parent_id"], rig.intent.parent_id);
    assert_eq!(initial["name"], rig.intent.name);
    assert_eq!(
        initial["local_hash"],
        notryx_protocol::content_hash(CONFIRMED)
    );
    assert_eq!(initial["create_status"], "not_queued");
    assert_eq!(initial["content_status"], "not_queued");
    assert!(initial.get("local_bytes").is_none());
    assert!(initial.get("bytes_base64").is_none());
    let create = deleted_copy_progress(&rig).await;
    assert_eq!(create["operation_id"], rig.intent.create_operation_id);
    assert_eq!(deleted_copy_progress(&rig).await, create);
    assert_eq!(deleted_copy_status(&rig).await["create_status"], "pending");
    let rig = restart(rig).await;
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state.gate.replace_owner(OWNER.into());
    }
    assert_eq!(deleted_copy_status(&rig).await["create_status"], "pending");
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    let status = deleted_copy_status(&rig).await;
    assert_eq!(status["create_status"], "created");
    assert_eq!(status["content_status"], "not_queued");
    let content = deleted_copy_progress(&rig).await;
    assert_ne!(content["operation_id"], rig.intent.create_operation_id);
    assert_eq!(deleted_copy_progress(&rig).await, content);
    assert_eq!(deleted_copy_status(&rig).await["content_status"], "pending");
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Committed);
    let status = deleted_copy_status(&rig).await;
    assert_eq!(status["content_status"], "saved");
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        CONFIRMED
    );
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_status_reports_name_conflict_after_restart_without_replacing_evidence() {
    let rig = deleted_copy_rig().await;
    deleted_copy_progress(&rig).await;
    let peer = rig
        .session_b
        .create(&CreateMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            request: CreateNodeRequest {
                node_id: Uuid::now_v7().to_string().parse().unwrap(),
                parent_id: rig.intent.parent_id.parse().unwrap(),
                name: rig.intent.name.clone(),
                kind: NodeKind::File,
                upload_id: None,
            },
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Created { .. }));
    let sent = crate::send::send_one(rig.runtime.0.clone(), OWNER)
        .await
        .unwrap();
    assert_eq!(sent.status, WorkspaceSendStatus::Failed);
    let rig = restart(rig).await;
    let status = deleted_copy_status(&rig).await;
    assert_eq!(status["create_status"], "needs_review");
    assert_eq!(status["create_failure"], "destination_exists");
    assert_eq!(status["content_status"], "not_queued");
    assert_eq!(
        status["create_operation_id"],
        rig.intent.create_operation_id
    );
    assert!(
        crate::remote_deleted_copy_native::progress_for_renderer(
            &rig.runtime,
            progress_request(&rig)
        )
        .await
        .is_err()
    );
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_status_reports_content_conflict_after_restart() {
    let rig = deleted_copy_rig().await;
    deleted_copy_progress(&rig).await;
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::Committed
    );
    deleted_copy_progress(&rig).await;
    let node = rig
        .session_b
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.intent.node_id)
        .unwrap();
    let peer = rig
        .session_b
        .put_content(&ContentMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: node.node_id,
            expected_revision: node.content_rev.unwrap(),
            bytes: b"peer copy content\n".to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Committed { .. }));
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        WorkspaceSendStatus::Conflict
    );
    let rig = restart(rig).await;
    let status = deleted_copy_status(&rig).await;
    assert_eq!(status["create_status"], "created");
    assert_eq!(status["content_status"], "needs_review");
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        b"peer copy content\n"
    );
    assert!(
        crate::remote_deleted_copy_native::progress_for_renderer(
            &rig.runtime,
            progress_request(&rig)
        )
        .await
        .is_err()
    );
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_progress_rejects_wrong_identity_or_registration_without_queueing() {
    let rig = deleted_copy_rig().await;
    let request = progress_request(&rig);
    for field in ["node_id", "create_operation_id", "binding", "owner"] {
        let mut value = serde_json::to_value(&request).unwrap();
        match field {
            "node_id" => value["node_id"] = serde_json::json!(Uuid::new_v4().to_string()),
            "create_operation_id" => {
                value["create_operation_id"] = serde_json::json!(Uuid::new_v4().to_string())
            }
            "binding" => {
                value["binding"]["device_stream_id"] = serde_json::json!(Uuid::new_v4().to_string())
            }
            "owner" => value["renderer_session_id"] = serde_json::json!("old-owner"),
            _ => unreachable!(),
        }
        let request = serde_json::from_value(value).unwrap();
        assert!(
            crate::remote_deleted_copy_native::progress_for_renderer(&rig.runtime, request)
                .await
                .is_err(),
            "{field}"
        );
        let status = deleted_copy_status(&rig).await;
        assert_eq!(status["create_status"], "not_queued");
        assert_eq!(status["content_status"], "not_queued");
    }
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}
