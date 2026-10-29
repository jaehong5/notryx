use crate::{
    ProductSession, ProductSessionKind, Runtime, RuntimeState, registration_binding,
    remote_change_dirty::{DirtyRemoteState, inspect_dirty_remote},
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
    sync_catchup::catch_up_metadata,
};
use notryx_client_runtime::{DeleteMutation, Session};
use notryx_client_store::ClientStore;
use notryx_protocol::OperationReceipt;
use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[tokio::test]
async fn remote_deletion_is_distinct_from_unresolved_and_preserves_both_clients_data() {
    for dirty in [false, true] {
        let (directory, server, _unused, _unused_session, other_document) = setup().await;
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing_a = server_test_support::pairing_request(&config).await.0;
        let session_a = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                pairing_a.workspace_id,
                &pairing_a.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let pairing_b = server_test_support::pairing_request(&config).await.0;
        let session_b = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                pairing_b.workspace_id,
                &pairing_b.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let node = session_a
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let document = node.node_id.to_string();
        let binding_a = registration_binding(session_a.binding());
        let binding_b = registration_binding(session_b.binding());
        let mut store_a = ClientStore::open(directory.path().join("delete-observation-a.sqlite"))
            .await
            .unwrap();
        store_a.activate_registration(&binding_a).await.unwrap();
        let opened = store_a
            .open_registered_document(
                &binding_a,
                &document,
                b"move source\n",
                node.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        if dirty {
            store_a
                .save_registered_checkpoint(
                    &binding_a,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"local edit\n",
                )
                .await
                .unwrap();
            store_a
                .queue_checkpoint(&binding_a, &document, 1)
                .await
                .unwrap();
        }
        let mut state_a = RuntimeState::new(store_a, "owner-a".into());
        state_a.session = Some(session_a.clone());
        state_a
            .gate
            .open_session(opened.edit_session_id.clone())
            .unwrap();
        state_a.product_sessions.insert(
            opened.edit_session_id.clone(),
            ProductSession {
                document_id: document.clone(),
                binding: binding_a.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let runtime_a = Runtime(Arc::new(Mutex::new(state_a)));
        let snapshot_a = runtime_a.0.lock().await.gate.snapshot("owner-a").unwrap();
        bootstrap_metadata(&runtime_a, &snapshot_a, &session_a)
            .await
            .unwrap();

        let mut store_b = ClientStore::open(directory.path().join("delete-observation-b.sqlite"))
            .await
            .unwrap();
        store_b.activate_registration(&binding_b).await.unwrap();
        let other = store_b
            .open_registered_document(&binding_b, &other_document, b"peer base\n", "peer-base")
            .await
            .unwrap();
        store_b
            .save_registered_checkpoint(
                &binding_b,
                &other_document,
                &other.edit_session_id,
                1,
                b"peer draft\n",
            )
            .await
            .unwrap();
        store_b
            .close_registered_document(&binding_b, &other_document, &other.edit_session_id)
            .await
            .unwrap();
        let before_b = store_b
            .read_registered_checkpoint(&binding_b, &other_document)
            .await
            .unwrap();
        let mut state_b = RuntimeState::new(store_b, "owner-b".into());
        state_b.session = Some(session_b.clone());
        let runtime_b = Runtime(Arc::new(Mutex::new(state_b)));
        let snapshot_b = runtime_b.0.lock().await.gate.snapshot("owner-b").unwrap();
        bootstrap_metadata(&runtime_b, &snapshot_b, &session_b)
            .await
            .unwrap();

        let before_a = {
            let mut state = runtime_a.0.lock().await;
            let draft = state
                .store
                .read_registered_checkpoint(&binding_a, &document)
                .await
                .unwrap();
            let outbox = state.store.list_outbox(&binding_a, 0, 100).await.unwrap();
            let bytes = if let Some(operation) = outbox.entries.first() {
                state
                    .store
                    .read_outbox_operation(&binding_a, &operation.operation_id)
                    .await
                    .unwrap()
            } else {
                None
            };
            (draft, outbox, bytes)
        };
        let delete = DeleteMutation {
            binding: session_b.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_node_etag: node.node_etag.clone(),
        };
        let receipt = session_b.delete(&delete).await.unwrap();
        assert!(
            matches!(&receipt, OperationReceipt::Deleted { node: deleted, .. } if deleted.node_id == node.node_id && deleted.deleted && deleted.content_hash == node.content_hash)
        );
        assert!(!directory.path().join("notes/move.md").exists());
        assert!(
            session_b
                .trash(None, 100)
                .await
                .unwrap()
                .nodes
                .iter()
                .any(|item| item.node_id == node.node_id)
        );

        let observed = inspect_dirty_remote(
            &runtime_a,
            "owner-a",
            &document,
            &opened.edit_session_id,
            u64::from(dirty),
        )
        .await
        .unwrap();
        assert_eq!(
            observed,
            DirtyRemoteState::Deleted,
            "only a committed tombstone is a deletion observation"
        );
        catch_up_metadata(&runtime_b, &snapshot_b, &session_b)
            .await
            .unwrap();
        let mut state_a = runtime_a.0.lock().await;
        let index_a = state_a
            .store
            .sync_index(&binding_a)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert!(
            index_a
                .iter()
                .any(|item| item.node_id == node.node_id && item.deleted)
        );
        let after_a = (
            state_a
                .store
                .read_registered_checkpoint(&binding_a, &document)
                .await
                .unwrap(),
            state_a.store.list_outbox(&binding_a, 0, 100).await.unwrap(),
            if let Some(operation) = before_a.1.entries.first() {
                state_a
                    .store
                    .read_outbox_operation(&binding_a, &operation.operation_id)
                    .await
                    .unwrap()
            } else {
                None
            },
        );
        assert_eq!(
            after_a, before_a,
            "delete observation changed checkpoint, base, or queued payload"
        );
        drop(state_a);
        let mut state_b = runtime_b.0.lock().await;
        assert_eq!(
            state_b
                .store
                .read_registered_checkpoint(&binding_b, &other_document)
                .await
                .unwrap(),
            before_b
        );
        assert!(
            state_b
                .store
                .sync_index(&binding_b)
                .await
                .unwrap()
                .unwrap()
                .1
                .iter()
                .any(|item| item.node_id == node.node_id && item.deleted)
        );
        drop(state_b);
        assert_eq!(
            session_b.server_info().await.unwrap().head_seq,
            match receipt {
                OperationReceipt::Deleted { commit_seq, .. } => commit_seq,
                _ => unreachable!(),
            }
        );
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn sent_content_operation_reports_peer_deletion_and_preserves_local_payload() {
    let (directory, server, runtime, session, document) = setup().await;
    let binding = registration_binding(session.binding());
    let (snapshot, edit_session, sent, before) = {
        let mut state = runtime.0.lock().await;
        let (opened, active) = state
            .store
            .try_open_registered_draft(&binding, &document)
            .await
            .unwrap()
            .unwrap();
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
        let sent = state.store.prepare_outbox(&binding).await.unwrap().unwrap();
        let before = (
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        );
        (
            state.gate.snapshot("owner").unwrap(),
            opened.edit_session_id,
            sent,
            before,
        )
    };
    bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let node = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == document)
        .unwrap();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: server.local_addr(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("server-state"),
            uploads: Default::default(),
        },
    };
    let pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let receipt = peer
        .delete(&DeleteMutation {
            binding: peer.binding().clone(),
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_node_etag: node.node_etag,
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Deleted { .. }));
    catch_up_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let observed = inspect_dirty_remote(&runtime, "owner", &document, &edit_session, 1)
        .await
        .unwrap();
    assert_eq!(observed, DirtyRemoteState::Deleted);
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap(),
        before.0
    );
    assert_eq!(
        state.store.list_outbox(&binding, 0, 100).await.unwrap(),
        before.1
    );
    assert_eq!(
        state
            .store
            .read_outbox_operation(&binding, &sent.operation_id)
            .await
            .unwrap(),
        Some(sent)
    );
    drop(state);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn delayed_deletion_observation_rejects_old_renderer_registration_and_generation() {
    use crate::remote_change_dirty::inspect_dirty_remote_notifying;
    use notryx_protocol::WorkspaceDirtyRemoteRequest;
    use std::time::Duration;

    for transition in ["renderer", "registration", "generation"] {
        let (directory, server, _unused, _unused_session, _document) = setup().await;
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let (address, mut observed, release, proxy) =
            crate::sync_bootstrap::tests::delayed_inventory_proxy(
                server.local_addr(),
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
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let document = node.node_id.to_string();
        let mut store = ClientStore::open(directory.path().join("delete-late.sqlite"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let opened = store
            .open_registered_document(
                &binding,
                &document,
                b"move source\n",
                node.content_rev.as_ref().unwrap().as_str(),
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
        store
            .queue_checkpoint(&binding, &document, 1)
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session.clone());
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
        let runtime = Runtime(Arc::new(Mutex::new(state)));
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let peer_pairing = server_test_support::pairing_request(&config).await.0;
        let peer = Session::pair(
            &format!("http://{}", server.local_addr()),
            peer_pairing.workspace_id,
            &peer_pairing.pairing_secret,
        )
        .await
        .unwrap();
        let receipt = peer
            .delete(&DeleteMutation {
                binding: peer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_node_etag: node.node_etag.clone(),
            })
            .await
            .unwrap();
        assert!(matches!(receipt, OperationReceipt::Deleted { .. }));
        let replacement = if transition == "registration" {
            let pairing = server_test_support::pairing_request(&config).await.0;
            Some(Arc::new(
                Session::pair(
                    &format!("http://{}", server.local_addr()),
                    pairing.workspace_id,
                    &pairing.pairing_secret,
                )
                .await
                .unwrap(),
            ))
        } else {
            None
        };
        let request = WorkspaceDirtyRemoteRequest {
            renderer_session_id: "owner".into(),
            document_id: document.clone(),
            edit_session_id: opened.edit_session_id.clone(),
            generation: "1".into(),
        };
        let task = tokio::spawn({
            let runtime = Runtime(runtime.0.clone());
            let request = request.clone();
            async move {
                let mut events = Vec::new();
                let result = inspect_dirty_remote_notifying(
                    &runtime,
                    &request,
                    1,
                    || Ok(()),
                    |event| events.push(event.to_owned()),
                )
                .await;
                (result, events)
            }
        });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = runtime.0.lock().await;
            match transition {
                "renderer" => {
                    state.gate.replace_owner("owner-reloaded".into());
                }
                "registration" => {
                    state
                        .store
                        .close_registered_document(&binding, &document, &opened.edit_session_id)
                        .await
                        .unwrap();
                    state.gate.close_session(&opened.edit_session_id).unwrap();
                    state.product_sessions.remove(&opened.edit_session_id);
                    let pair = state.gate.begin_pair("owner").unwrap();
                    let replacement = replacement.as_ref().unwrap();
                    state
                        .store
                        .activate_registration(&registration_binding(replacement.binding()))
                        .await
                        .unwrap();
                    state.gate.commit_pair(&pair).unwrap();
                    state.session = Some(replacement.clone());
                }
                "generation" => {
                    state
                        .store
                        .save_registered_checkpoint(
                            &binding,
                            &document,
                            &opened.edit_session_id,
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
        let (result, events) = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        if transition == "generation" {
            assert_eq!(result.unwrap(), DirtyRemoteState::Pending);
        } else {
            assert!(result.is_err(), "late response accepted after {transition}");
            assert!(
                !events
                    .iter()
                    .any(|event| event == "workspace-metadata-changed"),
                "old response notified the new {transition}"
            );
        }
        let mut state = runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            draft.bytes,
            if transition == "generation" {
                b"local later\n".as_slice()
            } else {
                b"local first\n".as_slice()
            }
        );
        assert_eq!(draft.base_bytes, b"move source\n");
        assert_eq!(draft.base_revision, node.content_rev.unwrap().as_str());
        let outbox = state.store.list_outbox(&binding, 0, 100).await.unwrap();
        assert_eq!(outbox.entries.len(), 1);
        let original = state
            .store
            .read_outbox_operation(&binding, &outbox.entries[0].operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.bytes, b"local first\n");
        drop(state);
        assert!(!directory.path().join("notes/move.md").exists());
        assert_eq!(
            peer.server_info().await.unwrap().head_seq,
            match receipt {
                OperationReceipt::Deleted { commit_seq, .. } => commit_seq,
                _ => unreachable!(),
            }
        );
        proxy.abort();
        server.shutdown().await.unwrap();
    }
}
