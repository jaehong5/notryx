include!("remote_change_tests.rs");

use notryx_client_runtime::{DeleteMutation, RestoreMutation};
use notryx_client_store::DeletedDraftCopyEvidence;
use notryx_protocol::RestoreNodeRequest;
use std::time::Duration;

const DELETED_LOCAL_HASH: &str =
    "sha256:c9c40669ded86ab1713bd97bf6d11e5e8a862dd0be24c351eb9023daf2a928d1";
type DeletedCopyRequest = notryx_protocol::WorkspaceDeletedDraftCopyConfirmRequest;

async fn deleted_by_peer(rig: &Rig, seq: u64, node: &NodeState) -> NodeState {
    let receipt = rig
        .peer
        .delete(&DeleteMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: seq,
            node_id: node.node_id,
            expected_node_etag: node.node_etag.clone(),
        })
        .await
        .unwrap();
    let OperationReceipt::Deleted { node, .. } = receipt else {
        panic!("delete receipt")
    };
    assert!(node.deleted);
    node
}

fn deleted_copy_request(rig: &Rig, deleted: &NodeState) -> DeletedCopyRequest {
    serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.node.node_id.to_string(),
        "edit_session_id": rig.edit_session,
        "generation": "1",
        "local_hash": DELETED_LOCAL_HASH,
        "parent_id": rig.node.parent_id.unwrap().to_string(),
        "name": "recovered.md",
        "tombstone_seq": deleted.last_change_seq.get().to_string()
    }))
    .unwrap()
}

async fn confirm_deleted_copy(
    rig: &Rig,
    request: DeletedCopyRequest,
) -> Result<serde_json::Value, String> {
    let result =
        crate::remote_deleted_copy_native::confirm_for_renderer(&rig.runtime, request).await?;
    Ok(serde_json::to_value(result).unwrap())
}

