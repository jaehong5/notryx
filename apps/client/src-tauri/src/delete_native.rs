use crate::{
    Runtime, RuntimeState, check_caller, from_contract, registration_binding, to_contract,
};
use notryx_client_runtime::{DeleteMutation, OperationLookup};
use notryx_client_store::{
    DeleteOutboxOperation, DeleteOutboxPage, OutboxStatus, PendingOutboxOperation,
    RegistrationBinding,
};
use notryx_domain::NodeId;
use notryx_protocol::{
    OperationReceipt, WorkspaceDeleteAbandonRequest, WorkspaceDeleteEntry,
    WorkspaceDeleteListRequest, WorkspaceDeletePage, WorkspaceDeleteQueueRequest,
    WorkspaceDeleteStatus,
};
use std::sync::Arc;
use tokio::sync::Mutex;

async fn validate_local_binding(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
) -> Result<(), String> {
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(binding)
        || state
            .session
            .as_ref()
            .is_some_and(|session| registration_binding(session.binding()) != *binding)
    {
        return Err("stale_registration".into());
    }
    Ok(())
}

pub(crate) async fn queue_delete_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    node_id: NodeId,
    expected_node_etag: &str,
) -> Result<DeleteOutboxOperation, String> {
    let mut state = runtime.lock().await;
    state.gate.check_queue(owner).map_err(str::to_owned)?;
    validate_local_binding(&mut state, binding).await?;
    state
        .store
        .queue_delete(binding, node_id, expected_node_etag)
        .await
        .map_err(|error| error.to_string())
}

fn entry(
    operation: DeleteOutboxOperation,
    receipt: Option<OperationReceipt>,
) -> Result<WorkspaceDeleteEntry, String> {
    let status = match operation.status {
        OutboxStatus::Queued => WorkspaceDeleteStatus::Queued,
        OutboxStatus::Sent => WorkspaceDeleteStatus::Sent,
        OutboxStatus::Abandoning => WorkspaceDeleteStatus::Abandoning,
        OutboxStatus::Abandoned => WorkspaceDeleteStatus::Abandoned,
        OutboxStatus::Committed => WorkspaceDeleteStatus::Committed,
        OutboxStatus::Failed => WorkspaceDeleteStatus::Failed,
        _ => return Err("UnsupportedFormat".into()),
    };
    Ok(WorkspaceDeleteEntry {
        operation_id: operation.operation_id,
        operation_seq: operation.operation_seq.to_string(),
        node_id: operation.node_id,
        expected_node_etag: operation.expected_node_etag,
        status,
        receipt,
    })
}

pub(crate) async fn delete_status_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: u64,
    limit: u16,
) -> Result<DeleteOutboxPage, String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    validate_local_binding(&mut state, binding).await?;
    state
        .store
        .list_delete_operations(binding, after, limit)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn workspace_queue_delete(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDeleteQueueRequest,
) -> Result<WorkspaceDeleteEntry, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let operation = queue_delete_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        request.node_id,
        &request.expected_node_etag,
    )
    .await?;
    entry(operation, None)
}

#[tauri::command]
pub(crate) async fn workspace_delete_status(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDeleteListRequest,
) -> Result<WorkspaceDeletePage, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let after = request
        .after_seq
        .parse::<u64>()
        .map_err(|_| "invalid_sequence")?;
    let page = delete_status_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        after,
        request.limit,
    )
    .await?;
    Ok(WorkspaceDeletePage {
        binding: to_contract(binding),
        entries: page
            .entries
            .into_iter()
            .map(|item| entry(item.operation, item.receipt))
            .collect::<Result<Vec<_>, _>>()?,
        next_after_seq: page.next_after_seq.map(|v| v.to_string()),
    })
}

#[tauri::command]
pub(crate) async fn workspace_request_delete_abandon(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceDeleteAbandonRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    request_delete_abandon_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        &request.operation_id,
        #[cfg(test)]
        None,
    )
    .await
}

