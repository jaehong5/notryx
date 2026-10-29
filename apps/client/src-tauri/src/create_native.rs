use crate::{
    Runtime, RuntimeState, check_caller, from_contract, registration_binding, to_contract,
};
use notryx_client_runtime::{CreateMutation, OperationLookup};
use notryx_client_store::{
    CreateOutboxOperation, CreateOutboxPage, OutboxStatus, PendingOutboxOperation,
    RegistrationBinding,
};
use notryx_domain::NodeId;
use notryx_protocol::{
    CreateNodeRequest, OperationReceipt, WorkspaceCreateAbandonRequest, WorkspaceCreateEntry,
    WorkspaceCreateListRequest, WorkspaceCreatePage, WorkspaceCreateQueueRequest,
    WorkspaceCreateStatus,
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn create_parents_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: Option<NodeId>,
) -> Result<notryx_protocol::InventoryPage, String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    validate_local_binding(&mut state, binding).await?;
    let (_, nodes) = state
        .store
        .sync_index(binding)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("not_connected")?;
    let after = after.map(|id| id.to_string());
    let mut nodes = nodes
        .into_iter()
        .filter(|node| {
            !node.deleted
                && node.kind == notryx_protocol::NodeKind::Folder
                && after
                    .as_ref()
                    .is_none_or(|after| node.node_id.to_string() > *after)
        })
        .take(101)
        .collect::<Vec<_>>();
    let next_after_id = if nodes.len() > 100 {
        nodes.pop();
        nodes.last().map(|node| node.node_id)
    } else {
        None
    };
    Ok(notryx_protocol::InventoryPage {
        workspace_epoch: binding
            .workspace_epoch
            .parse()
            .map_err(|_| "invalid_registration")?,
        nodes,
        next_after_id,
    })
}

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

#[tauri::command]
pub(crate) async fn workspace_create_parents(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: notryx_protocol::WorkspaceCreateParentsRequest,
) -> Result<notryx_protocol::WorkspaceCreateParentsPage, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let after = request
        .after_id
        .as_deref()
        .map(str::parse::<NodeId>)
        .transpose()
        .map_err(|_| "invalid_node")?;
    let page =
        create_parents_for_renderer(&runtime.0, &request.renderer_session_id, &binding, after)
            .await?;
    check_caller(&webview)?;
    Ok(notryx_protocol::WorkspaceCreateParentsPage {
        binding: to_contract(binding),
        workspace_epoch: page.workspace_epoch,
        nodes: page.nodes,
        next_after_id: page.next_after_id,
    })
}

fn entry(
    operation: CreateOutboxOperation,
    receipt: Option<OperationReceipt>,
) -> Result<WorkspaceCreateEntry, String> {
    let status = match operation.status {
        OutboxStatus::Queued => WorkspaceCreateStatus::Queued,
        OutboxStatus::Sent => WorkspaceCreateStatus::Sent,
        OutboxStatus::Abandoning => WorkspaceCreateStatus::Abandoning,
        OutboxStatus::Abandoned => WorkspaceCreateStatus::Abandoned,
        OutboxStatus::Committed => WorkspaceCreateStatus::Committed,
        OutboxStatus::Failed => WorkspaceCreateStatus::Failed,
        _ => return Err("UnsupportedFormat".into()),
    };
    Ok(WorkspaceCreateEntry {
        operation_id: operation.operation_id,
        operation_seq: operation.operation_seq.to_string(),
        node_id: operation.request.node_id,
        request: operation.request,
        status,
        receipt,
    })
}

#[tauri::command]
pub(crate) async fn workspace_queue_create(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceCreateQueueRequest,
) -> Result<WorkspaceCreateEntry, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let operation = queue_node_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        request.parent_id,
        &request.name,
        request.kind,
    )
    .await?;
    entry(operation, None)
}

