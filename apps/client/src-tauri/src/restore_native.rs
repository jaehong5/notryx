use crate::{
    Runtime, RuntimeState, check_caller, from_contract, registration_binding, to_contract,
};
use notryx_client_runtime::{OperationLookup, RestoreMutation};
use notryx_client_store::{
    OutboxStatus, PendingOutboxOperation, RegistrationBinding, RestoreOutboxOperation,
    RestoreOutboxPage,
};
use notryx_domain::NodeId;
use notryx_protocol::{
    OperationReceipt, RestoreNodeRequest, WorkspaceRestoreAbandonRequest, WorkspaceRestoreEntry,
    WorkspaceRestoreListRequest, WorkspaceRestorePage, WorkspaceRestoreQueueRequest,
    WorkspaceRestoreStatus,
};
use std::sync::Arc;
use tokio::sync::Mutex;

fn entry(
    operation: RestoreOutboxOperation,
    receipt: Option<OperationReceipt>,
) -> Result<WorkspaceRestoreEntry, String> {
    let status = match operation.status {
        OutboxStatus::Queued => WorkspaceRestoreStatus::Queued,
        OutboxStatus::Sent => WorkspaceRestoreStatus::Sent,
        OutboxStatus::Abandoning => WorkspaceRestoreStatus::Abandoning,
        OutboxStatus::Abandoned => WorkspaceRestoreStatus::Abandoned,
        OutboxStatus::Committed => WorkspaceRestoreStatus::Committed,
        OutboxStatus::Failed => WorkspaceRestoreStatus::Failed,
        _ => return Err("UnsupportedFormat".into()),
    };
    Ok(WorkspaceRestoreEntry {
        operation_id: operation.operation_id,
        operation_seq: operation.operation_seq.to_string(),
        node_id: operation.node_id,
        request: operation.request,
        status,
        receipt,
    })
}

#[tauri::command]
pub(crate) async fn workspace_queue_restore(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceRestoreQueueRequest,
) -> Result<WorkspaceRestoreEntry, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let operation = queue_restore_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        request.node_id,
        &request.request,
    )
    .await?;
    entry(operation, None)
}

#[tauri::command]
pub(crate) async fn workspace_restore_status(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceRestoreListRequest,
) -> Result<WorkspaceRestorePage, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let after = request
        .after_seq
        .parse::<u64>()
        .map_err(|_| "invalid_sequence")?;
    let page = restore_status_for_renderer(
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
    Ok(WorkspaceRestorePage {
        binding: to_contract(binding),
        entries,
        next_after_seq: page.next_after_seq.map(|seq| seq.to_string()),
    })
}

#[tauri::command]
pub(crate) async fn workspace_request_restore_abandon(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceRestoreAbandonRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    request_restore_abandon_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        &request.operation_id,
        #[cfg(test)]
        None,
    )
    .await
}

pub(crate) async fn queue_restore_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    node: NodeId,
    request: &RestoreNodeRequest,
) -> Result<RestoreOutboxOperation, String> {
    let mut state = runtime.lock().await;
    state.gate.check_queue(owner).map_err(str::to_owned)?;
    let current = state.session.as_ref().ok_or("not_connected")?;
    if registration_binding(current.binding()) != *binding {
        return Err("stale_registration".into());
    }
    state
        .store
        .queue_restore(binding, node, request)
        .await
        .map_err(|error| error.to_string())
}
pub(crate) async fn restore_status_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: u64,
    limit: u16,
) -> Result<RestoreOutboxPage, String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    let current = state.session.as_ref().ok_or("not_connected")?;
    if registration_binding(current.binding()) != *binding {
        return Err("stale_registration".into());
    }
    state
        .store
        .list_restore_operations(binding, after, limit)
        .await
        .map_err(|error| error.to_string())
}

