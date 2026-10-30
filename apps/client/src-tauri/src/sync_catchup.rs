use crate::{
    Runtime, RuntimeState, gate::RequestSnapshot, registration_binding, send::TransmissionBlock,
    sync_bootstrap::checked_state,
};
use notryx_client_runtime::{Error, Session};
use notryx_client_store::StoreError;
use notryx_domain::ChangeSeq;
use std::sync::Arc;
use tokio::sync::MutexGuard;

#[derive(Debug)]
pub(crate) enum CatchUpError {
    Guard(String),
    Store(StoreError),
    Network(Error, u64),
    BootstrapRequired,
    StaleSync,
    ServerNotReady(u64),
    HistoryRegression,
    ResyncRequired,
    BindingMismatch,
    ProtocolMismatch,
}

impl CatchUpError {
    pub(crate) fn message(self) -> String {
        match self {
            Self::Guard(error) => error,
            Self::Store(error) => error.to_string(),
            Self::Network(error, _) => format!("{error:?}"),
            Self::BootstrapRequired => "bootstrap_required".into(),
            Self::StaleSync => "stale_sync".into(),
            Self::ServerNotReady(_) => "server_not_ready".into(),
            Self::HistoryRegression => "history_regressed".into(),
            Self::ResyncRequired => "resync_required".into(),
            Self::BindingMismatch => "binding_mismatch".into(),
            Self::ProtocolMismatch => "protocol_mismatch".into(),
        }
    }
}

#[allow(dead_code)]
pub(crate) async fn catch_up_metadata(
    runtime: &Runtime,
    snapshot: &RequestSnapshot,
    session: &Arc<Session>,
) -> Result<ChangeSeq, String> {
    catch_up(runtime, Some(snapshot), session)
        .await
        .map_err(CatchUpError::message)
}

pub(crate) async fn catch_up(
    runtime: &Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
) -> Result<ChangeSeq, CatchUpError> {
    catch_up_for_generation(runtime, snapshot, session, None).await
}

