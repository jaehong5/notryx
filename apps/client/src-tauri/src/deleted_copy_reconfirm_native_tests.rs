include!("deleted_draft_copy_recovery_tests.rs");

type RetryRequest = notryx_protocol::WorkspaceDeletedDraftCopyReconfirmRequest;

async fn collided_copy_rig() -> DeletedCopyRig {
    let rig = deleted_copy_rig().await;
    rig.runtime
        .0
        .lock()
        .await
        .store
        .queue_deleted_draft_copy(&rig.binding, &rig.document)
        .await
        .unwrap();
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
    assert_eq!(
        sent.operation_id.as_deref(),
        Some(rig.intent.create_operation_id.as_str())
    );
    rig
}

fn retry_request(rig: &DeletedCopyRig, name: &str) -> RetryRequest {
    serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.document,
        "expected_create_operation_id": rig.intent.create_operation_id,
        "parent_id": rig.intent.parent_id,
        "name": name,
    }))
    .unwrap()
}

async fn reconfirm(
    rig: &DeletedCopyRig,
    request: RetryRequest,
) -> Result<serde_json::Value, String> {
    let result =
        crate::remote_deleted_copy_native::reconfirm_for_renderer(&rig.runtime, request).await?;
    Ok(serde_json::to_value(result).unwrap())
}

#[tokio::test]
async fn deleted_copy_reconfirm_catches_up_and_records_new_target_without_sending() {
    let rig = collided_copy_rig().await;
    let old = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap();
    let head = rig.session_a.server_info().await.unwrap().head_seq;
    let request = retry_request(&rig, "second-copy.md");
    let result = reconfirm(&rig, request.clone()).await.unwrap();
    assert_eq!(result["status"], "pending");
    assert_ne!(result["node_id"], rig.intent.node_id);
    assert_ne!(
        result["create_operation_id"],
        rig.intent.create_operation_id
    );
    let intent = {
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .sync_index(&rig.binding)
                .await
                .unwrap()
                .unwrap()
                .0,
            head
        );
        assert_eq!(
            state
                .store
                .list_create_operations(&rig.binding, 0, 100)
                .await
                .unwrap(),
            old
        );
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(intent.local_bytes, CONFIRMED);
    assert_eq!(intent.generation, rig.intent.generation);
    assert_eq!(intent.name, "second-copy.md");
    assert_eq!(result["node_id"], intent.node_id);
    assert_eq!(result["create_operation_id"], intent.create_operation_id);
    assert_eq!(reconfirm(&rig, request).await.unwrap(), result);
    assert_eq!(rig.session_a.server_info().await.unwrap().head_seq, head);
    assert!(
        !rig.config
            .workspace
            .notes_root
            .join("second-copy.md")
            .exists()
    );
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("deleted-copy.md")).unwrap(),
        b""
    );
    let checkpoint = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.bytes, LATER);
    assert_eq!(checkpoint.base_bytes, BASE);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_reconfirm_rejects_wrong_identity_and_fresh_destination_collision() {
    let rig = collided_copy_rig().await;
    let request = retry_request(&rig, "second-copy.md");
    let original = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .list_create_operations(&rig.binding, 0, 100)
        .await
        .unwrap();
    for field in [
        "owner",
        "binding",
        "expected_create_operation_id",
        "parent",
        "name",
    ] {
        let mut value = serde_json::to_value(&request).unwrap();
        match field {
            "owner" => value["renderer_session_id"] = serde_json::json!("old-owner"),
            "binding" => {
                value["binding"]["workspace_epoch"] = serde_json::json!(Uuid::new_v4().to_string())
            }
            "expected_create_operation_id" => {
                value[field] = serde_json::json!(Uuid::new_v4().to_string())
            }
            "parent" => value["parent_id"] = serde_json::json!(rig.document),
            "name" => value["name"] = serde_json::json!("../bad.md"),
            _ => unreachable!(),
        }
        assert!(
            reconfirm(&rig, serde_json::from_value(value).unwrap())
                .await
                .is_err(),
            "{field}"
        );
        assert_eq!(
            rig.runtime
                .0
                .lock()
                .await
                .store
                .read_deleted_draft_copy(&rig.binding, &rig.document)
                .await
                .unwrap(),
            Some(rig.intent.clone())
        );
    }
    let peer = rig
        .session_b
        .create(&CreateMutation {
            binding: rig.session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 3,
            request: CreateNodeRequest {
                node_id: Uuid::now_v7().to_string().parse().unwrap(),
                parent_id: rig.intent.parent_id.parse().unwrap(),
                name: "second-copy.md".into(),
                kind: NodeKind::File,
                upload_id: None,
            },
        })
        .await
        .unwrap();
    assert!(matches!(peer, OperationReceipt::Created { .. }));
    assert!(reconfirm(&rig, request).await.is_err());
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .list_create_operations(&rig.binding, 0, 100)
            .await
            .unwrap(),
        original
    );
    preserved(&rig).await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_reconfirm_retry_after_lost_response_is_available_offline() {
    let rig = collided_copy_rig().await;
    let request = retry_request(&rig, "second-copy.md");
    let result = reconfirm(&rig, request.clone()).await.unwrap();
    let DeletedCopyRig {
        directory: _directory,
        server,
        runtime,
        binding,
        document,
        ..
    } = rig;
    server.shutdown().await.unwrap();
    let retry = crate::remote_deleted_copy_native::reconfirm_for_renderer(&runtime, request)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(retry).unwrap(), result);
    let saved = runtime
        .0
        .lock()
        .await
        .store
        .read_deleted_draft_copy(&binding, &document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.local_bytes, CONFIRMED);
    assert_eq!(saved.node_id, result["node_id"]);
    assert_eq!(saved.create_operation_id, result["create_operation_id"]);
}
