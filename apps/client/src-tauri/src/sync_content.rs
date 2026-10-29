use crate::Runtime;
use notryx_client_runtime::{Error as NetworkError, Session};
use notryx_client_store::StoreError;
use notryx_protocol::NodeState;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContentFetchError {
    Network(NetworkError),
    Store(StoreError),
    Guard(String),
}

#[allow(dead_code)]
pub(crate) async fn download_and_cache(
    runtime: &Runtime,
    session: &Arc<Session>,
    node: &NodeState,
) -> Result<bool, ContentFetchError> {
    let binding = crate::registration_binding(session.binding());
    let generation = {
        let state = crate::sync_lifecycle::checked_registration(runtime, session)
            .await
            .map_err(ContentFetchError::Guard)?;
        let generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .response_trusted(&binding, generation)
        {
            return Err(ContentFetchError::Guard("stale_sync".into()));
        }
        generation
    };
    let bytes = session.content(node).await;
    let mut state = crate::sync_lifecycle::checked_registration(runtime, session)
        .await
        .map_err(ContentFetchError::Guard)?;
    if !state
        .transmission_gate
        .response_trusted(&binding, generation)
    {
        return Err(ContentFetchError::Guard("stale_sync".into()));
    }
    let bytes = bytes.map_err(ContentFetchError::Network)?;
    state
        .store
        .cache_content(&binding, node, &bytes)
        .await
        .map_err(ContentFetchError::Store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{registration_binding, sync_bootstrap};
    use notryx_client_runtime::ContentMutation;

    #[tokio::test]
    async fn verified_server_content_is_cached_without_changing_dirty_draft_or_outbox() {
        let (_directory, server, runtime, session, document) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
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
        let binding = registration_binding(session.binding());
        let (draft_before, outbox_before) = {
            let mut state = runtime.0.lock().await;
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &document)
                    .await
                    .unwrap()
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        assert_eq!(
            download_and_cache(&runtime, &session, &node).await,
            Ok(true)
        );
        let mut state = runtime.0.lock().await;
        let cached = state
            .store
            .read_cached_content(&binding, node.node_id)
            .await
            .unwrap()
            .unwrap();
        assert!(cached.current);
        assert_eq!(cached.bytes, b"server source\n");
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap()
                .unwrap(),
            draft_before
        );
        assert_eq!(
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            outbox_before
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn changed_server_content_cannot_be_cached_under_old_revision() {
        let (_directory, server, runtime, session, document) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
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
        let binding = registration_binding(session.binding());
        let (draft_before, outbox_before) = {
            let mut state = runtime.0.lock().await;
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &document)
                    .await
                    .unwrap()
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        session
            .put_content(&ContentMutation {
                binding: session.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.clone().unwrap(),
                bytes: b"new server source\n".to_vec(),
            })
            .await
            .unwrap();
        assert_eq!(
            download_and_cache(&runtime, &session, &node).await,
            Err(ContentFetchError::Network(NetworkError::Changed))
        );
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .read_cached_content(&binding, node.node_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap()
                .unwrap(),
            draft_before
        );
        assert_eq!(
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            outbox_before
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    struct DelayedFixture {
        directory: tempfile::TempDir,
        server: notryx_server::http::RunningServer,
        runtime: Runtime,
        writer: Arc<Session>,
        session: Arc<Session>,
        binding: notryx_client_store::RegistrationBinding,
        node: NodeState,
        cursor: notryx_domain::ChangeSeq,
        draft: notryx_client_store::PersistedDraft,
        operation: notryx_client_store::OutboxOperation,
        observed: tokio::sync::mpsc::Receiver<()>,
        release: tokio::sync::mpsc::Sender<()>,
        proxy: tokio::task::JoinHandle<()>,
    }

    async fn delayed_fixture() -> DelayedFixture {
        delayed_fixture_with_mode(None).await
    }

    async fn delayed_fixture_with_mode(
        mode: Option<Arc<std::sync::atomic::AtomicU8>>,
    ) -> DelayedFixture {
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};

        let (directory, server, runtime, writer, _document) = sync_bootstrap::tests::setup().await;
        let node = writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
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
        let (address, observed, release, proxy) = sync_bootstrap::tests::test_proxy(
            server.local_addr(),
            vec!["GET /v1/nodes/"],
            false,
            mode,
        )
        .await;
        let pairing = http::pairing_request(&config).await.0;
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
        let document = node.node_id.to_string();
        let (draft, operation) = {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
            let opened = state
                .store
                .open_registered_document(
                    &binding,
                    &document,
                    b"move source\n",
                    node.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"local edit",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
            let operation = state
                .store
                .queue_checkpoint(&binding, &document, 1)
                .await
                .unwrap();
            let draft = state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap()
                .unwrap();
            (draft, operation)
        };
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cursor = sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        DelayedFixture {
            directory,
            server,
            runtime,
            writer,
            session,
            binding,
            node,
            cursor,
            draft,
            operation,
            observed,
            release,
            proxy,
        }
    }

    #[tokio::test]
    async fn in_flight_download_is_not_promoted_after_remote_metadata_changes() {
        use std::time::Duration;
        let mut fixture = delayed_fixture().await;
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_session = fixture.session.clone();
        let task_node = fixture.node.clone();
        let task = tokio::spawn(async move {
            download_and_cache(&task_runtime, &task_session, &task_node).await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        drop(
            tokio::time::timeout(Duration::from_secs(2), fixture.runtime.0.lock())
                .await
                .unwrap(),
        );
        fixture
            .writer
            .put_content(&ContentMutation {
                binding: fixture.writer.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: fixture.node.node_id,
                expected_revision: fixture.node.content_rev.clone().unwrap(),
                bytes: b"later server revision\n".to_vec(),
            })
            .await
            .unwrap();
        let page = fixture.writer.changes(fixture.cursor, None).await.unwrap();
        {
            let mut state = fixture.runtime.0.lock().await;
            state
                .store
                .apply_changes(&fixture.binding, &page, page.through_seq)
                .await
                .unwrap();
        }
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Ok(false)
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_cached_content(&fixture.binding, fixture.node.node_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &fixture.node.node_id.to_string())
                .await
                .unwrap()
                .unwrap(),
            fixture.draft
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            fixture.operation
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn in_flight_download_is_rejected_after_registration_changes() {
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        use std::time::Duration;

        let mut fixture = delayed_fixture().await;
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_session = fixture.session.clone();
        let task_node = fixture.node.clone();
        let task = tokio::spawn(async move {
            download_and_cache(&task_runtime, &task_session, &task_node).await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture.directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture.directory.path().join("notes"),
                state_root: fixture.directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = http::pairing_request(&config).await.0;
        let new_session = Arc::new(
            Session::pair(
                &format!("http://{}", fixture.server.local_addr()),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        {
            let mut state = fixture.runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(new_session.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(new_session.clone());
        }
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err(ContentFetchError::Guard("stale_registration".into()))
        );
        let mut state = fixture.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &fixture.node.node_id.to_string())
                .await
                .unwrap()
                .unwrap(),
            fixture.draft
        );
        let mut frozen = fixture.operation;
        frozen.frozen = true;
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &frozen.operation_id)
                .await
                .unwrap()
                .unwrap(),
            frozen
        );
        assert!(
            state
                .store
                .read_cached_content(
                    &registration_binding(new_session.binding()),
                    fixture.node.node_id
                )
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn in_flight_download_error_is_rejected_after_registration_changes() {
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        use std::time::Duration;

        let mut fixture = delayed_fixture().await;
        fixture
            .writer
            .put_content(&ContentMutation {
                binding: fixture.writer.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: fixture.node.node_id,
                expected_revision: fixture.node.content_rev.clone().unwrap(),
                bytes: b"later server revision\n".to_vec(),
            })
            .await
            .unwrap();
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_session = fixture.session.clone();
        let task_node = fixture.node.clone();
        let task = tokio::spawn(async move {
            download_and_cache(&task_runtime, &task_session, &task_node).await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture.directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture.directory.path().join("notes"),
                state_root: fixture.directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = http::pairing_request(&config).await.0;
        let new_session = Arc::new(
            Session::pair(
                &format!("http://{}", fixture.server.local_addr()),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        {
            let mut state = fixture.runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(new_session.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(new_session.clone());
        }
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err(ContentFetchError::Guard("stale_registration".into()))
        );
        let mut state = fixture.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &fixture.node.node_id.to_string())
                .await
                .unwrap()
                .unwrap(),
            fixture.draft
        );
        let mut frozen = fixture.operation;
        frozen.frozen = true;
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &frozen.operation_id)
                .await
                .unwrap()
                .unwrap(),
            frozen
        );
        assert!(
            state
                .store
                .read_cached_content(
                    &registration_binding(new_session.binding()),
                    fixture.node.node_id
                )
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    async fn assert_delayed_download_is_rejected_after_trust_block(clear: bool) {
        use crate::send::TransmissionBlock;
        use std::time::Duration;
        let mut fixture = delayed_fixture().await;
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_session = fixture.session.clone();
        let task_node = fixture.node.clone();
        let task = tokio::spawn(async move {
            download_and_cache(&task_runtime, &task_session, &task_node).await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = fixture.runtime.0.lock().await;
            state
                .transmission_gate
                .block(&fixture.binding, TransmissionBlock::ResyncRequired);
            if clear {
                assert!(
                    state
                        .transmission_gate
                        .clear(&fixture.binding, TransmissionBlock::ResyncRequired)
                );
            }
        }
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err(ContentFetchError::Guard("stale_sync".into()))
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_cached_content(&fixture.binding, fixture.node.node_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &fixture.node.node_id.to_string())
                .await
                .unwrap()
                .unwrap(),
            fixture.draft
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            fixture.operation
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn in_flight_download_is_rejected_when_sync_is_blocked() {
        assert_delayed_download_is_rejected_after_trust_block(false).await;
    }

    #[tokio::test]
    async fn in_flight_download_is_rejected_after_sync_block_is_cleared() {
        assert_delayed_download_is_rejected_after_trust_block(true).await;
    }

    #[tokio::test]
    async fn open_start_prefers_existing_local_draft() {
        use base64::Engine;
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, document) = sync_bootstrap::tests::setup().await;
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Draft(opened) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("existing draft must be opened first");
        };
        assert!(opened.local_draft);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(opened.bytes_base64)
                .unwrap(),
            b"local draft"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delete_queued_after_remote_open_started_rejects_late_editable_response() {
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, other_document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let binding = registration_binding(session.binding());
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open")
        };
        let response = remote.session.content(&node).await;
        assert!(response.is_ok());
        {
            let mut state = runtime.0.lock().await;
            state
                .store
                .queue_delete(&binding, node.node_id, &node.node_etag)
                .await
                .unwrap();
        }
        let result = crate::finish_workspace_open(
            &runtime,
            remote,
            binding.clone(),
            node.node_id.to_string(),
            node.content_rev.as_ref().unwrap().as_str().to_owned(),
            response,
        )
        .await;
        assert!(matches!(result, Err(ref error) if error == "OutboxBlocked"));
        let mut state = runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &other_document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert!(
            state
                .store
                .next_pending_outbox(&binding)
                .await
                .unwrap()
                .is_some()
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn offline_cached_document_opens_and_preserves_base_after_checkpoint() {
        use base64::Engine;
        use notryx_client_store::ClientStore;
        use notryx_protocol::WorkspaceOpenRequest;
        let (directory, server, runtime, session, other_document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let binding = registration_binding(session.binding());
        let (other_draft, outbox) = {
            let mut state = runtime.0.lock().await;
            assert!(
                state
                    .store
                    .cache_content(&binding, &node, b"move source\n")
                    .await
                    .unwrap()
            );
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &other_document)
                    .await
                    .unwrap()
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("cached clean document must use the open path")
        };
        server.shutdown().await.unwrap();
        let bytes = remote.session.content(&node).await;
        assert_eq!(bytes, Err(NetworkError::Network));
        let revision = node.content_rev.as_ref().unwrap().as_str().to_owned();
        let opened = crate::finish_workspace_open(
            &runtime,
            remote,
            binding.clone(),
            node.node_id.to_string(),
            revision.clone(),
            bytes,
        )
        .await
        .expect("an offline cached document must open");
        assert!(!opened.readonly);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&opened.bytes_base64)
                .unwrap(),
            b"move source\n"
        );
        let document = node.node_id.to_string();
        {
            let mut state = runtime.0.lock().await;
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"offline edit\n",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
        }
        let state = Arc::try_unwrap(runtime.0).ok().unwrap().into_inner();
        state.store.close().await.unwrap();
        let mut reopened = ClientStore::open(directory.path().join("client.sqlite"))
            .await
            .unwrap();
        let draft = reopened
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"offline edit\n");
        assert_eq!(draft.base_bytes, b"move source\n");
        assert_eq!(draft.base_revision, revision);
        assert_eq!(
            reopened
                .read_registered_checkpoint(&binding, &other_document)
                .await
                .unwrap()
                .unwrap(),
            other_draft
        );
        assert_eq!(
            reopened.list_outbox(&binding, 0, 100).await.unwrap(),
            outbox
        );
        assert_eq!(
            std::fs::read(directory.path().join("notes/move.md")).unwrap(),
            b"move source\n"
        );
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn opening_cached_search_result_matches_latest_content_and_preserves_old_clean_draft() {
        use base64::Engine;
        use notryx_client_store::{OutboxStatus, SearchSource};
        use notryx_protocol::{
            AdminRequest, AdminResponse, AdminResult, WorkspaceOpenRequest, WorkspaceSendStatus,
        };
        use notryx_server::admin;
        let (directory, server, runtime, session, other_document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let old = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let binding = registration_binding(session.binding());
        let id = old.node_id.to_string();
        let (other_draft, other_operation) = {
            let mut state = runtime.0.lock().await;
            let draft = state
                .store
                .read_registered_checkpoint(&binding, &other_document)
                .await
                .unwrap()
                .unwrap();
            let page = state.store.list_outbox(&binding, 0, 100).await.unwrap();
            assert_eq!(page.entries.len(), 1);
            assert_eq!(page.entries[0].operation_seq, 1);
            (draft, page.entries[0].operation_id.clone())
        };
        let earlier = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(earlier.status, WorkspaceSendStatus::Conflict);
        {
            let mut state = runtime.0.lock().await;
            assert_eq!(
                state
                    .store
                    .confirmed_receipt_through(&binding)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                state
                    .store
                    .read_registered_checkpoint(&binding, &other_document)
                    .await
                    .unwrap()
                    .unwrap(),
                other_draft
            );
            let operation = state
                .store
                .read_outbox_operation(&binding, &other_operation)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(operation.status, OutboxStatus::Conflict);
            assert_eq!(operation.bytes, b"local draft");
        }
        let queued = {
            let mut state = runtime.0.lock().await;
            let opened = state
                .store
                .open_registered_document(
                    &binding,
                    &id,
                    b"move source\n",
                    old.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &id,
                    &opened.edit_session_id,
                    1,
                    b"clean committed\n",
                )
                .await
                .unwrap();
            let queued = state
                .store
                .queue_checkpoint(&binding, &id, 1)
                .await
                .unwrap();
            assert_eq!(queued.operation_seq, 2);
            state
                .store
                .close_registered_document(&binding, &id, &opened.edit_session_id)
                .await
                .unwrap();
            queued
        };
        let committed = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(committed.status, WorkspaceSendStatus::Committed);
        let old_draft = {
            let mut state = runtime.0.lock().await;
            assert_eq!(
                state
                    .store
                    .confirmed_receipt_through(&binding)
                    .await
                    .unwrap(),
                2
            );
            let operation = state
                .store
                .read_outbox_operation(&binding, &queued.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(operation.status, OutboxStatus::Committed);
            assert_eq!(operation.generation, 1);
            assert_eq!(operation.bytes, b"clean committed\n");
            let draft = state
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(draft.generation, 1);
            assert_eq!(draft.bytes, b"clean committed\n");
            assert_eq!(draft.base_bytes, b"clean committed\n");
            draft
        };
        let current = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == old.node_id)
            .unwrap();
        assert_eq!(
            old_draft.base_revision,
            current.content_rev.as_ref().unwrap().as_str()
        );
        let admin_socket = directory.path().join("admin.sock");
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let writer = notryx_client_runtime::Session::pair(
            &format!("http://{}", server.local_addr()),
            workspace_id,
            &pairing_secret,
        )
        .await
        .unwrap();
        writer
            .put_content(&ContentMutation {
                binding: writer.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: old.node_id,
                expected_revision: current.content_rev.clone().unwrap(),
                bytes: b"new remote content\n".to_vec(),
            })
            .await
            .unwrap();
        crate::sync_catchup::catch_up_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let latest = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == old.node_id)
            .unwrap();
        {
            let mut state = runtime.0.lock().await;
            assert!(
                state
                    .store
                    .cache_content(&binding, &latest, b"new remote content\n")
                    .await
                    .unwrap()
            );
            let page = state
                .store
                .search_candidates(&binding, None, 100)
                .await
                .unwrap();
            let candidate = page
                .candidates
                .iter()
                .find(|candidate| candidate.node_id == old.node_id)
                .unwrap();
            assert_eq!(candidate.source, SearchSource::Cache);
            assert_eq!(candidate.bytes(), b"new remote content\n");
        }
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: latest.clone(),
        };
        let opened = match crate::begin_workspace_open(&runtime, &request, None)
            .await
            .unwrap()
        {
            crate::WorkspaceOpenStart::Draft(opened) => opened,
            crate::WorkspaceOpenStart::Remote(remote) => {
                let bytes = remote.session.content(&latest).await;
                crate::finish_workspace_open(
                    &runtime,
                    remote,
                    binding.clone(),
                    id.clone(),
                    latest.content_rev.as_ref().unwrap().as_str().into(),
                    bytes,
                )
                .await
                .unwrap()
            }
            crate::WorkspaceOpenStart::Stale(_) => panic!("latest metadata should not be stale"),
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&opened.bytes_base64)
                .unwrap(),
            b"new remote content\n",
            "opening a search result must show the matching content"
        );
        assert!(!opened.readonly);
        assert!(!opened.local_draft);
        assert_eq!(opened.generation, old_draft.generation.to_string());
        assert!(opened.open_notice.is_none());
        assert_eq!(
            runtime
                .0
                .lock()
                .await
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap(),
            old_draft
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn offline_corrupt_cache_is_not_opened_or_used_as_a_draft_base() {
        use notryx_protocol::WorkspaceOpenRequest;
        use sqlx::{Connection, Row};
        let (directory, server, runtime, session, other_document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let binding = registration_binding(session.binding());
        let (other_draft, outbox) = {
            let mut state = runtime.0.lock().await;
            assert!(
                state
                    .store
                    .cache_content(&binding, &node, b"move source\n")
                    .await
                    .unwrap()
            );
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &other_document)
                    .await
                    .unwrap()
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        let mut corrupt = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("client.sqlite")),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE cached_content SET bytes=? WHERE node_id=?")
            .bind(b"corrupted".as_slice())
            .bind(node.node_id.to_string())
            .execute(&mut corrupt)
            .await
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        server.shutdown().await.unwrap();
        let bytes = remote.session.content(&node).await;
        assert_eq!(bytes, Err(NetworkError::Network));
        let result = crate::finish_workspace_open(
            &runtime,
            remote,
            binding.clone(),
            node.node_id.to_string(),
            node.content_rev.as_ref().unwrap().as_str().into(),
            bytes,
        )
        .await;
        assert!(result.is_err());
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .read_registered_checkpoint(&binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &other_document)
                .await
                .unwrap()
                .unwrap(),
            other_draft
        );
        assert_eq!(
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            outbox
        );
        drop(state);
        let row = sqlx::query("SELECT bytes FROM cached_content WHERE node_id=?")
            .bind(node.node_id.to_string())
            .fetch_one(&mut corrupt)
            .await
            .unwrap();
        assert_eq!(row.try_get::<Vec<u8>, _>("bytes").unwrap(), b"corrupted");
        corrupt.close().await.unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("notes/move.md")).unwrap(),
            b"move source\n"
        );
    }

    #[tokio::test]
    async fn offline_stale_cache_does_not_become_a_new_base() {
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, other_document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let binding = registration_binding(session.binding());
        let (other_draft, outbox) = {
            let mut state = runtime.0.lock().await;
            assert!(
                state
                    .store
                    .cache_content(&binding, &node, b"move source\n")
                    .await
                    .unwrap()
            );
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &other_document)
                    .await
                    .unwrap()
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        session
            .put_content(&ContentMutation {
                binding: session.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.clone().unwrap(),
                bytes: b"new remote content\n".to_vec(),
            })
            .await
            .unwrap();
        crate::sync_catchup::catch_up_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let latest = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|item| item.node_id == node.node_id)
            .unwrap();
        assert_ne!(latest.content_rev, node.content_rev);
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: latest.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        server.shutdown().await.unwrap();
        let bytes = remote.session.content(&latest).await;
        assert_eq!(bytes, Err(NetworkError::Network));
        let result = crate::finish_workspace_open(
            &runtime,
            remote,
            binding.clone(),
            latest.node_id.to_string(),
            latest.content_rev.as_ref().unwrap().as_str().into(),
            bytes,
        )
        .await;
        assert!(result.is_err());
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .read_registered_checkpoint(&binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &other_document)
                .await
                .unwrap()
                .unwrap(),
            other_draft
        );
        assert_eq!(
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            outbox
        );
    }

    #[tokio::test]
    async fn offline_fallback_does_not_cross_renderer_or_registration_change() {
        use notryx_protocol::WorkspaceOpenRequest;
        for change_registration in [false, true] {
            let (_directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
            let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
            sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
                .await
                .unwrap();
            let node = session
                .inventory(None)
                .await
                .unwrap()
                .nodes
                .into_iter()
                .find(|node| node.name == "move.md")
                .unwrap();
            let binding = registration_binding(session.binding());
            assert!(
                runtime
                    .0
                    .lock()
                    .await
                    .store
                    .cache_content(&binding, &node, b"move source\n")
                    .await
                    .unwrap()
            );
            let request = WorkspaceOpenRequest {
                renderer_session_id: "owner".into(),
                node: node.clone(),
            };
            let crate::WorkspaceOpenStart::Remote(remote) =
                crate::begin_workspace_open(&runtime, &request, None)
                    .await
                    .unwrap()
            else {
                panic!("remote open expected")
            };
            server.shutdown().await.unwrap();
            let bytes = remote.session.content(&node).await;
            assert_eq!(bytes, Err(NetworkError::Network));
            {
                let mut state = runtime.0.lock().await;
                if change_registration {
                    let replacement = notryx_client_store::RegistrationBinding {
                        device_id: uuid::Uuid::new_v4().to_string(),
                        device_stream_id: uuid::Uuid::new_v4().to_string(),
                        ..binding.clone()
                    };
                    state
                        .store
                        .activate_registration(&replacement)
                        .await
                        .unwrap();
                } else {
                    state.gate.replace_owner("replacement".into());
                }
            }
            let result = crate::finish_workspace_open(
                &runtime,
                remote,
                binding.clone(),
                node.node_id.to_string(),
                node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await;
            assert!(result.is_err());
            assert!(
                runtime
                    .0
                    .lock()
                    .await
                    .store
                    .read_registered_checkpoint(&binding, &node.node_id.to_string())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn permission_failure_is_not_silently_converted_to_cached_success() {
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let binding = registration_binding(session.binding());
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .cache_content(&binding, &node, b"move source\n")
                .await
                .unwrap()
        );
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        let result = crate::finish_workspace_open(
            &runtime,
            remote,
            binding.clone(),
            node.node_id.to_string(),
            node.content_rev.as_ref().unwrap().as_str().into(),
            Err(NetworkError::Forbidden),
        )
        .await;
        assert_eq!(result, Err("Forbidden".into()));
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .read_registered_checkpoint(&binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_steps_use_the_verified_remote_bytes_when_no_draft_exists() {
        use base64::Engine;
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, _document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("clean document must use remote content");
        };
        let bytes = remote.session.content(&request.node).await;
        let opened = crate::finish_workspace_open(
            &runtime,
            remote,
            registration_binding(session.binding()),
            request.node.node_id.to_string(),
            request.node.content_rev.as_ref().unwrap().as_str().into(),
            bytes,
        )
        .await
        .unwrap();
        assert!(!opened.local_draft);
        assert!(!opened.readonly);
        assert_eq!(opened.cache_warning, None);
        let cached = runtime
            .0
            .lock()
            .await
            .store
            .read_cached_content(
                &registration_binding(session.binding()),
                request.node.node_id,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(cached.current);
        assert_eq!(cached.bytes, b"move source\n");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(opened.bytes_base64)
                .unwrap(),
            b"move source\n"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_rejects_remote_base_after_sync_block_is_cleared() {
        use crate::send::TransmissionBlock;
        use notryx_protocol::WorkspaceOpenRequest;
        use std::time::Duration;
        let mut fixture = delayed_fixture().await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("document must start with remote content");
        };
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_node = node.clone();
        let task = tokio::spawn(async move {
            let bytes = remote.session.content(&task_node).await;
            let binding = registration_binding(remote.session.binding());
            crate::finish_workspace_open(
                &task_runtime,
                remote,
                binding,
                task_node.node_id.to_string(),
                task_node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = fixture.runtime.0.lock().await;
            state
                .transmission_gate
                .block(&fixture.binding, TransmissionBlock::ResyncRequired);
            assert!(
                state
                    .transmission_gate
                    .clear(&fixture.binding, TransmissionBlock::ResyncRequired)
            );
        }
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_sync".into())
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_existing_draft_while_sync_is_blocked() {
        use crate::send::TransmissionBlock;
        use base64::Engine;
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, document) = sync_bootstrap::tests::setup().await;
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        let binding = registration_binding(session.binding());
        runtime
            .0
            .lock()
            .await
            .transmission_gate
            .block(&binding, TransmissionBlock::ProtocolMismatch);
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Draft(opened) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("blocked sync must not prevent draft access");
        };
        assert!(opened.local_draft);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(opened.bytes_base64)
                .unwrap(),
            b"local draft"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_rejects_delayed_network_error_after_registration_changes() {
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_protocol::WorkspaceOpenRequest;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        use std::time::Duration;
        let mut fixture = delayed_fixture().await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        fixture
            .writer
            .put_content(&ContentMutation {
                binding: fixture.writer.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.clone().unwrap(),
                bytes: b"later server revision\n".to_vec(),
            })
            .await
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("document must start with remote content");
        };
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_node = node.clone();
        let task = tokio::spawn(async move {
            let bytes = remote.session.content(&task_node).await;
            assert_eq!(bytes, Err(NetworkError::Changed));
            let binding = registration_binding(remote.session.binding());
            crate::finish_workspace_open(
                &task_runtime,
                remote,
                binding,
                task_node.node_id.to_string(),
                task_node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture.directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture.directory.path().join("notes"),
                state_root: fixture.directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = http::pairing_request(&config).await.0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", fixture.server.local_addr()),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        {
            let mut state = fixture.runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(session.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session);
        }
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_registration".into())
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &fixture.node.node_id.to_string())
                .await
                .unwrap()
                .unwrap(),
            fixture.draft
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_rejects_remote_base_after_metadata_changes() {
        use notryx_protocol::WorkspaceOpenRequest;
        use std::time::Duration;
        let mut fixture = delayed_fixture().await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("document must start with remote content");
        };
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_node = node.clone();
        let task = tokio::spawn(async move {
            let bytes = remote.session.content(&task_node).await;
            let binding = registration_binding(remote.session.binding());
            crate::finish_workspace_open(
                &task_runtime,
                remote,
                binding,
                task_node.node_id.to_string(),
                task_node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        fixture
            .writer
            .put_content(&ContentMutation {
                binding: fixture.writer.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.clone().unwrap(),
                bytes: b"later server revision\n".to_vec(),
            })
            .await
            .unwrap();
        let page = fixture.writer.changes(fixture.cursor, None).await.unwrap();
        fixture
            .runtime
            .0
            .lock()
            .await
            .store
            .apply_changes(&fixture.binding, &page, page.through_seq)
            .await
            .unwrap();
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_content".into())
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_rejects_remote_base_after_server_deletion() {
        use notryx_protocol::WorkspaceOpenRequest;
        use std::time::Duration;
        let mut fixture = delayed_fixture().await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("document must start with remote content");
        };
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let task_node = node.clone();
        let task = tokio::spawn(async move {
            let bytes = remote.session.content(&task_node).await;
            let binding = registration_binding(remote.session.binding());
            crate::finish_workspace_open(
                &task_runtime,
                remote,
                binding,
                task_node.node_id.to_string(),
                task_node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_protocol::PairResponse;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture.directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture.directory.path().join("notes"),
                state_root: fixture.directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pair = http::pairing_request(&config).await.0;
        let response = http::post_json(
            fixture.server.local_addr(),
            &serde_json::to_vec(&pair).unwrap(),
        )
        .await;
        assert_eq!(response.status, 200);
        let device: PairResponse = serde_json::from_slice(&response.body).unwrap();
        let authorization = format!("Bearer {}", device.device_token);
        let epoch = device.workspace_epoch.to_string();
        let operation = uuid::Uuid::now_v7().to_string();
        let response = http::request(
            fixture.server.local_addr(),
            "DELETE",
            &format!("/v1/nodes/{}", node.node_id),
            &[
                ("Authorization", &authorization),
                ("X-Workspace-Epoch", &epoch),
                ("X-Device-Stream", &device.device_stream_id),
                ("X-Operation-Seq", "1"),
                ("Idempotency-Key", &operation),
                ("If-Match", &node.node_etag),
            ],
            &[],
        )
        .await;
        assert_eq!(response.status, 200);
        let page = fixture.writer.changes(fixture.cursor, None).await.unwrap();
        fixture
            .runtime
            .0
            .lock()
            .await
            .store
            .apply_changes(&fixture.binding, &page, page.through_seq)
            .await
            .unwrap();
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_content".into())
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_refuses_remote_base_without_current_index() {
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, _document) =
            sync_bootstrap::tests::setup().await;
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        assert!(matches!(
            crate::begin_workspace_open(&runtime, &request, None).await,
            Ok(crate::WorkspaceOpenStart::Stale(_))
        ));
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .read_registered_checkpoint(
                    &registration_binding(session.binding()),
                    &request.node.node_id.to_string()
                )
                .await
                .unwrap()
                .is_none()
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_contract_error_blocks_current_registration() {
        use crate::send::TransmissionBlock;
        use notryx_protocol::WorkspaceOpenRequest;
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::time::Duration;
        let mode = Arc::new(AtomicU8::new(0));
        let mut fixture = delayed_fixture_with_mode(Some(mode.clone())).await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        mode.store(5, Ordering::SeqCst);
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let node = request.node;
        let task = tokio::spawn(async move {
            let bytes = remote.session.content(&node).await;
            assert_eq!(bytes, Err(NetworkError::Contract));
            let binding = registration_binding(remote.session.binding());
            crate::finish_workspace_open(
                &task_runtime,
                remote,
                binding,
                node.node_id.to_string(),
                node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err("Contract".into())
        );
        let state = fixture.runtime.0.lock().await;
        assert!(
            state
                .transmission_gate
                .has_block(&fixture.binding, TransmissionBlock::ProtocolMismatch)
        );
        assert_eq!(
            state.sync_state.as_ref().unwrap().status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_rejects_renderer_metadata_that_disagrees_with_index_before_get() {
        use notryx_protocol::WorkspaceOpenRequest;
        let mut fixture = delayed_fixture().await;
        let mut node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        node.content_hash = Some(format!("sha256:{}", "0".repeat(64)));
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        assert!(matches!(
            crate::begin_workspace_open(&fixture.runtime, &request, None).await,
            Ok(crate::WorkspaceOpenStart::Stale(_))
        ));
        assert!(
            !fixture
                .runtime
                .0
                .lock()
                .await
                .transmission_gate
                .blocked(&fixture.binding)
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                fixture.observed.recv()
            )
            .await
            .is_err()
        );
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_contract_error_records_gate_and_preserves_draft_created_during_get() {
        use crate::send::TransmissionBlock;
        use base64::Engine;
        use notryx_protocol::WorkspaceOpenRequest;
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::time::Duration;
        let mode = Arc::new(AtomicU8::new(0));
        let mut fixture = delayed_fixture_with_mode(Some(mode.clone())).await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        mode.store(5, Ordering::SeqCst);
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let draft_node = request.node.clone();
        let node = request.node;
        let task = tokio::spawn(async move {
            let bytes = remote.session.content(&node).await;
            assert_eq!(bytes, Err(NetworkError::Contract));
            let binding = registration_binding(remote.session.binding());
            crate::finish_workspace_open(
                &task_runtime,
                remote,
                binding,
                node.node_id.to_string(),
                node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = fixture.runtime.0.lock().await;
            let document = draft_node.node_id.to_string();
            let opened = state
                .store
                .open_registered_document(
                    &fixture.binding,
                    &document,
                    b"server source\n",
                    draft_node.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &fixture.binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"created during download",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&fixture.binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
        }
        fixture.release.send(()).await.unwrap();
        let opened = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(opened.local_draft);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(opened.bytes_base64)
                .unwrap(),
            b"created during download"
        );
        let state = fixture.runtime.0.lock().await;
        assert!(
            state
                .transmission_gate
                .has_block(&fixture.binding, TransmissionBlock::ProtocolMismatch)
        );
        assert_eq!(
            state.sync_state.as_ref().unwrap().status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cache_write_failure_warns_without_blocking_opened_document() {
        use base64::Engine;
        use notryx_protocol::{WorkspaceCacheWarning, WorkspaceOpenRequest};
        use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
        let (directory, server, runtime, session, _document) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_cache_insert BEFORE INSERT ON cached_content BEGIN SELECT RAISE(ABORT, 'synthetic'); END").execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        let bytes = remote.session.content(&request.node).await;
        let binding = registration_binding(session.binding());
        let opened = crate::finish_workspace_open(
            &runtime,
            remote,
            binding.clone(),
            request.node.node_id.to_string(),
            request.node.content_rev.as_ref().unwrap().as_str().into(),
            bytes,
        )
        .await
        .unwrap();
        assert_eq!(opened.cache_warning, Some(WorkspaceCacheWarning::Storage));
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&opened.bytes_base64)
                .unwrap(),
            b"move source\n"
        );
        assert!(!opened.local_draft);
        assert!(!opened.readonly);
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .read_cached_content(&binding, request.node.node_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(state.product_sessions.contains_key(&opened.edit_session_id));
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_oversized_http_response_blocks_current_registration() {
        use crate::send::TransmissionBlock;
        use notryx_protocol::WorkspaceOpenRequest;
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::time::Duration;
        let mode = Arc::new(AtomicU8::new(0));
        let mut fixture = delayed_fixture_with_mode(Some(mode.clone())).await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        mode.store(6, Ordering::SeqCst);
        let task_runtime = Runtime(fixture.runtime.0.clone());
        let node = request.node;
        let task = tokio::spawn(async move {
            let bytes = remote.session.content(&node).await;
            assert_eq!(bytes, Err(NetworkError::Contract));
            let binding = registration_binding(remote.session.binding());
            crate::finish_workspace_open(
                &task_runtime,
                remote,
                binding,
                node.node_id.to_string(),
                node.content_rev.as_ref().unwrap().as_str().into(),
                bytes,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap()
                .unwrap(),
            Err("Contract".into())
        );
        let state = fixture.runtime.0.lock().await;
        assert!(
            state
                .transmission_gate
                .has_block(&fixture.binding, TransmissionBlock::ProtocolMismatch)
        );
        assert_eq!(
            state.sync_state.as_ref().unwrap().status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        drop(state);
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stale_download_catches_up_and_retrieves_current_revision() {
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, _document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        session
            .put_content(&ContentMutation {
                binding: session.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.clone().unwrap(),
                bytes: b"new remote content\n".to_vec(),
            })
            .await
            .unwrap();
        assert_eq!(
            remote.session.content(&node).await,
            Err(NetworkError::Changed)
        );
        let latest = crate::refresh_open_node(&runtime, &remote)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(latest.content_rev, node.content_rev);
        let binding = registration_binding(session.binding());
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .read_registered_checkpoint(&binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        let retry = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: latest.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(next) =
            crate::begin_workspace_open(&runtime, &retry, Some(&remote))
                .await
                .unwrap()
        else {
            panic!("current metadata must be requested after catch-up");
        };
        let bytes = next.session.content(&latest).await;
        assert_eq!(bytes.as_deref(), Ok(b"new remote content\n".as_slice()));
        let opened = crate::finish_workspace_open(
            &runtime,
            next,
            binding,
            latest.node_id.to_string(),
            latest.content_rev.as_ref().unwrap().as_str().into(),
            bytes,
        )
        .await
        .unwrap();
        assert_eq!(opened.cache_warning, None);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retry_catchup_stops_when_server_is_offline_without_creating_a_draft() {
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, _document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        server.shutdown().await.unwrap();
        assert_eq!(
            crate::refresh_open_node(&runtime, &remote).await,
            Err("Network".into())
        );
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .read_registered_checkpoint(
                    &registration_binding(session.binding()),
                    &node.node_id.to_string()
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn retry_does_not_open_a_new_registration() {
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_protocol::WorkspaceOpenRequest;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        let fixture = delayed_fixture().await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node,
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture.directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture.directory.path().join("notes"),
                state_root: fixture.directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = http::pairing_request(&config).await.0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", fixture.server.local_addr()),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        {
            let mut state = fixture.runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(session.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session);
        }
        assert!(
            matches!(crate::begin_workspace_open(&fixture.runtime, &request, Some(&remote)).await, Err(error) if error == "stale_registration")
        );
        assert_eq!(
            crate::refresh_open_node(&fixture.runtime, &remote).await,
            Err("stale_registration".into())
        );
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retry_prefers_draft_created_during_catchup() {
        use base64::Engine;
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, _document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        session
            .put_content(&ContentMutation {
                binding: session.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.clone().unwrap(),
                bytes: b"new remote content\n".to_vec(),
            })
            .await
            .unwrap();
        assert_eq!(
            remote.session.content(&node).await,
            Err(NetworkError::Changed)
        );
        let latest = crate::refresh_open_node(&runtime, &remote)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(latest.content_rev, node.content_rev);
        let binding = registration_binding(session.binding());
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .read_registered_checkpoint(&binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        {
            let mut state = runtime.0.lock().await;
            let document = node.node_id.to_string();
            let opened = state
                .store
                .open_registered_document(
                    &binding,
                    &document,
                    b"move source\n",
                    node.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"local draft while syncing",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
        }
        let retry = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: latest,
        };
        let crate::WorkspaceOpenStart::Draft(opened) =
            crate::begin_workspace_open(&runtime, &retry, Some(&remote))
                .await
                .unwrap()
        else {
            panic!("local draft must win during retry")
        };
        assert!(opened.local_draft);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(opened.bytes_base64)
                .unwrap(),
            b"local draft while syncing"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn deleted_before_get_triggers_tombstone_catchup() {
        use notryx_protocol::WorkspaceOpenRequest;
        let fixture = delayed_fixture().await;
        let node = fixture
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "example.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&fixture.runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_protocol::PairResponse;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture.directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture.directory.path().join("notes"),
                state_root: fixture.directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pair = http::pairing_request(&config).await.0;
        let response = http::post_json(
            fixture.server.local_addr(),
            &serde_json::to_vec(&pair).unwrap(),
        )
        .await;
        assert_eq!(response.status, 200);
        let device: PairResponse = serde_json::from_slice(&response.body).unwrap();
        let authorization = format!("Bearer {}", device.device_token);
        let epoch = device.workspace_epoch.to_string();
        let operation = uuid::Uuid::now_v7().to_string();
        let response = http::request(
            fixture.server.local_addr(),
            "DELETE",
            &format!("/v1/nodes/{}", node.node_id),
            &[
                ("Authorization", &authorization),
                ("X-Workspace-Epoch", &epoch),
                ("X-Device-Stream", &device.device_stream_id),
                ("X-Operation-Seq", "1"),
                ("Idempotency-Key", &operation),
                ("If-Match", &node.node_etag),
            ],
            &[],
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            fixture.writer.content(&node).await,
            Err(NetworkError::Changed)
        );
        assert_eq!(
            crate::refresh_open_node(&fixture.runtime, &remote)
                .await
                .unwrap(),
            None
        );
        assert!(
            fixture
                .runtime
                .0
                .lock()
                .await
                .store
                .read_registered_checkpoint(&fixture.binding, &node.node_id.to_string())
                .await
                .unwrap()
                .is_none()
        );
        fixture.proxy.abort();
        let _ = fixture.proxy.await;
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_catchup_prefers_new_local_draft() {
        use base64::Engine;
        use notryx_protocol::WorkspaceOpenRequest;
        let (_directory, server, runtime, session, _document) =
            sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let request = WorkspaceOpenRequest {
            renderer_session_id: "owner".into(),
            node: node.clone(),
        };
        let crate::WorkspaceOpenStart::Remote(remote) =
            crate::begin_workspace_open(&runtime, &request, None)
                .await
                .unwrap()
        else {
            panic!("remote open expected")
        };
        server.shutdown().await.unwrap();
        let result = crate::refresh_open_node(&runtime, &remote).await;
        assert_eq!(result, Err("Network".into()));
        let binding = registration_binding(session.binding());
        {
            let mut state = runtime.0.lock().await;
            let document = node.node_id.to_string();
            let opened = state
                .store
                .open_registered_document(
                    &binding,
                    &document,
                    b"move source\n",
                    node.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"retained draft",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
        }
        let crate::WorkspaceRefresh::Draft(opened) =
            crate::finish_open_refresh(&runtime, &remote, result)
                .await
                .unwrap()
        else {
            panic!("local draft must be available after failed catch-up")
        };
        assert!(opened.local_draft);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(opened.bytes_base64)
                .unwrap(),
            b"retained draft"
        );
    }
}