async fn request_restore_abandon_for_renderer(
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
        let Some(PendingOutboxOperation::Restore(item)) = state
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
    let mutation = RestoreMutation {
        binding: session.binding().clone(),
        operation_id: parsed,
        operation_seq: item.operation_seq,
        node_id: item.node_id,
        request: item.request.clone(),
    };
    let lookup = session.restore_operation(&mutation).await;
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
                .request_restore_abandon_matching(binding, &item)
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
    use super::*;
    use crate::registration_binding;
    use notryx_client_runtime::Session;
    use notryx_client_store::ClientStore;
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::os::unix::fs::PermissionsExt;

    struct Rig {
        directory: tempfile::TempDir,
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
            directory,
            server,
            state: Arc::new(Mutex::new(runtime)),
            binding,
            replacement,
        }
    }
    fn request() -> RestoreNodeRequest {
        serde_json::from_value(serde_json::json!({"expected_meta_rev":"meta-1", "parent_id":"019a1000-0000-7000-8000-000000000001", "name":"restore.md"})).unwrap()
    }
    fn node() -> NodeId {
        "019a1000-0000-7000-8000-000000000002".parse().unwrap()
    }

    #[tokio::test]
    async fn active_renderer_queues_once_and_reads_durable_pending_state() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        assert_eq!(queued.operation_seq, 1);
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation, queued);
        assert!(page.entries[0].receipt.is_none());
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restore_status_displays_durable_abandon_request() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        {
            let mut state = rig.state.lock().await;
            let Some(notryx_client_store::PendingOutboxOperation::Restore(sent)) = state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
            else {
                panic!("restore must be at the head of the outbox")
            };
            assert!(
                state
                    .store
                    .request_restore_abandon_matching(&rig.binding, &sent)
                    .await
                    .unwrap()
            );
        }
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        let stored = page.entries.into_iter().next().unwrap();
        let display = entry(stored.operation, stored.receipt).unwrap();
        let json = serde_json::to_value(&display).unwrap();
        assert_eq!(json["status"], "abandoning");
        assert_eq!(json["operation_id"], queued.operation_id);
        assert_eq!(json["operation_seq"], "1");
        assert!(json["receipt"].is_null());
        assert_eq!(display.request, request());
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restore_status_displays_abandoned_receipt() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        let receipt: OperationReceipt = serde_json::from_value(serde_json::json!({
            "status": "abandoned",
            "operation_id": queued.operation_id,
            "operation_seq": "1",
            "workspace_epoch": rig.binding.workspace_epoch,
        }))
        .unwrap();
        {
            let mut state = rig.state.lock().await;
            let Some(notryx_client_store::PendingOutboxOperation::Restore(sent)) = state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
            else {
                panic!("restore must be at the head of the outbox")
            };
            assert!(
                state
                    .store
                    .request_restore_abandon_matching(&rig.binding, &sent)
                    .await
                    .unwrap()
            );
            state
                .store
                .prepare_pending_outbox(&rig.binding)
                .await
                .unwrap()
                .unwrap();
            state
                .store
                .apply_restore_receipt(&rig.binding, &receipt)
                .await
                .unwrap();
        }
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        let stored = page.entries.into_iter().next().unwrap();
        let display = entry(stored.operation, stored.receipt).unwrap();
        let json = serde_json::to_value(&display).unwrap();
        assert_eq!(json["status"], "abandoned");
        assert_eq!(json["operation_id"], queued.operation_id);
        assert_eq!(json["operation_seq"], "1");
        assert_eq!(display.request, request());
        assert_eq!(display.receipt, Some(receipt));
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_restore_checks_missing_before_recording_durable_intent() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        request_restore_abandon_for_renderer(
            &rig.state,
            "owner",
            &rig.binding,
            &queued.operation_id,
            None,
        )
        .await
        .unwrap();
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation.operation_id, queued.operation_id);
        assert_eq!(
            page.entries[0].operation.operation_seq,
            queued.operation_seq
        );
        assert_eq!(page.entries[0].operation.request, queued.request);
        assert_eq!(page.entries[0].operation.status, OutboxStatus::Abandoning);
        assert_eq!(
            page.entries[0].operation.abandon_state,
            notryx_client_store::OutboxAbandonState::Requested
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
    async fn cancel_restore_keeps_request_when_server_cannot_confirm_missing() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        rig.server.shutdown().await.unwrap();
        assert!(
            request_restore_abandon_for_renderer(
                &rig.state,
                "owner",
                &rig.binding,
                &queued.operation_id,
                None,
            )
            .await
            .is_err()
        );
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation, queued);
        assert!(page.entries[0].receipt.is_none());
    }

    #[tokio::test]
    async fn cancel_restore_rejects_delayed_lookup_after_renderer_replacement() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let runtime = rig.state.clone();
        let binding = rig.binding.clone();
        let id = queued.operation_id.clone();
        let task = tokio::spawn(async move {
            request_restore_abandon_for_renderer(
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
        let page = restore_status_for_renderer(&rig.state, "replacement", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation, queued);
        assert!(page.entries[0].receipt.is_none());
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_restore_rejects_a_non_head_operation() {
        let rig = rig().await;
        let first =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        let second_node: NodeId = "019a1000-0000-7000-8000-000000000004".parse().unwrap();
        let second =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, second_node, &request())
                .await
                .unwrap();
        assert!(matches!(
            request_restore_abandon_for_renderer(
                &rig.state,
                "owner",
                &rig.binding,
                &second.operation_id,
                None,
            )
            .await,
            Err(ref error) if error == "not_fifo_head"
        ));
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].operation, first);
        assert_eq!(page.entries[1].operation, second);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_restore_rejects_delayed_lookup_after_reregistration() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let runtime = rig.state.clone();
        let binding = rig.binding.clone();
        let id = queued.operation_id.clone();
        let task = tokio::spawn(async move {
            request_restore_abandon_for_renderer(
                &runtime,
                "owner",
                &binding,
                &id,
                Some((ready_tx, resume_rx)),
            )
            .await
        });
        ready_rx.await.unwrap();
        {
            let mut state = rig.state.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(rig.replacement.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(rig.replacement.clone());
        }
        resume_tx.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ref error) if error == "stale_registration"));
        let current = registration_binding(rig.replacement.binding());
        let page = restore_status_for_renderer(&rig.state, "owner", &current, 0, 100)
            .await
            .unwrap();
        assert!(page.entries.is_empty());
        let mut state = rig.state.lock().await;
        let old = state
            .store
            .list_restore_operations(&rig.binding, 0, 100)
            .await
            .unwrap();
        assert!(old.entries[0].frozen);
        assert_eq!(old.entries[0].operation, queued);
        drop(state);
        rig.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_restore_rejects_lookup_invalidated_by_resync() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let runtime = rig.state.clone();
        let binding = rig.binding.clone();
        let id = queued.operation_id.clone();
        let task = tokio::spawn(async move {
            request_restore_abandon_for_renderer(
                &runtime,
                "owner",
                &binding,
                &id,
                Some((ready_tx, resume_rx)),
            )
            .await
        });
        ready_rx.await.unwrap();
        {
            let mut state = rig.state.lock().await;
            state
                .transmission_gate
                .block(&rig.binding, crate::send::TransmissionBlock::ResyncRequired);
            assert!(
                state
                    .transmission_gate
                    .clear(&rig.binding, crate::send::TransmissionBlock::ResyncRequired,)
            );
        }
        resume_tx.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(ref error) if error == "stale_sync"));
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation, queued);
        rig.server.shutdown().await.unwrap();
    }

    #[derive(Clone, Copy)]
    enum LookupRace {
        Resync,
        Renderer,
        Pairing,
        Registration,
    }

    async fn cancel_with_stale_unverified_lookup(status: u16, body: String, race: LookupRace) {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let workspace = uuid::Uuid::new_v4().to_string();
        let epoch = uuid::Uuid::new_v4().to_string();
        let pair = serde_json::json!({"protocol":"mdw-sync/1.2", "workspace_id":workspace, "workspace_epoch":epoch, "principal_id":uuid::Uuid::new_v4().to_string(), "device_id":uuid::Uuid::new_v4().to_string(), "device_stream_id":uuid::Uuid::new_v4().to_string(), "device_token":"synthetic-token", "role":"reader", "idle_expires_at":"1", "absolute_expires_at":"2"}).to_string();
        let replacement_pair = serde_json::json!({"protocol":"mdw-sync/1.2", "workspace_id":workspace, "workspace_epoch":epoch, "principal_id":uuid::Uuid::new_v4().to_string(), "device_id":uuid::Uuid::new_v4().to_string(), "device_stream_id":uuid::Uuid::new_v4().to_string(), "device_token":"synthetic-token-2", "role":"reader", "idle_expires_at":"1", "absolute_expires_at":"2"}).to_string();
        let fake = tokio::spawn(async move {
            for (response_status, body) in [(200, pair), (200, replacement_pair), (status, body)] {
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
                    "HTTP/1.1 {response_status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        let replacement = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                workspace.parse().unwrap(),
                "synthetic-pairing-2",
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
        let queued = store
            .queue_restore(&binding, node(), &request())
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        let runtime = Arc::new(Mutex::new(state));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let task = {
            let runtime = runtime.clone();
            let binding = binding.clone();
            let id = queued.operation_id.clone();
            tokio::spawn(async move {
                request_restore_abandon_for_renderer(
                    &runtime,
                    "owner",
                    &binding,
                    &id,
                    Some((ready_tx, resume_rx)),
                )
                .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();
        let mut pair = None;
        {
            let mut state = runtime.lock().await;
            match race {
                LookupRace::Resync => {
                    state
                        .transmission_gate
                        .block(&binding, crate::send::TransmissionBlock::ResyncRequired);
                    assert!(
                        state
                            .transmission_gate
                            .clear(&binding, crate::send::TransmissionBlock::ResyncRequired)
                    );
                }
                LookupRace::Renderer => state.gate.replace_owner("replacement".into()),
                LookupRace::Pairing => pair = Some(state.gate.begin_pair("owner").unwrap()),
                LookupRace::Registration => {
                    let pair = state.gate.begin_pair("owner").unwrap();
                    state
                        .store
                        .activate_registration(&registration_binding(replacement.binding()))
                        .await
                        .unwrap();
                    state.gate.commit_pair(&pair).unwrap();
                    state.session = Some(replacement.clone());
                }
            }
        }
        resume_tx.send(()).unwrap();
        let result = task.await.unwrap();
        match race {
            LookupRace::Resync => {
                assert!(matches!(result, Err(ref error) if error == "stale_sync"))
            }
            LookupRace::Renderer => {
                assert!(matches!(result, Err(ref error) if error == "stale_renderer_session"))
            }
            LookupRace::Pairing => {
                assert!(matches!(result, Err(ref error) if error == "pairing_in_progress"))
            }
            LookupRace::Registration => {
                assert!(matches!(result, Err(ref error) if error == "stale_registration"))
            }
        }
        {
            let mut state = runtime.lock().await;
            if let Some(pair) = pair {
                state.gate.fail_pair(&pair).unwrap();
            }
            if matches!(race, LookupRace::Registration) {
                let current = registration_binding(replacement.binding());
                assert!(!state.transmission_gate.blocked(&current));
                assert!(!state.transmission_gate.blocked(&binding));
            } else {
                assert!(
                    state
                        .transmission_gate
                        .has_block(&binding, crate::send::TransmissionBlock::UnverifiedResponse)
                );
            }
            let page = state
                .store
                .list_restore_operations(&binding, 0, 100)
                .await
                .unwrap();
            assert_eq!(page.entries[0].operation, queued);
            assert!(page.entries[0].receipt.is_none());
        }
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_restore_keeps_review_block_for_stale_invalid_lookup() {
        cancel_with_stale_unverified_lookup(200, "{}".into(), LookupRace::Resync).await;
    }

    #[tokio::test]
    async fn cancel_restore_keeps_review_block_for_stale_retired_lookup() {
        let body = serde_json::json!({"code":"operation_retired", "message":"retired", "request_id":"019a1000-0000-7000-8000-000000000003"}).to_string();
        cancel_with_stale_unverified_lookup(410, body, LookupRace::Resync).await;
    }

    #[tokio::test]
    async fn cancel_restore_keeps_review_block_after_renderer_replacement() {
        cancel_with_stale_unverified_lookup(200, "{}".into(), LookupRace::Renderer).await;
    }

    #[tokio::test]
    async fn cancel_restore_keeps_review_block_during_pairing() {
        let body = serde_json::json!({"code":"operation_retired", "message":"retired", "request_id":"019a1000-0000-7000-8000-000000000003"}).to_string();
        cancel_with_stale_unverified_lookup(410, body, LookupRace::Pairing).await;
    }

    #[tokio::test]
    async fn cancel_restore_does_not_block_replacement_registration_on_old_error() {
        cancel_with_stale_unverified_lookup(200, "{}".into(), LookupRace::Registration).await;
    }

    #[tokio::test]
    async fn cancel_restore_does_not_replace_existing_server_receipt() {
        let rig = rig().await;
        let queued =
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .unwrap();
        let session = rig.state.lock().await.session.clone().unwrap();
        let mutation = notryx_client_runtime::RestoreMutation {
            binding: session.binding().clone(),
            operation_id: queued.operation_id.parse().unwrap(),
            operation_seq: queued.operation_seq,
            node_id: queued.node_id,
            request: queued.request.clone(),
        };
        let original = session.abandon_restore(&mutation).await.unwrap();
        assert!(matches!(original, OperationReceipt::Abandoned { .. }));
        assert!(matches!(
            request_restore_abandon_for_renderer(
                &rig.state,
                "owner",
                &rig.binding,
                &queued.operation_id,
                None,
            )
            .await,
            Err(ref error) if error == "operation_has_result"
        ));
        assert!(matches!(
            session.restore_operation(&mutation).await.unwrap(),
            notryx_client_runtime::OperationLookup::Receipt(receipt) if *receipt == original
        ));
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation, queued);
        assert!(page.entries[0].receipt.is_none());
        rig.server.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn stale_renderer_cannot_enqueue_or_read_restore() {
        let rig = rig().await;
        rig.state
            .lock()
            .await
            .gate
            .replace_owner("replacement".into());
        assert!(
            matches!(queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request()).await, Err(ref error) if error == "stale_renderer_session")
        );
        assert!(
            matches!(restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100).await, Err(ref error) if error == "stale_renderer_session")
        );
        let page = restore_status_for_renderer(&rig.state, "replacement", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert!(page.entries.is_empty());
        rig.server.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn reregistered_session_rejects_old_binding_without_enqueuing() {
        let rig = rig().await;
        {
            let mut state = rig.state.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state
                .store
                .activate_registration(&registration_binding(rig.replacement.binding()))
                .await
                .unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(rig.replacement.clone());
        }
        assert!(
            matches!(queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request()).await, Err(ref error) if error == "stale_registration")
        );
        assert!(
            matches!(restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100).await, Err(ref error) if error == "stale_registration")
        );
        let current = registration_binding(rig.replacement.binding());
        assert!(
            restore_status_for_renderer(&rig.state, "owner", &current, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        rig.server.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn failed_restore_enqueue_does_not_publish_pending_state() {
        let rig = rig().await;
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(rig.directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::raw_sql("CREATE TRIGGER reject_restore BEFORE INSERT ON client_outbox WHEN NEW.operation_kind='restore' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;").execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        assert!(
            queue_restore_for_renderer(&rig.state, "owner", &rig.binding, node(), &request())
                .await
                .is_err()
        );
        let page = restore_status_for_renderer(&rig.state, "owner", &rig.binding, 0, 100)
            .await
            .unwrap();
        assert!(page.entries.is_empty());
        rig.server.shutdown().await.unwrap();
    }
}