#[tauri::command]
pub(crate) async fn workspace_create_status(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceCreateListRequest,
) -> Result<WorkspaceCreatePage, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let after = request
        .after_seq
        .parse::<u64>()
        .map_err(|_| "invalid_sequence")?;
    let page = create_status_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        after,
        request.limit,
    )
    .await?;
    let entries = page
        .entries
        .into_iter()
        .map(|item| entry(item.operation, item.receipt))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WorkspaceCreatePage {
        binding: to_contract(binding),
        entries,
        next_after_seq: page.next_after_seq.map(|seq| seq.to_string()),
    })
}

#[tauri::command]
pub(crate) async fn workspace_request_create_abandon(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceCreateAbandonRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    request_create_abandon_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        &request.operation_id,
        #[cfg(test)]
        None,
    )
    .await
}

pub(crate) async fn queue_node_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    parent_id: NodeId,
    name: &str,
    kind: notryx_protocol::NodeKind,
) -> Result<CreateOutboxOperation, String> {
    let mut state = runtime.lock().await;
    state.gate.check_queue(owner).map_err(str::to_owned)?;
    validate_local_binding(&mut state, binding).await?;
    if kind == notryx_protocol::NodeKind::File && !name.to_ascii_lowercase().ends_with(".md") {
        return Err("invalid_markdown_name".into());
    }
    let node_id: NodeId = uuid::Uuid::now_v7()
        .to_string()
        .parse()
        .map_err(|_| "invalid_node")?;
    let request = CreateNodeRequest {
        node_id,
        parent_id,
        name: name.to_owned(),
        kind,
        upload_id: None,
    };
    state
        .store
        .queue_create(binding, &request)
        .await
        .map_err(|error| error.to_string())
}
#[cfg(test)]
pub(crate) async fn queue_create_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    parent_id: NodeId,
    name: &str,
) -> Result<CreateOutboxOperation, String> {
    queue_node_for_renderer(
        runtime,
        owner,
        binding,
        parent_id,
        name,
        notryx_protocol::NodeKind::Folder,
    )
    .await
}

pub(crate) async fn create_status_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: u64,
    limit: u16,
) -> Result<CreateOutboxPage, String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    validate_local_binding(&mut state, binding).await?;
    state
        .store
        .list_create_operations(binding, after, limit)
        .await
        .map_err(|error| error.to_string())
}

pub(super) async fn request_create_abandon_for_renderer(
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
            let Some(PendingOutboxOperation::Create(item)) = state
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
                .request_create_abandon_matching(binding, &item)
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
        let Some(PendingOutboxOperation::Create(item)) = state
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
    let mutation = CreateMutation {
        binding: session.binding().clone(),
        operation_id: parsed,
        operation_seq: item.operation_seq,
        request: item.request.clone(),
    };
    let lookup = session.create_operation(&mutation).await;
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
                    .request_create_abandon_matching(binding, &item)
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
                .request_create_abandon_matching(binding, &item)
                .await
                .map_err(|error| error.to_string())?
            {
                return Err("stale_operation".into());
            }
            Ok(())
        }
        OperationLookup::Receipt(_) => Err("operation_has_result".into()),
        OperationLookup::Retired => Err("operation_retired".into()),
    }
}

#[cfg(test)]
mod tests {
    // Append inside create_native.rs under #[cfg(test)] mod tests.
    // The real-server fixture below mirrors restore_native::tests::rig, whose
    // private module is not accessible from this sibling module.
    use super::*;
    use notryx_client_runtime::Session;
    use notryx_client_store::{ClientStore, OutboxAbandonState};
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use std::os::unix::fs::PermissionsExt;

