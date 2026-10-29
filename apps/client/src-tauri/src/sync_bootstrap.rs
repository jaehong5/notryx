use crate::{Runtime, RuntimeState, gate::RequestSnapshot, registration_binding};
use notryx_client_runtime::{Error as NetworkError, Session};
use notryx_client_store::StoreError;
use notryx_domain::ChangeSeq;
use notryx_protocol::ServerInfo;
use std::sync::Arc;
use tokio::sync::MutexGuard;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../server/tests/support/mod.rs"]
pub(crate) mod server_test_support;

#[allow(dead_code)]
pub(crate) async fn bootstrap_metadata(
    runtime: &Runtime,
    snapshot: &RequestSnapshot,
    session: &Arc<Session>,
) -> Result<ChangeSeq, String> {
    let binding = registration_binding(session.binding());
    let state = checked_state(runtime, snapshot, session).await?;
    let generation = state.transmission_gate.response_generation(&binding);
    drop(state);
    let initial = session
        .server_info()
        .await
        .map_err(|error| format!("{error:?}"))?;
    bootstrap_with_initial(runtime, Some(snapshot), session, initial, generation)
        .await
        .map_err(|error| match error {
            BootstrapError::Remote(error, _) => format!("{error:?}"),
            BootstrapError::Local(error) => error,
            BootstrapError::Store(error) => error.to_string(),
            BootstrapError::ServerNotReady(_) => "server_not_ready".into(),
        })
}

#[derive(Debug)]
pub(crate) enum BootstrapError {
    Remote(NetworkError, u64),
    Local(String),
    Store(StoreError),
    ServerNotReady(u64),
}

pub(crate) async fn bootstrap_with_initial(
    runtime: &Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
    initial: ServerInfo,
    generation: u64,
) -> Result<ChangeSeq, BootstrapError> {
    let binding = registration_binding(session.binding());
    if !initial.ready {
        drop(checked_metadata_state(runtime, snapshot, session, generation).await?);
        return Err(BootstrapError::ServerNotReady(generation));
    }
    let bootstrap = checked_metadata_state(runtime, snapshot, session, generation)
        .await?
        .store
        .begin_bootstrap(&binding, &initial)
        .await
        .map_err(BootstrapError::Store)?;

    let mut after = None;
    loop {
        let page = metadata_network_result(
            runtime,
            snapshot,
            session,
            generation,
            session.inventory(after).await,
        )
        .await?;
        checked_metadata_state(runtime, snapshot, session, generation)
            .await?
            .store
            .bootstrap_inventory(&binding, &bootstrap, &page)
            .await
            .map_err(BootstrapError::Store)?;
        after = page.next_after_id;
        if after.is_none() {
            break;
        }
    }

    let end = metadata_network_result(
        runtime,
        snapshot,
        session,
        generation,
        session.server_info().await,
    )
    .await?;
    if !end.ready {
        drop(checked_metadata_state(runtime, snapshot, session, generation).await?);
        return Err(BootstrapError::ServerNotReady(generation));
    }
    checked_metadata_state(runtime, snapshot, session, generation)
        .await?
        .store
        .bootstrap_finish_inventory(&binding, &bootstrap, &end)
        .await
        .map_err(BootstrapError::Store)?;
    let mut cursor = initial.head_seq;
    while cursor < end.head_seq {
        let page = metadata_network_result(
            runtime,
            snapshot,
            session,
            generation,
            session.changes(cursor, Some(end.head_seq)).await,
        )
        .await?;
        checked_metadata_state(runtime, snapshot, session, generation)
            .await?
            .store
            .bootstrap_changes(&binding, &bootstrap, &page)
            .await
            .map_err(BootstrapError::Store)?;
        cursor = page.covered_through;
    }
    let latest = metadata_network_result(
        runtime,
        snapshot,
        session,
        generation,
        session.server_info().await,
    )
    .await?;
    if !latest.ready {
        drop(checked_metadata_state(runtime, snapshot, session, generation).await?);
        return Err(BootstrapError::ServerNotReady(generation));
    }
    checked_metadata_state(runtime, snapshot, session, generation)
        .await?
        .store
        .bootstrap_publish(&binding, &bootstrap, &latest)
        .await
        .map_err(BootstrapError::Store)?;
    Ok(end.head_seq)
}

pub(crate) async fn metadata_network_result<T>(
    runtime: &Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
    generation: u64,
    result: Result<T, NetworkError>,
) -> Result<T, BootstrapError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            drop(checked_metadata_state(runtime, snapshot, session, generation).await?);
            Err(BootstrapError::Remote(error, generation))
        }
    }
}