pub(crate) async fn catch_up_for_generation(
    runtime: &Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
    expected_generation: Option<u64>,
) -> Result<ChangeSeq, CatchUpError> {
    let binding = registration_binding(session.binding());
    let mut state = sync_state(runtime, snapshot, session).await?;
    let generation = expected_generation
        .unwrap_or_else(|| state.transmission_gate.response_generation(&binding));
    if !state
        .transmission_gate
        .metadata_response_trusted(&binding, generation)
    {
        return Err(CatchUpError::StaleSync);
    }
    let initial_sync = state
        .transmission_gate
        .has_block(&binding, TransmissionBlock::InitialSync);
    if state
        .transmission_gate
        .has_block(&binding, TransmissionBlock::ResyncRequired)
        && (expected_generation.is_none() || !initial_sync)
    {
        return Err(CatchUpError::ResyncRequired);
    }
    if expected_generation.is_none() && initial_sync {
        return Err(CatchUpError::StaleSync);
    }
    let (mut cursor, nodes) = state
        .store
        .sync_index(&binding)
        .await
        .map_err(CatchUpError::Store)?
        .ok_or(CatchUpError::BootstrapRequired)?;
    let root = nodes
        .iter()
        .find(|node| node.parent_id.is_none() && !node.deleted)
        .ok_or(CatchUpError::Store(StoreError::UnsupportedFormat))?
        .node_id;
    drop(state);
    let info = match session.server_info().await {
        Ok(info) => info,
        Err(error) => return network_error(runtime, snapshot, session, generation, error).await,
    };
    {
        let mut state = sync_state(runtime, snapshot, session).await?;
        if !state
            .transmission_gate
            .metadata_response_trusted(&binding, generation)
        {
            return Err(CatchUpError::StaleSync);
        }
        if info.root_id != root {
            state
                .transmission_gate
                .block(&binding, TransmissionBlock::BindingMismatch);
            return Err(CatchUpError::BindingMismatch);
        }
        if info.head_seq < cursor {
            state
                .transmission_gate
                .block(&binding, TransmissionBlock::HistoryRegression);
            return Err(CatchUpError::HistoryRegression);
        }
        if info.journal_floor_seq > cursor {
            state
                .transmission_gate
                .block(&binding, TransmissionBlock::ResyncRequired);
            return Err(CatchUpError::ResyncRequired);
        }
        if !info.ready {
            return Err(CatchUpError::ServerNotReady(generation));
        }
    }
    let through = info.head_seq;
    while cursor < through {
        let page = match session.changes(cursor, Some(through)).await {
            Ok(page) => page,
            Err(Error::Response) => {
                let latest = match session.server_info().await {
                    Ok(info) => info,
                    Err(error) => {
                        return network_error(runtime, snapshot, session, generation, error).await;
                    }
                };
                let mut state = sync_state(runtime, snapshot, session).await?;
                if !state
                    .transmission_gate
                    .metadata_response_trusted(&binding, generation)
                {
                    return Err(CatchUpError::StaleSync);
                }
                if latest.root_id != root {
                    state
                        .transmission_gate
                        .block(&binding, TransmissionBlock::BindingMismatch);
                    return Err(CatchUpError::BindingMismatch);
                }
                if latest.head_seq < through {
                    state
                        .transmission_gate
                        .block(&binding, TransmissionBlock::HistoryRegression);
                    return Err(CatchUpError::HistoryRegression);
                }
                if latest.journal_floor_seq > cursor {
                    state
                        .transmission_gate
                        .block(&binding, TransmissionBlock::ResyncRequired);
                    return Err(CatchUpError::ResyncRequired);
                }
                return Err(CatchUpError::Network(Error::Response, generation));
            }
            Err(error) => {
                return network_error(runtime, snapshot, session, generation, error).await;
            }
        };
        let mut state = sync_state(runtime, snapshot, session).await?;
        if !state
            .transmission_gate
            .metadata_response_trusted(&binding, generation)
        {
            return Err(CatchUpError::StaleSync);
        }
        let current = state
            .store
            .sync_index(&binding)
            .await
            .map_err(CatchUpError::Store)?
            .ok_or(CatchUpError::BootstrapRequired)?
            .0;
        if current != cursor {
            return Err(CatchUpError::StaleSync);
        }
        if let Err(error) = state.store.apply_changes(&binding, &page, through).await {
            if matches!(
                error,
                StoreError::InvalidRequest | StoreError::UnsupportedFormat
            ) {
                state
                    .transmission_gate
                    .block(&binding, TransmissionBlock::ProtocolMismatch);
            }
            return Err(CatchUpError::Store(error));
        }
        cursor = page.covered_through;
    }
    Ok(cursor)
}

async fn network_error(
    runtime: &Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
    generation: u64,
    error: Error,
) -> Result<ChangeSeq, CatchUpError> {
    let mut state = sync_state(runtime, snapshot, session).await?;
    let binding = registration_binding(session.binding());
    if !state
        .transmission_gate
        .metadata_response_trusted(&binding, generation)
    {
        return Err(CatchUpError::StaleSync);
    }
    let (reason, result) = match error {
        Error::Binding | Error::EpochMismatch => (
            Some(TransmissionBlock::BindingMismatch),
            CatchUpError::BindingMismatch,
        ),
        Error::ResyncRequired => (
            Some(TransmissionBlock::ResyncRequired),
            CatchUpError::ResyncRequired,
        ),
        Error::Contract | Error::TooLarge => (
            Some(TransmissionBlock::ProtocolMismatch),
            CatchUpError::ProtocolMismatch,
        ),
        other => (None, CatchUpError::Network(other, generation)),
    };
    if let Some(reason) = reason {
        state.transmission_gate.block(&binding, reason);
    }
    Err(result)
}