pub(super) async fn request_delete_abandon_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    operation_id: &str,
    #[cfg(test)] after_lookup: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<(), String> {
    let parsed: notryx_domain::OperationId =
        operation_id.parse().map_err(|_| "invalid_operation")?;
    if parsed.to_string() != operation_id {
        return Err("invalid_operation".into());
    }
    let (snapshot, session, generation, item) = {
        let mut state = runtime.lock().await;
        state.gate.check_queue(owner).map_err(str::to_owned)?;
        let snapshot = state.gate.snapshot(owner).map_err(str::to_owned)?;
        if state.session.is_none() {
            validate_local_binding(&mut state, binding).await?;
            let Some(PendingOutboxOperation::Delete(item)) = state
                .store
                .next_pending_outbox(binding)
                .await
                .map_err(|error| error.to_string())?
            else {
                return Err("not_fifo_head".into());
            };
            if item.operation_id != operation_id {
                return Err("not_fifo_head".into());
            }
            if item.status != OutboxStatus::Queued {
                return Err("not_connected".into());
            }
            if !state
                .store
                .request_delete_abandon_matching(binding, &item)
                .await
                .map_err(|error| error.to_string())?
            {
                return Err("stale_operation".into());
            }
            return Ok(());
        }
        let session = state.session.clone().ok_or("not_connected")?;
        if registration_binding(session.binding()) != *binding {
            return Err("stale_registration".into());
        }
        let generation = state.transmission_gate.response_generation(binding);
        if !state
            .transmission_gate
            .response_trusted(binding, generation)
        {
            return Err("stale_sync".into());
        }
        let Some(PendingOutboxOperation::Delete(item)) = state
            .store
            .next_pending_outbox(binding)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Err("not_fifo_head".into());
        };
        if item.operation_id != operation_id {
            return Err("not_fifo_head".into());
        }
        (snapshot, session, generation, item)
    };
    let mutation = DeleteMutation {
        binding: session.binding().clone(),
        operation_id: parsed,
        operation_seq: item.operation_seq,
        node_id: item.node_id,
        expected_node_etag: item.expected_node_etag.clone(),
    };
    let lookup = session.delete_operation(&mutation).await;
    #[cfg(test)]
    if let Some((ready, resume)) = after_lookup {
        let _ = ready.send(());
        let _ = resume.await;
    }
    let mut state = runtime.lock().await;
    let request_error = state
        .gate
        .validate(&snapshot)
        .err()
        .or_else(|| state.gate.check_queue(owner).err());
    let same_session = state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &session));
    let trusted = state
        .transmission_gate
        .response_trusted(binding, generation);
    if same_session && (request_error.is_some() || !trusted) {
        let requires_review = match &lookup {
            Ok(OperationLookup::Retired) => true,
            Err(
                notryx_client_runtime::Error::Network
                | notryx_client_runtime::Error::Unavailable
                | notryx_client_runtime::Error::Unauthorized
                | notryx_client_runtime::Error::GatewayAuthRequired,
            ) => false,
            Err(_) => true,
            _ => false,
        };
        if requires_review {
            state
                .transmission_gate
                .block(binding, crate::send::TransmissionBlock::UnverifiedResponse);
            state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
                binding: to_contract(binding.clone()),
                status: notryx_protocol::WorkspaceSyncStatus::NeedsReview,
            });
        }
    }
    if let Some(error) = request_error {
        return Err(error.into());
    }
    if !same_session {
        return Err("stale_registration".into());
    }
    if !trusted {
        return Err("stale_sync".into());
    }
    let lookup = match lookup {
        Ok(lookup) => lookup,
        Err(error) => {
            if matches!(
                error,
                notryx_client_runtime::Error::Network
                    | notryx_client_runtime::Error::Unavailable
                    | notryx_client_runtime::Error::Unauthorized
                    | notryx_client_runtime::Error::GatewayAuthRequired
            ) && item.status == OutboxStatus::Queued
            {
                if !state
                    .store
                    .request_delete_abandon_matching(binding, &item)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    return Err("stale_operation".into());
                }
                return Ok(());
            }
            let reason = match error {
                notryx_client_runtime::Error::Contract | notryx_client_runtime::Error::TooLarge => {
                    Some(crate::send::TransmissionBlock::ProtocolMismatch)
                }
                notryx_client_runtime::Error::EpochMismatch => {
                    Some(crate::send::TransmissionBlock::EpochMismatch)
                }
                notryx_client_runtime::Error::Binding => {
                    Some(crate::send::TransmissionBlock::BindingMismatch)
                }
                _ => None,
            };
            if let Some(reason) = reason {
                state.transmission_gate.block(binding, reason);
                state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
                    binding: to_contract(binding.clone()),
                    status: notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                });
            }
            return Err(format!("{error:?}"));
        }
    };
    match lookup {
        OperationLookup::Missing => {
            if !state
                .store
                .request_delete_abandon_matching(binding, &item)
                .await
                .map_err(|error| error.to_string())?
            {
                return Err("stale_operation".into());
            }
            Ok(())
        }
        OperationLookup::Receipt(_) => Err("operation_has_result".into()),
        OperationLookup::Retired => {
            state
                .transmission_gate
                .block(binding, crate::send::TransmissionBlock::UnverifiedResponse);
            state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
                binding: to_contract(binding.clone()),
                status: notryx_protocol::WorkspaceSyncStatus::NeedsReview,
            });
            Err("operation_retired".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{registration_binding, sync_bootstrap};
    use notryx_client_runtime::Session;
    use notryx_client_store::{ClientStore, OutboxAbandonState};
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceSendStatus};
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn queue_delete_uses_current_binding_and_exact_confirmed_node_etag() {
        let (_directory, server, runtime, session, other) = sync_bootstrap::tests::setup().await;
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
            queue_delete_for_renderer(
                &runtime.0,
                "wrong-owner",
                &binding,
                node.node_id,
                &node.node_etag
            )
            .await
            .is_err()
        );
        assert!(
            queue_delete_for_renderer(&runtime.0, "owner", &binding, node.node_id, "\"stale\"")
                .await
                .is_err()
        );
        let item =
            queue_delete_for_renderer(&runtime.0, "owner", &binding, node.node_id, &node.node_etag)
                .await
                .unwrap();
        assert_eq!(item.expected_node_etag, node.node_etag);
        assert!(
            delete_status_for_renderer(&runtime.0, "wrong-owner", &binding, 0, 10)
                .await
                .is_err()
        );
        assert!(
            delete_status_for_renderer(&runtime.0, "owner", &binding, 0, 0)
                .await
                .is_err()
        );
        let page = delete_status_for_renderer(&runtime.0, "owner", &binding, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation.operation_id, item.operation_id);
        let mut locked = runtime.0.lock().await;
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &other)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert_eq!(item.operation_seq, 2);
        drop(locked);
        server.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn reader_cancel_checks_actual_server_then_abandons_without_deleting() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
                state_root: server_state,
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
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Principal { principal_id },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::CreatePrincipal {
                role: notryx_protocol::WorkspaceRole::Reader,
            },
        )
        .await
        .unwrap()
        else {
            panic!("reader");
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing { principal_id },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
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
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        assert_eq!(
            crate::send::send_one(state.clone(), "owner")
                .await
                .unwrap()
                .status,
            WorkspaceSendStatus::PermissionDenied
        );
        assert!(
            request_delete_abandon_for_renderer(
                &state,
                "owner",
                &binding,
                &item.operation_id,
                None
            )
            .await
            .is_ok()
        );
        {
            let mut locked = state.lock().await;
            assert!(
                matches!(locked.store.next_pending_outbox(&binding).await.unwrap(), Some(PendingOutboxOperation::Delete(ref pending)) if pending.abandon_state == OutboxAbandonState::Requested)
            );
        }
        assert_eq!(
            crate::send::send_one(state.clone(), "owner")
                .await
                .unwrap()
                .status,
            WorkspaceSendStatus::Abandoned
        );
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_detects_existing_delete_receipt_without_replacing_intent() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
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
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
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
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        {
            let mut locked = state.lock().await;
            locked.store.prepare_pending_outbox(&binding).await.unwrap();
        }
        let request = DeleteMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            expected_node_etag: item.expected_node_etag.clone(),
        };
        assert!(matches!(
            session.delete(&request).await.unwrap(),
            OperationReceipt::Deleted { .. }
        ));
        assert!(
            matches!(request_delete_abandon_for_renderer(&state, "owner", &binding, &item.operation_id, None).await, Err(ref error) if error == "operation_has_result")
        );
        let mut locked = state.lock().await;
        assert!(
            matches!(locked.store.next_pending_outbox(&binding).await.unwrap(), Some(PendingOutboxOperation::Delete(ref pending)) if pending.abandon_state == OutboxAbandonState::None && pending.expected_node_etag == item.expected_node_etag)
        );
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
        drop(locked);
        assert!(!notes.join("Example.md").exists());
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_racing_with_committed_delete_applies_original_receipt() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
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
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
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
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        {
            let mut locked = state.lock().await;
            locked.store.prepare_pending_outbox(&binding).await.unwrap();
        }
        let (ready, observed) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let task_state = state.clone();
        let task_binding = binding.clone();
        let operation_id = item.operation_id.clone();
        let task = tokio::spawn(async move {
            request_delete_abandon_for_renderer(
                &task_state,
                "owner",
                &task_binding,
                &operation_id,
                Some((ready, resumed)),
            )
            .await
        });
        observed.await.unwrap();
        let request = DeleteMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            expected_node_etag: item.expected_node_etag.clone(),
        };
        assert!(matches!(
            session.delete(&request).await.unwrap(),
            OperationReceipt::Deleted { .. }
        ));
        resume.send(()).unwrap();
        task.await.unwrap().unwrap();
        let result = crate::send::send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        let page = locked
            .store
            .list_delete_operations(&binding, 0, 10)
            .await
            .unwrap();
        assert!(matches!(
            page.entries[0].receipt,
            Some(OperationReceipt::Deleted { .. })
        ));
        assert_eq!(page.entries[0].operation.status, OutboxStatus::Committed);
        drop(locked);
        assert!(!notes.join("Example.md").exists());
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_retired_delete_blocks_only_current_registration() {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let server_state = directory.path().join("server-state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&server_state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &server_state).await.unwrap();
        std::fs::write(notes.join("Example.md"), b"base\n").unwrap();
        let document = workspace.adopt_root_document("Example.md").await.unwrap();
        workspace.close().await.unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: notes.clone(),
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
            panic!("admin status")
        };
        let AdminResponse::Ok {
            result: AdminResult::Pairing { pairing_secret, .. },
        } = admin::request(
            &config.server.admin_socket,
            &AdminRequest::IssuePairing {
                principal_id: owner_principal_id.clone(),
            },
        )
        .await
        .unwrap()
        else {
            panic!("pairing grant")
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
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let db_path = directory.path().join("client.sqlite3");
        let mut store = ClientStore::open(&db_path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)")
            .bind(node.node_id.to_string()).bind(serde_json::to_string(&node).unwrap()).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&db_path).await.unwrap();
        let item = store
            .queue_delete(&binding, node.node_id, &node.node_etag)
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session.clone());
        let state = Arc::new(Mutex::new(runtime));
        {
            let mut locked = state.lock().await;
            locked.sync_state = Some(notryx_protocol::WorkspaceSyncState {
                binding: to_contract(binding.clone()),
                status: notryx_protocol::WorkspaceSyncStatus::Ready,
            });
            locked.store.prepare_pending_outbox(&binding).await.unwrap();
        }
        let request = DeleteMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            expected_node_etag: item.expected_node_etag.clone(),
        };
        assert!(matches!(
            session.delete(&request).await.unwrap(),
            OperationReceipt::Deleted { .. }
        ));
        session.acknowledge(1).await.unwrap();
        assert!(
            matches!(request_delete_abandon_for_renderer(&state, "owner", &binding, &item.operation_id, None).await, Err(ref error) if error == "operation_retired")
        );
        let runtime = Runtime(state.clone());
        assert_eq!(
            crate::sync_lifecycle::sync_state_for_renderer(&runtime, "owner")
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSyncStatus::NeedsReview
        );
        let mut locked = state.lock().await;
        assert!(
            locked
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::UnverifiedResponse)
        );
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
        assert!(
            matches!(locked.store.next_pending_outbox(&binding).await.unwrap(), Some(PendingOutboxOperation::Delete(ref pending)) if pending.abandon_state == OutboxAbandonState::None)
        );
        drop(locked);
        assert!(!notes.join("Example.md").exists());
        server.shutdown().await.unwrap();
    }
}