async fn checked_metadata_state<'a>(
    runtime: &'a Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
    generation: u64,
) -> Result<MutexGuard<'a, RuntimeState>, BootstrapError> {
    let state = checked_sync_state(runtime, snapshot, session).await?;
    let binding = registration_binding(session.binding());
    if !state
        .transmission_gate
        .metadata_response_trusted(&binding, generation)
    {
        return Err(BootstrapError::Local("stale_sync".into()));
    }
    Ok(state)
}

async fn checked_sync_state<'a>(
    runtime: &'a Runtime,
    snapshot: Option<&RequestSnapshot>,
    session: &Arc<Session>,
) -> Result<MutexGuard<'a, RuntimeState>, BootstrapError> {
    match snapshot {
        Some(snapshot) => checked_state(runtime, snapshot, session).await,
        None => crate::sync_lifecycle::checked_registration(runtime, session).await,
    }
    .map_err(BootstrapError::Local)
}

pub(crate) async fn checked_state<'a>(
    runtime: &'a Runtime,
    snapshot: &RequestSnapshot,
    session: &Arc<Session>,
) -> Result<MutexGuard<'a, RuntimeState>, String> {
    let mut state = runtime.0.lock().await;
    state.gate.validate(snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, session))
        || state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&registration_binding(session.binding()))
    {
        return Err("stale_registration".into());
    }
    Ok(state)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{RuntimeState, registration_binding};
    use notryx_client_store::ClientStore;
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use std::os::unix::fs::PermissionsExt;
    use tokio::sync::Mutex;

    pub(crate) async fn setup() -> (
        tempfile::TempDir,
        RunningServer,
        Runtime,
        Arc<Session>,
        String,
    ) {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("example.md"), b"server source\n").unwrap();
        let node = workspace.adopt_root_document("example.md").await.unwrap();
        std::fs::write(notes.join("move.md"), b"move source\n").unwrap();
        workspace.adopt_root_document("move.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes,
                state_root: server_state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result:
                AdminResult::Status {
                    workspace_id,
                    owner_principal_id,
                    ..
                },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing")
        };
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("client.sqlite"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let opened = store
            .open_registered_document(
                &binding,
                &node.node_id.to_string(),
                b"base",
                "base-revision",
            )
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &binding,
                &node.node_id.to_string(),
                &opened.edit_session_id,
                1,
                b"local draft",
            )
            .await
            .unwrap();
        store
            .close_registered_document(&binding, &node.node_id.to_string(), &opened.edit_session_id)
            .await
            .unwrap();
        store
            .queue_checkpoint(&binding, &node.node_id.to_string(), 1)
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session.clone());
        let runtime = Runtime(Arc::new(Mutex::new(state)));
        (
            directory,
            server,
            runtime,
            session,
            node.node_id.to_string(),
        )
    }

    #[tokio::test]
    async fn actual_server_bootstrap_publishes_index_and_preserves_draft_outbox() {
        let (_directory, server, runtime, session, document) = setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let result = bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let info = session.server_info().await.unwrap();
        assert_eq!(result, info.head_seq);
        let inventory = session.inventory(None).await.unwrap();
        let binding = registration_binding(session.binding());
        let mut state = runtime.0.lock().await;
        let (cursor, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        assert_eq!(cursor, info.head_seq);
        assert_eq!(nodes, inventory.nodes);
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
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn obsolete_renderer_cannot_publish_bootstrap() {
        let (_directory, server, runtime, session, document) = setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        runtime
            .0
            .lock()
            .await
            .gate
            .replace_owner("new-owner".into());
        assert_eq!(
            bootstrap_metadata(&runtime, &snapshot, &session).await,
            Err("stale_renderer_session".into())
        );
        let binding = registration_binding(session.binding());
        let mut state = runtime.0.lock().await;
        assert!(state.store.sync_index(&binding).await.unwrap().is_none());
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"local draft"
        );
        assert_eq!(
            state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    pub(crate) async fn delayed_inventory_proxy(
        target: std::net::SocketAddr,
        points: Vec<&'static str>,
        small_pages: bool,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::Receiver<()>,
        tokio::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        test_proxy(target, points, small_pages, None).await
    }

    pub(crate) async fn test_proxy(
        target: std::net::SocketAddr,
        points: Vec<&'static str>,
        small_pages: bool,
        mode: Option<Arc<std::sync::atomic::AtomicU8>>,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::Receiver<()>,
        tokio::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (observed_tx, observed_rx) = tokio::sync::mpsc::channel(1);
        let (release_tx, mut release_rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            let mut points = points.into_iter().peekable();
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let (header_end, total) = loop {
                    let count = client.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let header = std::str::from_utf8(&request[..end]).unwrap();
                        let length = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break (end, end + 4 + length);
                        }
                    }
                };
                let header = std::str::from_utf8(&request[..header_end]).unwrap();
                let should_pause = points
                    .peek()
                    .is_some_and(|point| header.starts_with(*point));
                let header = if small_pages && header.starts_with("GET /v1/nodes?") {
                    header.replace("limit=100", "limit=2")
                } else {
                    header.to_owned()
                };
                let mut forwarded = header
                    .lines()
                    .filter(|line| !line.to_ascii_lowercase().starts_with("connection:"))
                    .map(|line| format!("{line}\r\n"))
                    .collect::<String>()
                    .into_bytes();
                forwarded.extend_from_slice(b"Connection: close\r\n\r\n");
                forwarded.extend_from_slice(&request[header_end + 4..total]);
                let mut server = TcpStream::connect(target).await.unwrap();
                server.write_all(&forwarded).await.unwrap();
                let mut response = Vec::new();
                server.read_to_end(&mut response).await.unwrap();
                let selected = mode
                    .as_ref()
                    .map_or(0, |mode| mode.load(std::sync::atomic::Ordering::SeqCst));
                let is_changes = header.starts_with("GET /v1/changes?");
                if matches!(selected, 5 | 6) && header.starts_with("GET /v1/nodes/") {
                    let end = response
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .unwrap();
                    let headers = std::str::from_utf8(&response[..end]).unwrap();
                    let mut altered = Vec::new();
                    let mut replaced = false;
                    for line in headers.lines() {
                        if selected == 5 && line.to_ascii_lowercase().starts_with("content-digest:")
                        {
                            altered.extend_from_slice(b"content-digest: sha-256=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:\r\n");
                            replaced = true;
                        } else if selected == 6
                            && line.to_ascii_lowercase().starts_with("content-length:")
                        {
                            altered.extend_from_slice(b"content-length: 26214401\r\n");
                            replaced = true;
                        } else {
                            altered.extend_from_slice(line.as_bytes());
                            altered.extend_from_slice(b"\r\n");
                        }
                    }
                    assert!(replaced);
                    altered.extend_from_slice(b"\r\n");
                    altered.extend_from_slice(&response[end + 4..]);
                    response = altered;
                }
                if selected == 8 && header.starts_with("GET /v1/nodes?") {
                    let body = "{}";
                    response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
                } else if selected == 1 && is_changes {
                    let body = serde_json::json!({"code":"resync_required","message":"synthetic","request_id":uuid::Uuid::now_v7().to_string()}).to_string();
                    response = format!("HTTP/1.1 410 Gone\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
                } else if selected == 4 && is_changes {
                    let body = serde_json::json!({"code":"unavailable","message":"synthetic","request_id":uuid::Uuid::now_v7().to_string()}).to_string();
                    response = format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
                } else if (matches!(selected, 2 | 7) && header.starts_with("GET /v1/server-info"))
                    || (selected == 3 && is_changes)
                {
                    let end = response
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .unwrap()
                        + 4;
                    let mut value: serde_json::Value =
                        serde_json::from_slice(&response[end..]).unwrap();
                    if selected == 2 {
                        value["workspace_epoch"] =
                            serde_json::json!(uuid::Uuid::new_v4().to_string());
                    } else if selected == 7 {
                        value["root_id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
                    } else if let Some(first) =
                        value["nodes"].as_array().and_then(|nodes| nodes.first())
                    {
                        let seq = first["last_change_seq"].clone();
                        value["nodes"]
                            .as_array_mut()
                            .unwrap()
                            .retain(|node| node["last_change_seq"] == seq);
                        value["covered_through"] = seq.clone();
                        value["has_more"] = serde_json::json!(seq != value["through_seq"]);
                    }
                    let body = value.to_string();
                    response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
                }
                if should_pause {
                    points.next();
                    if observed_tx.send(()).await.is_err() || release_rx.recv().await.is_none() {
                        break;
                    }
                }
                client.write_all(&response).await.unwrap();
            }
        });
        (address, observed_rx, release_tx, task)
    }

    #[tokio::test]
    async fn bootstrap_replays_real_mutations_while_inventory_response_is_in_flight() {
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_protocol::PairResponse;
        use serde_json::json;
        use std::time::Duration;

        let (directory, server, runtime, original, document) = setup().await;
        let before = original.inventory(None).await.unwrap();
        let root = before
            .nodes
            .iter()
            .find(|node| node.parent_id.is_none())
            .unwrap();
        let moved = before
            .nodes
            .iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let deleted = before
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
        let (address, mut observed, release, proxy) =
            delayed_inventory_proxy(server.local_addr(), vec!["GET /v1/nodes?"], false).await;
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
        let writer_request = http::pairing_request(&config).await.0;
        let paired = http::post_json(
            server.local_addr(),
            &serde_json::to_vec(&writer_request).unwrap(),
        )
        .await;
        assert_eq!(paired.status, 200);
        let writer: PairResponse = serde_json::from_slice(&paired.body).unwrap();
        let old_binding = registration_binding(original.binding());
        let binding = registration_binding(session.binding());
        {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
        }
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cloned = Runtime(runtime.0.clone());
        let cloned_session = session.clone();
        let task =
            tokio::spawn(
                async move { bootstrap_metadata(&cloned, &snapshot, &cloned_session).await },
            );
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        let _state = tokio::time::timeout(Duration::from_secs(2), runtime.0.lock())
            .await
            .expect("network wait must not hold the runtime lock");
        drop(_state);
        let authorization = format!("Bearer {}", writer.device_token);
        let epoch = writer.workspace_epoch.to_string();
        let root_id = root.node_id.to_string();
        let created_id = uuid::Uuid::now_v7().to_string();
        for (sequence, method, path, body, etag) in [
            (
                1u64,
                "POST",
                "/v1/nodes".to_owned(),
                Some(
                    json!({"node_id":created_id,"parent_id":root_id,"name":"new.md","kind":"file"}),
                ),
                None,
            ),
            (
                2,
                "PATCH",
                format!("/v1/nodes/{}/metadata", moved.node_id),
                Some(json!({"expected_parent_id":root_id,"parent_id":root_id,"name":"renamed.md"})),
                Some(moved.meta_etag.as_str()),
            ),
            (
                3,
                "DELETE",
                format!("/v1/nodes/{}", deleted.node_id),
                None,
                Some(deleted.node_etag.as_str()),
            ),
        ] {
            let sequence = sequence.to_string();
            let operation = uuid::Uuid::now_v7().to_string();
            let mut headers = vec![
                ("Authorization", authorization.as_str()),
                ("X-Workspace-Epoch", epoch.as_str()),
                ("X-Device-Stream", writer.device_stream_id.as_str()),
                ("X-Operation-Seq", sequence.as_str()),
                ("Idempotency-Key", operation.as_str()),
            ];
            if let Some(value) = etag {
                headers.push(("If-Match", value));
            }
            let body = body
                .map(|body| serde_json::to_vec(&body).unwrap())
                .unwrap_or_default();
            if !body.is_empty() {
                headers.push(("Content-Type", "application/json"));
            }
            let response = http::request(server.local_addr(), method, &path, &headers, &body).await;
            assert_eq!(
                response.status,
                200,
                "mutation {method} failed: {}",
                String::from_utf8_lossy(&response.body)
            );
        }
        release.send(()).await.unwrap();
        let cursor = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let info = session.server_info().await.unwrap();
        assert_eq!(cursor, info.head_seq);
        let live = session.inventory(None).await.unwrap().nodes;
        let mut state = runtime.0.lock().await;
        let (stored_cursor, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        assert_eq!(stored_cursor, cursor);
        assert_eq!(
            nodes
                .iter()
                .filter(|node| !node.deleted)
                .cloned()
                .collect::<Vec<_>>(),
            live
        );
        assert!(
            nodes
                .iter()
                .any(|node| node.node_id.to_string() == document && node.deleted)
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&old_binding, &document)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"local draft"
        );
        assert_eq!(
            state
                .store
                .list_outbox(&old_binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        drop(state);
        proxy.abort();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn re_registration_during_network_wait_rejects_old_response_and_preserves_outbox() {
        use crate::sync_bootstrap::server_test_support as http;
        use std::time::Duration;

        let (directory, server, runtime, original, document) = setup().await;
        let stale_document = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .iter()
            .find(|node| node.name == "move.md")
            .unwrap()
            .node_id
            .to_string();
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
            delayed_inventory_proxy(server.local_addr(), vec!["GET /v1/nodes?"], false).await;
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
        let original_binding = registration_binding(original.binding());
        let stale_binding = registration_binding(session.binding());
        let queued = {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&stale_binding)
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
            let open = state
                .store
                .open_registered_document(
                    &stale_binding,
                    &stale_document,
                    b"stale base",
                    "stale-revision",
                )
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &stale_binding,
                    &stale_document,
                    &open.edit_session_id,
                    1,
                    b"stale local draft",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&stale_binding, &stale_document, &open.edit_session_id)
                .await
                .unwrap();
            state
                .store
                .queue_checkpoint(&stale_binding, &stale_document, 1)
                .await
                .unwrap()
        };
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cloned = Runtime(runtime.0.clone());
        let task =
            tokio::spawn(async move { bootstrap_metadata(&cloned, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();

        let replacement_pairing = http::pairing_request(&config).await.0;
        let replacement = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                replacement_pairing.workspace_id,
                &replacement_pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let replacement_binding = registration_binding(replacement.binding());
        {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&replacement_binding)
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(replacement);
        }
        release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_registration".into())
        );
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .store
                .sync_index(&replacement_binding)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&original_binding, &document)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"local draft"
        );
        assert_eq!(
            state
                .store
                .list_outbox(&original_binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        let stale_draft = state
            .store
            .read_registered_checkpoint(&stale_binding, &stale_document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stale_draft.bytes, b"stale local draft");
        assert_eq!(stale_draft.base_bytes, b"stale base");
        assert_eq!(stale_draft.base_revision, "stale-revision");
        let mut expected = queued.clone();
        expected.frozen = true;
        assert_eq!(
            state
                .store
                .read_outbox_operation(&stale_binding, &queued.operation_id)
                .await
                .unwrap(),
            Some(expected)
        );
        drop(state);
        proxy.abort();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalidated_inventory_error_is_not_reported_as_current_contract_error() {
        use crate::send::TransmissionBlock;
        use crate::sync_bootstrap::server_test_support as http;
        use std::sync::atomic::AtomicU8;
        use std::time::Duration;

        let (directory, server, runtime, _original, _document) = setup().await;
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
        let mode = Arc::new(AtomicU8::new(8));
        let (address, mut observed, release, proxy) = test_proxy(
            server.local_addr(),
            vec!["GET /v1/nodes?"],
            false,
            Some(mode),
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
        {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
        }
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cloned = Runtime(runtime.0.clone());
        let task =
            tokio::spawn(async move { bootstrap_metadata(&cloned, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = runtime.0.lock().await;
            state
                .transmission_gate
                .block(&binding, TransmissionBlock::ProtocolMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
        }
        release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_sync".into())
        );
        proxy.abort();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalidated_inventory_response_cannot_publish_bootstrap() {
        use crate::send::TransmissionBlock;
        use crate::sync_bootstrap::server_test_support as http;
        use std::time::Duration;

        let (directory, server, runtime, original, document) = setup().await;
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
            delayed_inventory_proxy(server.local_addr(), vec!["GET /v1/nodes?"], false).await;
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
        let original_binding = registration_binding(original.binding());
        let binding = registration_binding(session.binding());
        {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
            state
                .transmission_gate
                .begin_initial_sync(&binding)
                .unwrap();
        }
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cloned = Runtime(runtime.0.clone());
        let task =
            tokio::spawn(async move { bootstrap_metadata(&cloned, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let mut state = runtime.0.lock().await;
            state
                .transmission_gate
                .block(&binding, TransmissionBlock::ProtocolMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&binding, TransmissionBlock::ProtocolMismatch)
            );
        }
        release.send(()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap();
        let mut state = runtime.0.lock().await;
        assert!(state.store.sync_index(&binding).await.unwrap().is_none());
        assert!(result.is_err());
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&original_binding, &document)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"local draft"
        );
        drop(state);
        proxy.abort();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn renderer_reload_during_inventory_response_rejects_the_old_request() {
        use crate::sync_bootstrap::server_test_support as http;
        use std::time::Duration;

        let (directory, server, runtime, original, document) = setup().await;
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
            delayed_inventory_proxy(server.local_addr(), vec!["GET /v1/nodes?"], false).await;
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
        let original_binding = registration_binding(original.binding());
        let binding = registration_binding(session.binding());
        {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
        }
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cloned = Runtime(runtime.0.clone());
        let task =
            tokio::spawn(async move { bootstrap_metadata(&cloned, &snapshot, &session).await });
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        runtime
            .0
            .lock()
            .await
            .gate
            .replace_owner("new-owner".into());
        release.send(()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap()
                .unwrap(),
            Err("stale_renderer_session".into())
        );
        let mut state = runtime.0.lock().await;
        assert!(state.store.sync_index(&binding).await.unwrap().is_none());
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&original_binding, &document)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"local draft"
        );
        assert_eq!(
            state
                .store
                .list_outbox(&original_binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        drop(state);
        proxy.abort();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bootstrap_recovers_preceding_keyset_id_and_excludes_changes_after_h1() {
        use crate::sync_bootstrap::server_test_support as http;
        use notryx_protocol::PairResponse;
        use serde_json::json;
        use std::time::Duration;

        let (directory, server, runtime, original, _document) = setup().await;
        let first_page = original.inventory(None).await.unwrap();
        assert_eq!(first_page.nodes.len(), 3);
        let root = first_page
            .nodes
            .iter()
            .find(|node| node.parent_id.is_none())
            .unwrap()
            .node_id
            .to_string();
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
        let (address, mut observed, release, proxy) = delayed_inventory_proxy(
            server.local_addr(),
            vec!["GET /v1/nodes?", "GET /v1/changes?"],
            true,
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
        let writer_request = http::pairing_request(&config).await.0;
        let paired = http::post_json(
            server.local_addr(),
            &serde_json::to_vec(&writer_request).unwrap(),
        )
        .await;
        let writer: PairResponse = serde_json::from_slice(&paired.body).unwrap();
        let binding = registration_binding(session.binding());
        {
            let mut state = runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(session.clone());
        }
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        let cloned = Runtime(runtime.0.clone());
        let cloned_session = session.clone();
        let task =
            tokio::spawn(
                async move { bootstrap_metadata(&cloned, &snapshot, &cloned_session).await },
            );
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();

        let earlier_id = "00000000-0000-7000-8000-000000000001";
        assert!(earlier_id < first_page.nodes[1].node_id.to_string().as_str());
        let later_id = uuid::Uuid::now_v7().to_string();
        let authorization = format!("Bearer {}", writer.device_token);
        let epoch = writer.workspace_epoch.to_string();
        async fn create(
            address: std::net::SocketAddr,
            writer: &PairResponse,
            authorization: &str,
            epoch: &str,
            root: &str,
            id: &str,
            seq: u64,
        ) {
            let sequence = seq.to_string();
            let operation = uuid::Uuid::now_v7().to_string();
            let body = serde_json::to_vec(&json!({"node_id":id,"parent_id":root,"name":format!("new-{seq}.md"),"kind":"file"})).unwrap();
            let response = http::request(
                address,
                "POST",
                "/v1/nodes",
                &[
                    ("Authorization", authorization),
                    ("X-Workspace-Epoch", epoch),
                    ("X-Device-Stream", writer.device_stream_id.as_str()),
                    ("X-Operation-Seq", sequence.as_str()),
                    ("Idempotency-Key", operation.as_str()),
                    ("Content-Type", "application/json"),
                ],
                &body,
            )
            .await;
            assert_eq!(
                response.status,
                200,
                "{}",
                String::from_utf8_lossy(&response.body)
            );
        }
        create(
            server.local_addr(),
            &writer,
            &authorization,
            &epoch,
            &root,
            earlier_id,
            1,
        )
        .await;
        release.send(()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        let fixed_head = original.server_info().await.unwrap().head_seq;
        create(
            server.local_addr(),
            &writer,
            &authorization,
            &epoch,
            &root,
            &later_id,
            2,
        )
        .await;
        release.send(()).await.unwrap();
        let cursor = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(cursor, fixed_head);
        let latest = session.server_info().await.unwrap().head_seq;
        assert!(latest > cursor);
        let mut state = runtime.0.lock().await;
        let (stored, nodes) = state.store.sync_index(&binding).await.unwrap().unwrap();
        assert_eq!(stored, cursor);
        assert!(
            nodes
                .iter()
                .any(|node| node.node_id.to_string() == earlier_id)
        );
        assert!(
            !nodes
                .iter()
                .any(|node| node.node_id.to_string() == later_id)
        );
        drop(state);
        let next = session.changes(cursor, Some(latest)).await.unwrap();
        assert!(
            next.nodes
                .iter()
                .any(|node| node.node_id.to_string() == later_id)
        );
        proxy.abort();
        server.shutdown().await.unwrap();
    }
}