async fn sync_state<'a>(
    runtime: &'a Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
) -> Result<MutexGuard<'a, RuntimeState>, CatchUpError> {
    match snapshot {
        Some(snapshot) => checked_state(runtime, snapshot, session).await,
        None => crate::sync_lifecycle::checked_registration(runtime, session).await,
    }
    .map_err(CatchUpError::Guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        registration_binding,
        sync_bootstrap::{
            bootstrap_metadata, server_test_support as http,
            tests::{setup, test_proxy},
        },
    };
    use notryx_client_runtime::ContentMutation;
    use notryx_client_store::{OutboxOperation, RegistrationBinding};
    use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
    use notryx_server::http::RunningServer;
    use std::sync::atomic::{AtomicU8, Ordering};

    struct ProxyFixture {
        _directory: tempfile::TempDir,
        server: RunningServer,
        runtime: Runtime,
        session: Arc<Session>,
        snapshot: RequestSnapshot,
        binding: RegistrationBinding,
        document: String,
        cursor: ChangeSeq,
        operation: OutboxOperation,
        mode: Arc<AtomicU8>,
        observed: tokio::sync::mpsc::Receiver<()>,
        release: tokio::sync::mpsc::Sender<()>,
        proxy: tokio::task::JoinHandle<()>,
    }

    async fn proxy_fixture() -> ProxyFixture {
        proxy_fixture_with_points(vec![]).await
    }

    async fn proxy_fixture_with_points(points: Vec<&'static str>) -> ProxyFixture {
        let (directory, server, runtime, original, _document) = setup().await;
        let node = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let document = node.node_id.to_string();
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
        let mode = Arc::new(AtomicU8::new(0));
        let (address, observed, release, proxy) =
            test_proxy(server.local_addr(), points, false, Some(mode.clone())).await;
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
        let operation = {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
            let opened = state
                .store
                .open_registered_document(&binding, &document, b"peer base", "peer-base-revision")
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"peer draft",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
            state
                .store
                .queue_checkpoint(&binding, &document, 1)
                .await
                .unwrap()
        };
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cursor = bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let writer_pairing = http::pairing_request(&config).await.0;
        let writer = Session::pair(
            &format!("http://{}", server.local_addr()),
            writer_pairing.workspace_id,
            &writer_pairing.pairing_secret,
        )
        .await
        .unwrap();
        writer
            .put_content(&ContentMutation {
                binding: writer.binding().clone(),
                operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.unwrap(),
                bytes: b"remote source\n".to_vec(),
            })
            .await
            .unwrap();
        ProxyFixture {
            _directory: directory,
            server,
            runtime,
            session,
            snapshot,
            binding,
            document,
            cursor,
            operation,
            mode,
            observed,
            release,
            proxy,
        }
    }

    async fn assert_blocked_and_preserved(
        fixture: ProxyFixture,
        mode: u8,
        expected: &str,
        expected_status: notryx_protocol::WorkspaceSendStatus,
        expected_block: crate::send::TransmissionBlock,
    ) {
        fixture.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            catch_up_metadata(&fixture.runtime, &fixture.snapshot, &fixture.session).await,
            Err(expected.into())
        );
        assert_eq!(
            crate::send::send_one(fixture.runtime.0.clone(), "owner")
                .await
                .unwrap()
                .status,
            expected_status
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .transmission_gate
                .has_block(&fixture.binding, expected_block)
        );
        assert_eq!(
            state
                .store
                .sync_index(&fixture.binding)
                .await
                .unwrap()
                .unwrap()
                .0,
            fixture.cursor
        );
        let draft = state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"peer draft");
        assert_eq!(draft.base_bytes, b"peer base");
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
                .await
                .unwrap(),
            Some(fixture.operation)
        );
        drop(state);
        assert_eq!(
            std::fs::read(fixture._directory.path().join("notes/move.md")).unwrap(),
            b"remote source\n"
        );
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resync_required_blocks_automatic_send_and_preserves_cursor_and_draft() {
        assert_blocked_and_preserved(
            proxy_fixture().await,
            1,
            "resync_required",
            notryx_protocol::WorkspaceSendStatus::SyncPending,
            crate::send::TransmissionBlock::ResyncRequired,
        )
        .await;
    }

    #[tokio::test]
    async fn server_binding_mismatch_blocks_automatic_send_and_preserves_cursor_and_draft() {
        assert_blocked_and_preserved(
            proxy_fixture().await,
            2,
            "binding_mismatch",
            notryx_protocol::WorkspaceSendStatus::NeedsReview,
            crate::send::TransmissionBlock::BindingMismatch,
        )
        .await;
    }

    async fn writer(fixture: &ProxyFixture) -> notryx_protocol::PairResponse {
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture._directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture._directory.path().join("notes"),
                state_root: fixture._directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = http::pairing_request(&config).await.0;
        let paired = http::post_json(
            fixture.server.local_addr(),
            &serde_json::to_vec(&pairing).unwrap(),
        )
        .await;
        assert_eq!(paired.status, 200);
        serde_json::from_slice(&paired.body).unwrap()
    }

    async fn create_node(
        fixture: &ProxyFixture,
        writer: &notryx_protocol::PairResponse,
        root: &str,
        sequence: u64,
    ) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let operation = uuid::Uuid::now_v7().to_string();
        let auth = format!("Bearer {}", writer.device_token);
        let epoch = writer.workspace_epoch.to_string();
        let sequence_string = sequence.to_string();
        let body = serde_json::to_vec(&serde_json::json!({"node_id":id,"parent_id":root,"name":format!("created-{sequence}.md"),"kind":"file"})).unwrap();
        let result = http::request(
            fixture.server.local_addr(),
            "POST",
            "/v1/nodes",
            &[
                ("Authorization", &auth),
                ("X-Workspace-Epoch", &epoch),
                ("X-Device-Stream", &writer.device_stream_id),
                ("X-Operation-Seq", &sequence_string),
                ("Idempotency-Key", &operation),
                ("Content-Type", "application/json"),
            ],
            &body,
        )
        .await;
        assert_eq!(result.status, 200);
        id
    }

    #[tokio::test]
    async fn paged_catch_up_uses_first_head_when_server_changes_between_pages() {
        use std::time::Duration;

        let mut fixture = proxy_fixture_with_points(vec!["GET /v1/changes?"]).await;
        let root = fixture
            .session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.parent_id.is_none())
            .unwrap()
            .node_id
            .to_string();
        let writer = writer(&fixture).await;
        let first_created = create_node(&fixture, &writer, &root, 1).await;
        let through = fixture.session.server_info().await.unwrap().head_seq;
        fixture.mode.store(3, Ordering::SeqCst);
        let snapshot = fixture
            .runtime
            .0
            .lock()
            .await
            .gate
            .snapshot("owner")
            .unwrap();
        let runtime = Runtime(fixture.runtime.0.clone());
        let session = fixture.session.clone();
        let task =
            tokio::spawn(async move { catch_up_metadata(&runtime, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        let later_created = create_node(&fixture, &writer, &root, 2).await;
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            through
        );
        let latest = fixture.session.server_info().await.unwrap().head_seq;
        assert!(latest > through);
        let mut state = fixture.runtime.0.lock().await;
        let (cursor, nodes) = state
            .store
            .sync_index(&fixture.binding)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cursor, through);
        assert!(
            nodes
                .iter()
                .any(|node| node.node_id.to_string() == first_created)
        );
        assert!(
            !nodes
                .iter()
                .any(|node| node.node_id.to_string() == later_created)
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
                .await
                .unwrap(),
            Some(fixture.operation.clone())
        );
        drop(state);
        assert_eq!(
            catch_up_metadata(&fixture.runtime, &fixture.snapshot, &fixture.session)
                .await
                .unwrap(),
            latest
        );
        assert!(
            fixture
                .runtime
                .0
                .lock()
                .await
                .store
                .sync_index(&fixture.binding)
                .await
                .unwrap()
                .unwrap()
                .1
                .iter()
                .any(|node| node.node_id.to_string() == later_created)
        );
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_later_page_keeps_last_complete_cursor_and_outbox_for_retry() {
        use std::time::Duration;

        let mut fixture = proxy_fixture_with_points(vec!["GET /v1/changes?"]).await;
        let root = fixture
            .session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.parent_id.is_none())
            .unwrap()
            .node_id
            .to_string();
        let writer = writer(&fixture).await;
        let created = create_node(&fixture, &writer, &root, 1).await;
        let through = fixture.session.server_info().await.unwrap().head_seq;
        assert_eq!(through.get(), fixture.cursor.get() + 2);
        fixture.mode.store(3, Ordering::SeqCst);
        let snapshot = fixture
            .runtime
            .0
            .lock()
            .await
            .gate
            .snapshot("owner")
            .unwrap();
        let runtime = Runtime(fixture.runtime.0.clone());
        let session = fixture.session.clone();
        let task =
            tokio::spawn(async move { catch_up_metadata(&runtime, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        fixture.mode.store(4, Ordering::SeqCst);
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap()
                .unwrap(),
            Err("Unavailable".into())
        );
        let mut state = fixture.runtime.0.lock().await;
        let (cursor, nodes) = state
            .store
            .sync_index(&fixture.binding)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cursor.get(), fixture.cursor.get() + 1);
        assert!(!nodes.iter().any(|node| node.node_id.to_string() == created));
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
                .await
                .unwrap(),
            Some(fixture.operation.clone())
        );
        let draft = state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"peer draft");
        assert_eq!(draft.base_bytes, b"peer base");
        drop(state);
        fixture.mode.store(0, Ordering::SeqCst);
        assert_eq!(
            catch_up_metadata(&fixture.runtime, &fixture.snapshot, &fixture.session)
                .await
                .unwrap(),
            through
        );
        assert!(
            fixture
                .runtime
                .0
                .lock()
                .await
                .store
                .sync_index(&fixture.binding)
                .await
                .unwrap()
                .unwrap()
                .1
                .iter()
                .any(|node| node.node_id.to_string() == created)
        );
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ordinary_catch_up_does_not_advance_while_resync_is_pending() {
        let fixture = proxy_fixture().await;
        let before = fixture
            .runtime
            .0
            .lock()
            .await
            .store
            .sync_index(&fixture.binding)
            .await
            .unwrap()
            .unwrap();
        fixture
            .runtime
            .0
            .lock()
            .await
            .transmission_gate
            .block(&fixture.binding, TransmissionBlock::ResyncRequired);
        let result = catch_up_metadata(&fixture.runtime, &fixture.snapshot, &fixture.session).await;
        let mut state = fixture.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .sync_index(&fixture.binding)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        assert!(result.is_err());
        drop(state);
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn changed_root_at_same_head_blocks_catch_up_and_send() {
        let fixture = proxy_fixture().await;
        catch_up_metadata(&fixture.runtime, &fixture.snapshot, &fixture.session)
            .await
            .unwrap();
        let before = fixture
            .runtime
            .0
            .lock()
            .await
            .store
            .sync_index(&fixture.binding)
            .await
            .unwrap()
            .unwrap();
        fixture.mode.store(7, Ordering::SeqCst);
        assert_eq!(
            catch_up_metadata(&fixture.runtime, &fixture.snapshot, &fixture.session).await,
            Err("binding_mismatch".into())
        );
        let mut state = fixture.runtime.0.lock().await;
        assert!(
            state
                .transmission_gate
                .has_block(&fixture.binding, TransmissionBlock::BindingMismatch)
        );
        assert_eq!(
            state
                .store
                .sync_index(&fixture.binding)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        drop(state);
        assert_eq!(
            crate::send::send_one(fixture.runtime.0.clone(), "owner")
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSendStatus::NeedsReview
        );
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalidated_changes_response_preserves_index_and_draft() {
        use std::time::Duration;

        let mut fixture = proxy_fixture_with_points(vec!["GET /v1/changes?"]).await;
        let before = fixture
            .runtime
            .0
            .lock()
            .await
            .store
            .sync_index(&fixture.binding)
            .await
            .unwrap()
            .unwrap();
        let snapshot = fixture.snapshot.clone();
        let runtime = Runtime(fixture.runtime.0.clone());
        let session = fixture.session.clone();
        let task =
            tokio::spawn(async move { catch_up_metadata(&runtime, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = fixture.runtime.0.lock().await;
            state
                .transmission_gate
                .block(&fixture.binding, TransmissionBlock::ProtocolMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&fixture.binding, TransmissionBlock::ProtocolMismatch)
            );
        }
        fixture.release.send(()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap();
        let mut state = fixture.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .sync_index(&fixture.binding)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        assert!(result.is_err());
        let draft = state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"peer draft");
        assert_eq!(draft.base_bytes, b"peer base");
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
                .await
                .unwrap(),
            Some(fixture.operation)
        );
        drop(state);
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn re_registration_while_changes_response_waits_preserves_cursor_and_outbox() {
        use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
        use std::time::Duration;

        let mut fixture = proxy_fixture_with_points(vec!["GET /v1/changes?"]).await;
        let snapshot = fixture
            .runtime
            .0
            .lock()
            .await
            .gate
            .snapshot("owner")
            .unwrap();
        let runtime = Runtime(fixture.runtime.0.clone());
        let session = fixture.session.clone();
        let task =
            tokio::spawn(async move { catch_up_metadata(&runtime, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), fixture.observed.recv())
            .await
            .unwrap()
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: fixture.server.local_addr(),
                admin_socket: fixture._directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: fixture._directory.path().join("notes"),
                state_root: fixture._directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pair = http::pairing_request(&config).await.0;
        let replacement = Arc::new(
            Session::pair(
                &format!("http://{}", fixture.server.local_addr()),
                pair.workspace_id,
                &pair.pairing_secret,
            )
            .await
            .unwrap(),
        );
        {
            let mut state = fixture.runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(replacement.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(replacement);
        }
        fixture.release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_registration".into())
        );
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(fixture._directory.path().join("client.sqlite"))
                .read_only(true),
        )
        .await
        .unwrap();
        let cursor: String = sqlx::query_scalar("SELECT r.cursor FROM remote_index r JOIN client_registrations c ON c.id=r.registration_id WHERE c.server_origin=? AND c.workspace_id=? AND c.workspace_epoch=? AND c.device_id=? AND c.device_stream_id=?")
            .bind(&fixture.binding.server_origin).bind(&fixture.binding.workspace_id).bind(&fixture.binding.workspace_epoch).bind(&fixture.binding.device_id).bind(&fixture.binding.device_stream_id).fetch_one(&mut db).await.unwrap();
        db.close().await.unwrap();
        assert_eq!(cursor, fixture.cursor.get().to_string());
        let mut state = fixture.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"peer draft");
        assert_eq!(draft.base_bytes, b"peer base");
        let mut expected = fixture.operation.clone();
        expected.frozen = true;
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &expected.operation_id)
                .await
                .unwrap(),
            Some(expected)
        );
        drop(state);
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn catch_up_reads_real_server_changes_and_keeps_local_outbox() {
        let (directory, server, runtime, session, document) = setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let h0 = bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let before = session.inventory(None).await.unwrap();
        let node = before
            .nodes
            .iter()
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
        let request = http::pairing_request(&config).await.0;
        let writer = Session::pair(
            &format!("http://{}", server.local_addr()),
            request.workspace_id,
            &request.pairing_secret,
        )
        .await
        .unwrap();
        let mutation = ContentMutation {
            binding: writer.binding().clone(),
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_revision: node.content_rev.clone().unwrap(),
            bytes: b"remote change\n".to_vec(),
        };
        writer.put_content(&mutation).await.unwrap();
        let binding = registration_binding(session.binding());
        let old_operation = {
            let mut state = runtime.0.lock().await;
            let id = state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .operation_id
                .clone();
            state
                .store
                .read_outbox_operation(&binding, &id)
                .await
                .unwrap()
                .unwrap()
        };
        let cursor = catch_up_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let latest = session.server_info().await.unwrap().head_seq;
        assert_eq!(cursor, latest);
        assert!(cursor > h0);
        let mut state = runtime.0.lock().await;
        let (stored_cursor, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        assert_eq!(stored_cursor, latest);
        let updated = nodes
            .iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        assert_ne!(updated.content_rev, node.content_rev);
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert_eq!(draft.base_bytes, b"base");
        assert_eq!(
            state
                .store
                .read_outbox_operation(&binding, &old_operation.operation_id)
                .await
                .unwrap(),
            Some(old_operation)
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn server_head_behind_cursor_blocks_sends_and_preserves_local_state() {
        use notryx_protocol::WorkspaceSendStatus;
        use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

        let (directory, server, runtime, session, document) = setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cursor = bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let binding = registration_binding(session.binding());
        let before = {
            let mut state = runtime.0.lock().await;
            let id = state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .operation_id
                .clone();
            state
                .store
                .read_outbox_operation(&binding, &id)
                .await
                .unwrap()
                .unwrap()
        };
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite")),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE remote_index SET cursor=?")
            .bind((cursor.get() + 1).to_string())
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        assert_eq!(
            catch_up_metadata(&runtime, &snapshot, &session).await,
            Err("history_regressed".into())
        );
        let outcome = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(outcome.status, WorkspaceSendStatus::NeedsReview);
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .sync_index(&binding)
                .await
                .unwrap()
                .unwrap()
                .0
                .get(),
            cursor.get() + 1
        );
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert_eq!(draft.base_bytes, b"base");
        assert_eq!(
            state
                .store
                .read_outbox_operation(&binding, &before.operation_id)
                .await
                .unwrap(),
            Some(before)
        );
        drop(state);
        assert_eq!(
            std::fs::read(directory.path().join("notes/example.md")).unwrap(),
            b"server source\n"
        );
        server.shutdown().await.unwrap();
    }
}