    struct Rig {
        _directory: tempfile::TempDir,
        server: RunningServer,
        state: Arc<Mutex<RuntimeState>>,
        binding: RegistrationBinding,
        replacement: Arc<Session>,
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
        Workspace::initialize(&notes, &state)
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
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
        let mut sessions = Vec::new();
        for _ in 0..2 {
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
                panic!("pairing")
            };
            sessions.push(Arc::new(
                Session::pair(
                    &format!("http://{}", server.local_addr()),
                    workspace_id,
                    &pairing_secret,
                )
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
        let mut runtime = RuntimeState::new(store, "owner".into());
        runtime.session = Some(session);
        Rig {
            _directory: directory,
            server,
            state: Arc::new(Mutex::new(runtime)),
            binding,
            replacement,
        }
    }
    async fn queue_folder(rig: &Rig) -> (Arc<Session>, CreateOutboxOperation) {
        let session = rig.state.lock().await.session.clone().unwrap();
        let runtime = crate::Runtime(rig.state.clone());
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        crate::sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let parent = session.server_info().await.unwrap().root_id;
        let queued = queue_create_for_renderer(&rig.state, "owner", &rig.binding, parent, "Folder")
            .await
            .unwrap();
        (session, queued)
    }

    fn mutation(session: &Session, item: &CreateOutboxOperation) -> CreateMutation {
        CreateMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            request: item.request.clone(),
        }
    }

