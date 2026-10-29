include!("remote_deleted_copy_confirm_tests.rs");

type DeletedSnapshotRequest = notryx_protocol::WorkspaceDeletedDraftCopySnapshotRequest;

fn deleted_snapshot_request(rig: &Rig) -> DeletedSnapshotRequest {
    serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.node.node_id.to_string(),
        "edit_session_id": rig.edit_session,
        "generation": "1"
    }))
    .unwrap()
}

async fn prepare_deleted_snapshot(
    rig: &Rig,
    request: DeletedSnapshotRequest,
) -> Result<serde_json::Value, String> {
    let snapshot =
        crate::remote_deleted_copy_native::prepare_for_renderer(&rig.runtime, request).await?;
    Ok(serde_json::to_value(snapshot).unwrap())
}

#[tokio::test]
async fn prepare_deleted_copy_snapshot_returns_confirmed_durable_identity_after_catchup() {
    let rig = rig().await;
    let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
    let request = deleted_snapshot_request(&rig);
    let snapshot = prepare_deleted_snapshot(&rig, request).await.unwrap();
    assert_eq!(
        snapshot["binding"],
        serde_json::to_value(crate::to_contract(rig.binding.clone())).unwrap()
    );
    assert_eq!(snapshot["document_id"], rig.node.node_id.to_string());
    assert_eq!(snapshot["edit_session_id"], rig.edit_session);
    assert_eq!(snapshot["generation"], "1");
    assert_eq!(snapshot["local_hash"], DELETED_LOCAL_HASH);
    assert_eq!(
        snapshot["tombstone_seq"],
        deleted.last_change_seq.get().to_string()
    );
    assert!(snapshot.get("bytes").is_none());
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap()
            .is_none()
    );
    assert_original_preserved(&rig, b"local first\n").await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn prepare_deleted_copy_snapshot_rejects_live_restored_and_stale_generation() {
    let rig = rig().await;
    let request = deleted_snapshot_request(&rig);
    assert!(
        prepare_deleted_snapshot(&rig, request.clone())
            .await
            .is_err()
    );
    let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
    let restored = rig
        .peer
        .restore(&RestoreMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: rig.node.node_id,
            request: RestoreNodeRequest {
                expected_meta_rev: deleted.meta_rev,
                parent_id: rig.node.parent_id.unwrap(),
                name: rig.node.name.clone(),
            },
        })
        .await
        .unwrap();
    let OperationReceipt::Restored { node: restored, .. } = restored else {
        panic!("restore receipt")
    };
    assert!(
        prepare_deleted_snapshot(&rig, request.clone())
            .await
            .is_err()
    );
    deleted_by_peer(&rig, 3, &restored).await;
    save_next(&rig).await;
    assert!(prepare_deleted_snapshot(&rig, request).await.is_err());
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap()
            .is_none()
    );
    assert_original_preserved(&rig, b"local second\n").await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn prepare_deleted_copy_snapshot_rejects_owner_session_or_checkpoint_changed_during_catchup()
{
    for transition in ["owner", "session", "checkpoint"] {
        let (rig, mut observed, release, proxy) = delayed_deleted_copy_rig().await;
        deleted_by_peer(&rig, 1, &rig.node).await;
        let request = deleted_snapshot_request(&rig);
        let runtime = Runtime(rig.runtime.0.clone());
        let task = tokio::spawn(async move {
            crate::remote_deleted_copy_native::prepare_for_renderer(&runtime, request).await
        });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = rig.runtime.0.lock().await;
            match transition {
                "owner" => state.gate.replace_owner("new-owner".into()),
                "session" => {
                    let document = rig.node.node_id.to_string();
                    state
                        .store
                        .close_registered_document(&rig.binding, &document, &rig.edit_session)
                        .await
                        .unwrap();
                    state.gate.close_session(&rig.edit_session).unwrap();
                    state.product_sessions.remove(&rig.edit_session);
                }
                "checkpoint" => {
                    state
                        .store
                        .save_registered_checkpoint(
                            &rig.binding,
                            &rig.node.node_id.to_string(),
                            &rig.edit_session,
                            2,
                            b"local later\n",
                        )
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
        }
        release.send(()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "{transition}"
        );
        assert!(
            rig.runtime
                .0
                .lock()
                .await
                .store
                .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        assert_original_preserved(
            &rig,
            if transition == "checkpoint" {
                b"local later\n"
            } else {
                b"local first\n"
            },
        )
        .await;
        proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