async fn assert_original_preserved(rig: &Rig, expected: &[u8]) {
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, expected);
    assert_eq!(draft.base_bytes, b"server source\n");
    assert!(
        state
            .store
            .list_create_operations(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn deleted_copy_confirmation_catches_up_to_fixed_head_and_persists_only_confirmed_intent() {
    let rig = rig().await;
    let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
    let head = rig.peer.server_info().await.unwrap().head_seq;
    let request = deleted_copy_request(&rig, &deleted);
    let response = confirm_deleted_copy(&rig, request.clone()).await.unwrap();
    assert_eq!(response["status"], "pending");
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
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(intent.local_bytes, b"local first\n");
    assert_eq!(intent.generation, 1);
    assert_eq!(intent.parent_id, rig.node.parent_id.unwrap().to_string());
    assert_eq!(intent.name, "recovered.md");
    assert_eq!(
        intent.evidence,
        DeletedDraftCopyEvidence::Tombstone {
            last_change_seq: deleted.last_change_seq
        }
    );
    assert_eq!(response["node_id"], intent.node_id);
    assert_eq!(response["create_operation_id"], intent.create_operation_id);
    assert_ne!(intent.node_id, rig.node.node_id.to_string());
    assert_ne!(intent.node_id, intent.create_operation_id);
    assert_eq!(
        Uuid::parse_str(&intent.node_id).unwrap().get_version_num(),
        7
    );
    assert_eq!(
        Uuid::parse_str(&intent.create_operation_id)
            .unwrap()
            .get_version_num(),
        7
    );
    assert_eq!(confirm_deleted_copy(&rig, request).await.unwrap(), response);
    assert_eq!(rig.peer.server_info().await.unwrap().head_seq, head);
    assert_original_preserved(&rig, b"local first\n").await;
    assert!(!rig._directory.path().join("notes/recovered.md").exists());
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_confirmation_rejects_stale_displayed_identity_and_destination() {
    let rig = rig().await;
    let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
    let request = deleted_copy_request(&rig, &deleted);
    for field in [
        "local_hash",
        "generation",
        "tombstone_seq",
        "binding",
        "session",
        "parent",
        "name",
    ] {
        let mut value = serde_json::to_value(&request).unwrap();
        match field {
            "local_hash" => {
                value["local_hash"] = serde_json::json!(
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                )
            }
            "generation" => value["generation"] = serde_json::json!("2"),
            "tombstone_seq" => value["tombstone_seq"] = serde_json::json!("0"),
            "binding" => {
                value["binding"]["device_stream_id"] = serde_json::json!(Uuid::new_v4().to_string())
            }
            "session" => value["edit_session_id"] = serde_json::json!(Uuid::new_v4().to_string()),
            "parent" => value["parent_id"] = serde_json::json!(rig.node.node_id.to_string()),
            "name" => value["name"] = serde_json::json!("../escape.md"),
            _ => unreachable!(),
        }
        let decoded = serde_json::from_value(value);
        if let Ok(candidate) = decoded {
            assert!(
                confirm_deleted_copy(&rig, candidate).await.is_err(),
                "{field}"
            );
        }
        assert!(
            rig.runtime
                .0
                .lock()
                .await
                .store
                .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
                .await
                .unwrap()
                .is_none(),
            "{field}"
        );
    }
    assert_original_preserved(&rig, b"local first\n").await;
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_confirmation_rejects_restored_and_redeleted_node_with_new_tombstone() {
    let rig = rig().await;
    let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
    let request = deleted_copy_request(&rig, &deleted);
    let restored = rig
        .peer
        .restore(&RestoreMutation {
            binding: rig.peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 2,
            node_id: rig.node.node_id,
            request: RestoreNodeRequest {
                expected_meta_rev: deleted.meta_rev.clone(),
                parent_id: rig.node.parent_id.unwrap(),
                name: rig.node.name.clone(),
            },
        })
        .await
        .unwrap();
    let OperationReceipt::Restored { node, .. } = restored else {
        panic!("restore receipt")
    };
    let redeleted = deleted_by_peer(&rig, 3, &node).await;
    assert_ne!(deleted.last_change_seq, redeleted.last_change_seq);
    assert!(confirm_deleted_copy(&rig, request).await.is_err());
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
async fn deleted_copy_confirmation_preserves_unresolved_sent_operation_and_payload() {
    let rig = rig().await;
    let queued = queue(&rig).await;
    let (sent, receipt) = prepare_and_commit_on_server(&rig).await;
    assert!(matches!(receipt, OperationReceipt::Committed { .. }));
    assert_eq!(sent.operation_id, queued.operation_id);
    assert_eq!(sent.status, notryx_client_store::OutboxStatus::Sent);
    let latest = rig
        .peer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == rig.node.node_id)
        .unwrap();
    let deleted = deleted_by_peer(&rig, 1, &latest).await;
    let request = deleted_copy_request(&rig, &deleted);
    let response = confirm_deleted_copy(&rig, request).await.unwrap();
    assert_eq!(response["status"], "pending");
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_outbox_operation(&rig.binding, &sent.operation_id)
            .await
            .unwrap(),
        Some(sent)
    );
    assert!(
        state
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap()
            .is_some()
    );
    drop(state);
    assert_original_preserved(&rig, b"local first\n").await;
    rig.server.shutdown().await.unwrap();
}

async fn delayed_deleted_copy_rig() -> (
    Rig,
    tokio::sync::mpsc::Receiver<()>,
    tokio::sync::mpsc::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let mut rig = rig().await;
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
    let (address, observed, release, proxy) =
        crate::sync_bootstrap::tests::delayed_inventory_proxy(
            rig.server.local_addr(),
            vec!["GET /v1/changes?"],
            false,
        )
        .await;
    let pairing = server_test_support::pairing_request(&config).await.0;
    let session = Arc::new(
        Session::pair(
            &format!("http://{address}"),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let binding = registration_binding(session.binding());
    let document = rig.node.node_id.to_string();
    let mut store = ClientStore::open(rig._directory.path().join("deleted-copy-delayed.sqlite"))
        .await
        .unwrap();
    store.activate_registration(&binding).await.unwrap();
    let opened = store
        .open_registered_document(
            &binding,
            &document,
            b"server source\n",
            rig.node.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            &document,
            &opened.edit_session_id,
            1,
            b"local first\n",
        )
        .await
        .unwrap();
    let mut state = RuntimeState::new(store, OWNER.into());
    state.session = Some(session.clone());
    state
        .gate
        .open_session(opened.edit_session_id.clone())
        .unwrap();
    state.product_sessions.insert(
        opened.edit_session_id.clone(),
        ProductSession {
            document_id: document,
            binding: binding.clone(),
            kind: ProductSessionKind::Active,
        },
    );
    let runtime = Runtime(Arc::new(Mutex::new(state)));
    let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    rig.runtime = runtime;
    rig.binding = binding;
    rig.session = session;
    rig.edit_session = opened.edit_session_id;
    (rig, observed, release, proxy)
}

#[tokio::test]
async fn deleted_copy_confirmation_rechecks_owner_registration_session_and_draft_after_catchup() {
    for transition in ["owner", "registration", "session", "checkpoint"] {
        let (rig, mut observed, release, proxy) = delayed_deleted_copy_rig().await;
        let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
        let request = deleted_copy_request(&rig, &deleted);
        let replacement = if transition == "registration" {
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
            let pair = server_test_support::pairing_request(&config).await.0;
            Some(Arc::new(
                Session::pair(
                    &format!("http://{}", rig.server.local_addr()),
                    pair.workspace_id,
                    &pair.pairing_secret,
                )
                .await
                .unwrap(),
            ))
        } else {
            None
        };
        let runtime = Runtime(rig.runtime.0.clone());
        let task = tokio::spawn(async move {
            crate::remote_deleted_copy_native::confirm_for_renderer(&runtime, request).await
        });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = rig.runtime.0.lock().await;
            let document = rig.node.node_id.to_string();
            match transition {
                "owner" => state.gate.replace_owner("new-owner".into()),
                "registration" => {
                    state
                        .store
                        .close_registered_document(&rig.binding, &document, &rig.edit_session)
                        .await
                        .unwrap();
                    state.gate.close_session(&rig.edit_session).unwrap();
                    state.product_sessions.remove(&rig.edit_session);
                    let pair = state.gate.begin_pair(OWNER).unwrap();
                    let replacement = replacement.as_ref().unwrap();
                    state
                        .store
                        .activate_registration(&registration_binding(replacement.binding()))
                        .await
                        .unwrap();
                    state.gate.commit_pair(&pair).unwrap();
                    state.session = Some(replacement.clone());
                }
                "session" => {
                    state
                        .store
                        .close_registered_document(&rig.binding, &document, &rig.edit_session)
                        .await
                        .unwrap();
                    state.gate.close_session(&rig.edit_session).unwrap();
                    state.product_sessions.remove(&rig.edit_session);
                    let (opened, _) = state
                        .store
                        .try_open_registered_draft(&rig.binding, &document)
                        .await
                        .unwrap()
                        .unwrap();
                    state
                        .gate
                        .open_session(opened.edit_session_id.clone())
                        .unwrap();
                    state.product_sessions.insert(
                        opened.edit_session_id,
                        ProductSession {
                            document_id: document,
                            binding: rig.binding.clone(),
                            kind: ProductSessionKind::Active,
                        },
                    );
                }
                "checkpoint" => {
                    state
                        .store
                        .save_registered_checkpoint(
                            &rig.binding,
                            &document,
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
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
                .await
                .unwrap()
                .is_none(),
            "{transition}"
        );
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            draft.bytes,
            if transition == "checkpoint" {
                b"local later\n".as_slice()
            } else {
                b"local first\n".as_slice()
            }
        );
        assert_eq!(draft.base_bytes, b"server source\n");
        drop(state);
        proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

async fn check_confirmed_deleted_copy_retry(transition: &str) {
    {
        let rig = rig().await;
        let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
        let request = deleted_copy_request(&rig, &deleted);
        let accepted = confirm_deleted_copy(&rig, request.clone()).await.unwrap();
        let before = rig
            .runtime
            .0
            .lock()
            .await
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap()
            .unwrap();
        if transition == "restored" {
            let receipt = rig
                .peer
                .restore(&RestoreMutation {
                    binding: rig.peer.binding().clone(),
                    operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                    operation_seq: 2,
                    node_id: rig.node.node_id,
                    request: RestoreNodeRequest {
                        expected_meta_rev: deleted.meta_rev.clone(),
                        parent_id: rig.node.parent_id.unwrap(),
                        name: rig.node.name.clone(),
                    },
                })
                .await
                .unwrap();
            assert!(matches!(receipt, OperationReceipt::Restored { .. }));
        }
        if transition == "edited" {
            save_next(&rig).await;
        }
        let Rig {
            _directory,
            server,
            runtime,
            binding,
            node,
            ..
        } = rig;
        let mut server = Some(server);
        if transition == "offline" {
            server.take().unwrap().shutdown().await.unwrap();
        }
        let response =
            crate::remote_deleted_copy_native::confirm_for_renderer(&runtime, request.clone())
                .await
                .unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            accepted,
            "{transition}"
        );
        let mut changed = serde_json::to_value(&request).unwrap();
        changed["name"] = serde_json::json!("different.md");
        let changed = serde_json::from_value(changed).unwrap();
        assert!(
            crate::remote_deleted_copy_native::confirm_for_renderer(&runtime, changed)
                .await
                .is_err(),
            "{transition}"
        );
        let mut state = runtime.0.lock().await;
        let after = state
            .store
            .read_deleted_draft_copy(&binding, &node.node_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after, before, "{transition}");
        let checkpoint = state
            .store
            .read_registered_checkpoint(&binding, &node.node_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            checkpoint.bytes,
            if transition == "edited" {
                b"local second\n".as_slice()
            } else {
                b"local first\n".as_slice()
            }
        );
        assert_eq!(checkpoint.base_bytes, b"server source\n");
        drop(state);
        if let Some(server) = server {
            server.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn confirmed_deleted_copy_retry_after_offline_preserves_prior_selection() {
    check_confirmed_deleted_copy_retry("offline").await;
}

#[tokio::test]
async fn confirmed_deleted_copy_retry_after_restore_preserves_prior_selection() {
    check_confirmed_deleted_copy_retry("restored").await;
}

#[tokio::test]
async fn confirmed_deleted_copy_retry_after_later_edit_preserves_prior_selection() {
    check_confirmed_deleted_copy_retry("edited").await;
}