    #[tokio::test]
    async fn only_confirmed_missing_records_a_durable_create_abandon_intent() {
        let rig = rig().await;
        let (session, queued) = queue_folder(&rig).await;
        assert!(matches!(
            session
                .create_operation(&mutation(&session, &queued))
                .await
                .unwrap(),
            OperationLookup::Missing
        ));
        request_create_abandon_for_renderer(
            &rig.state,
            "owner",
            &rig.binding,
            &queued.operation_id,
            None,
        )
        .await
        .unwrap();
        let page = create_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation.operation_id, queued.operation_id);
        assert_eq!(page.entries[0].operation.status, OutboxStatus::Abandoning);
        assert_eq!(
            page.entries[0].operation.abandon_state,
            OutboxAbandonState::Requested
        );
        assert!(page.entries[0].receipt.is_none());
        assert_eq!(
            rig.state
                .lock()
                .await
                .store
                .receipt_ack_through(&rig.binding)
                .await
                .unwrap(),
            0
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delayed_missing_does_not_cancel_after_renderer_replacement() {
        let rig = rig().await;
        let (_, queued) = queue_folder(&rig).await;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let runtime = rig.state.clone();
        let binding = rig.binding.clone();
        let id = queued.operation_id.clone();
        let task = tokio::spawn(async move {
            request_create_abandon_for_renderer(
                &runtime,
                "owner",
                &binding,
                &id,
                Some((ready_tx, resume_rx)),
            )
            .await
        });
        ready_rx.await.unwrap();
        rig.state
            .lock()
            .await
            .gate
            .replace_owner("replacement".into());
        resume_tx.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ref error) if error == "stale_renderer_session"));
        let page = create_status_for_renderer(&rig.state, "replacement", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation, queued);
        assert!(page.entries[0].receipt.is_none());
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delayed_missing_does_not_cancel_after_reregistration() {
        let rig = rig().await;
        let (_, queued) = queue_folder(&rig).await;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let runtime = rig.state.clone();
        let binding = rig.binding.clone();
        let id = queued.operation_id.clone();
        let task = tokio::spawn(async move {
            request_create_abandon_for_renderer(
                &runtime,
                "owner",
                &binding,
                &id,
                Some((ready_tx, resume_rx)),
            )
            .await
        });
        ready_rx.await.unwrap();
        let replacement = registration_binding(rig.replacement.binding());
        {
            let mut state = rig.state.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&replacement)
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(rig.replacement.clone());
        }
        resume_tx.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ref error) if error == "stale_registration"));
        let mut state = rig.state.lock().await;
        let previous = state
            .store
            .list_create_operations(&rig.binding, 0, 100)
            .await
            .unwrap();
        assert!(previous.entries[0].frozen);
        assert_eq!(previous.entries[0].operation, queued);
        assert!(
            state
                .store
                .list_create_operations(&replacement, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        drop(state);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn admitted_create_receipt_prevents_local_abandon_intent() {
        let rig = rig().await;
        let (session, queued) = queue_folder(&rig).await;
        let sent = {
            let mut state = rig.state.lock().await;
            let Some(PendingOutboxOperation::Create(sent)) = state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
            else {
                panic!("create")
            };
            sent
        };
        let original = session.create(&mutation(&session, &sent)).await.unwrap();
        assert!(matches!(&original, OperationReceipt::Created { .. }));
        let head = session.server_info().await.unwrap().head_seq;
        assert!(
            matches!(request_create_abandon_for_renderer(&rig.state, "owner", &rig.binding, &queued.operation_id, None).await, Err(ref error) if error == "operation_has_result")
        );
        let page = create_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation, sent);
        assert_eq!(
            page.entries[0].operation.abandon_state,
            OutboxAbandonState::None
        );
        assert_eq!(session.server_info().await.unwrap().head_seq, head);
        assert!(
            matches!(session.create_operation(&mutation(&session, &sent)).await.unwrap(), OperationLookup::Receipt(receipt) if *receipt == original)
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retired_create_receipt_cannot_be_reinterpreted_as_missing() {
        let rig = rig().await;
        let (session, queued) = queue_folder(&rig).await;
        let sent = {
            let mut state = rig.state.lock().await;
            let Some(PendingOutboxOperation::Create(sent)) = state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
            else {
                panic!("create")
            };
            sent
        };
        session.create(&mutation(&session, &sent)).await.unwrap();
        session.acknowledge(sent.operation_seq).await.unwrap();
        assert!(
            matches!(request_create_abandon_for_renderer(&rig.state, "owner", &rig.binding, &queued.operation_id, None).await, Err(ref error) if error == "operation_retired")
        );
        let page = create_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation, sent);
        assert_eq!(
            page.entries[0].operation.abandon_state,
            OutboxAbandonState::None
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delayed_missing_cannot_abandon_a_changed_fifo_head() {
        let rig = rig().await;
        let (_, queued) = queue_folder(&rig).await;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let runtime = rig.state.clone();
        let binding = rig.binding.clone();
        let id = queued.operation_id.clone();
        let task = tokio::spawn(async move {
            request_create_abandon_for_renderer(
                &runtime,
                "owner",
                &binding,
                &id,
                Some((ready_tx, resume_rx)),
            )
            .await
        });
        ready_rx.await.unwrap();
        let sent = {
            let mut state = rig.state.lock().await;
            let Some(PendingOutboxOperation::Create(sent)) = state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
            else {
                panic!("create")
            };
            sent
        };
        resume_tx.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ref error) if error == "stale_operation"));
        let page = create_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation, sent);
        assert_eq!(
            page.entries[0].operation.abandon_state,
            OutboxAbandonState::None
        );
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn disconnected_session_can_cancel_an_unsent_create_without_losing_it() {
        let rig = rig().await;
        let (_, queued) = queue_folder(&rig).await;
        rig.server.shutdown().await.unwrap();
        request_create_abandon_for_renderer(
            &rig.state,
            "owner",
            &rig.binding,
            &queued.operation_id,
            None,
        )
        .await
        .unwrap();
        let page = create_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation.request, queued.request);
        assert_eq!(page.entries[0].operation.status, OutboxStatus::Abandoning);
        assert_eq!(
            page.entries[0].operation.abandon_state,
            OutboxAbandonState::Requested
        );
        assert!(page.entries[0].receipt.is_none());
    }

    #[tokio::test]
    async fn revoked_session_can_cancel_an_unsent_create_without_losing_it() {
        let rig = rig().await;
        let (session, queued) = queue_folder(&rig).await;
        admin::request(
            &rig._directory.path().join("admin.sock"),
            &AdminRequest::RevokeDevice {
                device_id: session.binding().device_id.to_string(),
            },
        )
        .await
        .unwrap();
        request_create_abandon_for_renderer(
            &rig.state,
            "owner",
            &rig.binding,
            &queued.operation_id,
            None,
        )
        .await
        .unwrap();
        let page = create_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation.request, queued.request);
        assert_eq!(page.entries[0].operation.status, OutboxStatus::Abandoning);
        assert_eq!(
            page.entries[0].operation.abandon_state,
            OutboxAbandonState::Requested
        );
        assert!(page.entries[0].receipt.is_none());
        rig.server.shutdown().await.unwrap();
    }
}
