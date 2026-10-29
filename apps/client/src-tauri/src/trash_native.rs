use crate::{RuntimeState, registration_binding, send::TransmissionBlock, to_contract};
use notryx_domain::NodeId;
use notryx_protocol::TrashPage;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn trash_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    after: Option<NodeId>,
    limit: u16,
    #[cfg(test)] after_response: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<TrashPage, String> {
    let (snapshot, session, binding, generation) = {
        let state = runtime.lock().await;
        let snapshot = state.gate.snapshot(owner).map_err(str::to_owned)?;
        let session = state.session.clone().ok_or("not_connected")?;
        let binding = registration_binding(session.binding());
        let generation = state.transmission_gate.response_generation(&binding);
        (snapshot, session, binding, generation)
    };
    let page = session.trash(after, limit).await;
    #[cfg(test)]
    if let Some((ready, resume)) = after_response {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let mut state = runtime.lock().await;
    state.gate.validate(&snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &session))
    {
        return Err("stale_registration".into());
    }
    if !state
        .transmission_gate
        .response_trusted(&binding, generation)
    {
        return Err("stale_sync".into());
    }
    let reason = match &page {
        Err(notryx_client_runtime::Error::Contract | notryx_client_runtime::Error::TooLarge) => {
            Some(TransmissionBlock::ProtocolMismatch)
        }
        Err(notryx_client_runtime::Error::EpochMismatch) => Some(TransmissionBlock::EpochMismatch),
        Err(notryx_client_runtime::Error::Binding) => Some(TransmissionBlock::BindingMismatch),
        _ => None,
    };
    if let Some(reason) = reason {
        state.transmission_gate.block(&binding, reason);
        state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
            binding: to_contract(binding),
            status: notryx_protocol::WorkspaceSyncStatus::NeedsReview,
        });
    }
    page.map_err(|error| format!("{error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RuntimeState, registration_binding};
    use notryx_client_runtime::Session;
    use notryx_client_store::ClientStore;
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceRole};
    use notryx_server::{
        DeleteNode, NodeSnapshot, Outcome, Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    use tokio::sync::Mutex;

    struct Rig {
        _directory: tempfile::TempDir,
        server: RunningServer,
        runtime: Arc<Mutex<RuntimeState>>,
        replacement: Arc<Session>,
        nodes: Vec<notryx_domain::NodeId>,
    }

    async fn rig() -> Rig {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
        let stream = workspace.register_stream().await.unwrap();
        let mut nodes = Vec::new();
        for (i, name) in ["one.md", "two.md"].into_iter().enumerate() {
            std::fs::write(notes.join(name), b"synthetic\n").unwrap();
            let document = workspace.adopt_root_document(name).await.unwrap();
            let node = NodeSnapshot::from(&document);
            let operation_id = if i == 0 {
                "019a1000-0000-7000-8000-000000000003"
            } else {
                "019a1000-0000-7000-8000-000000000004"
            };
            let result = workspace
                .delete_node(DeleteNode {
                    epoch: workspace.epoch(),
                    stream_id: stream,
                    operation_seq: i as u64 + 1,
                    operation_id: operation_id.parse().unwrap(),
                    node_id: node.node_id,
                    expected_node_etag: node.etag().unwrap(),
                })
                .await
                .unwrap();
            assert!(matches!(result.outcome, Outcome::Deleted(_)));
            nodes.push(node.node_id);
        }
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes,
                state_root: state,
                uploads: Default::default(),
            },
        };
        let server = RunningServer::start(config.clone()).await.unwrap();
        let AdminResponse::Ok {
            result: AdminResult::Status { workspace_id, .. },
        } = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
            .await
            .unwrap()
        else {
            panic!("status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Principal { principal_id },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::CreatePrincipal {
                role: WorkspaceRole::Reader,
            },
        )
        .await
        .unwrap()
        else {
            panic!("reader")
        };
        let origin = format!("http://{}", server.local_addr());
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let AdminResponse::Ok {
                result: AdminResult::Pairing { pairing_secret, .. },
            } = admin::request(
                &config.server.admin_socket,
                &AdminRequest::IssuePairing {
                    principal_id: principal_id.clone(),
                },
            )
            .await
            .unwrap()
            else {
                panic!("pairing")
            };
            sessions.push(Arc::new(
                Session::pair(&origin, workspace_id, &pairing_secret)
                    .await
                    .unwrap(),
            ));
        }
        let replacement = sessions.pop().unwrap();
        let session = sessions.pop().unwrap();
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        Rig {
            _directory: directory,
            server,
            runtime: Arc::new(Mutex::new(state)),
            replacement,
            nodes,
        }
    }

    #[tokio::test]
    async fn reader_can_page_deleted_items_without_a_client_draft() {
        let rig = rig().await;
        let first = trash_for_renderer(&rig.runtime, "owner", None, 1, None)
            .await
            .unwrap();
        assert_eq!(first.nodes.len(), 1);
        assert!(first.nodes[0].restorable);
        let after = first.next_after_id.unwrap();
        let second = trash_for_renderer(&rig.runtime, "owner", Some(after), 1, None)
            .await
            .unwrap();
        assert_eq!(second.nodes.len(), 1);
        assert!(second.next_after_id.is_none());
        assert!(rig.nodes.contains(&first.nodes[0].node_id));
        assert!(rig.nodes.contains(&second.nodes[0].node_id));
        assert_ne!(first.nodes[0].node_id, second.nodes[0].node_id);
        assert!(
            trash_for_renderer(&rig.runtime, "owner", None, 0, None)
                .await
                .is_err()
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delayed_trash_response_cannot_cross_renderer_replacement() {
        let rig = rig().await;
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let runtime = rig.runtime.clone();
        let task = tokio::spawn(async move {
            trash_for_renderer(&runtime, "owner", None, 10, Some((ready, resumed))).await
        });
        observed.await.unwrap();
        rig.runtime
            .lock()
            .await
            .gate
            .replace_owner("replacement".into());
        resume.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ref error) if error == "stale_renderer_session"));
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delayed_trash_response_cannot_cross_reregistration() {
        let rig = rig().await;
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let runtime = rig.runtime.clone();
        let task = tokio::spawn(async move {
            trash_for_renderer(&runtime, "owner", None, 10, Some((ready, resumed))).await
        });
        observed.await.unwrap();
        {
            let mut state = rig.runtime.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(rig.replacement.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(rig.replacement.clone());
        }
        resume.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ref error) if error == "stale_registration"));
        rig.server.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn wrong_epoch_trash_page_is_not_published() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let workspace = uuid::Uuid::new_v4().to_string();
        let epoch = uuid::Uuid::new_v4().to_string();
        let pair = serde_json::json!({"protocol":"mdw-sync/1.2", "workspace_id":workspace, "workspace_epoch":epoch, "principal_id":uuid::Uuid::new_v4().to_string(), "device_id":uuid::Uuid::new_v4().to_string(), "device_stream_id":uuid::Uuid::new_v4().to_string(), "device_token":"synthetic-token", "role":"reader", "idle_expires_at":"1", "absolute_expires_at":"2"}).to_string();
        let page = serde_json::json!({"workspace_epoch":uuid::Uuid::new_v4().to_string(), "nodes":[], "next_after_id":null}).to_string();
        let fake = tokio::spawn(async move {
            for body in [pair, page] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let session = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                workspace.parse().unwrap(),
                "synthetic-pairing",
            )
            .await
            .unwrap(),
        );
        let directory = tempfile::tempdir().unwrap();
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        let runtime = Arc::new(Mutex::new(state));
        assert!(
            matches!(trash_for_renderer(&runtime, "owner", None, 10, None).await, Err(ref error) if error == "Binding")
        );
        assert!(
            runtime
                .lock()
                .await
                .transmission_gate
                .has_block(&binding, TransmissionBlock::BindingMismatch)
        );
        fake.await.unwrap();
    }
}
