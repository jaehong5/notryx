use crate::{
    Runtime, RuntimeState, check_caller, from_contract, registration_binding, to_contract,
};
use notryx_client_runtime::{MoveMutation, OperationLookup};
use notryx_client_store::{
    MoveOutboxOperation, MoveOutboxPage, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
};
use notryx_domain::NodeId;
use notryx_protocol::{
    OperationReceipt, WorkspaceMoveAbandonRequest, WorkspaceMoveEntry, WorkspaceMoveListRequest,
    WorkspaceMovePage, WorkspaceMoveQueueRequest, WorkspaceMoveStatus,
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

pub(crate) async fn queue_move_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    node_id: NodeId,
    expected_meta_etag: &str,
    request: &notryx_protocol::MoveNodeRequest,
) -> Result<MoveOutboxOperation, String> {
    let mut state = runtime.lock().await;
    state.gate.check_queue(owner).map_err(str::to_owned)?;
    validate_local_binding(&mut state, binding).await?;
    state
        .store
        .queue_move(binding, node_id, expected_meta_etag, request)
        .await
        .map_err(|error| error.to_string())
}

fn entry(
    operation: MoveOutboxOperation,
    receipt: Option<OperationReceipt>,
) -> Result<WorkspaceMoveEntry, String> {
    let status = match operation.status {
        OutboxStatus::Queued => WorkspaceMoveStatus::Queued,
        OutboxStatus::Sent => WorkspaceMoveStatus::Sent,
        OutboxStatus::Abandoning => WorkspaceMoveStatus::Abandoning,
        OutboxStatus::Abandoned => WorkspaceMoveStatus::Abandoned,
        OutboxStatus::Committed => WorkspaceMoveStatus::Committed,
        OutboxStatus::Failed => WorkspaceMoveStatus::Failed,
        _ => return Err("UnsupportedFormat".into()),
    };
    Ok(WorkspaceMoveEntry {
        operation_id: operation.operation_id,
        operation_seq: operation.operation_seq.to_string(),
        node_id: operation.node_id,
        expected_meta_etag: operation.expected_meta_etag,
        request: operation.request,
        status,
        receipt,
    })
}

pub(crate) async fn move_status_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: u64,
    limit: u16,
) -> Result<MoveOutboxPage, String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    validate_local_binding(&mut state, binding).await?;
    state
        .store
        .list_move_operations(binding, after, limit)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn workspace_queue_move(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceMoveQueueRequest,
) -> Result<WorkspaceMoveEntry, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let operation = queue_move_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        request.node_id,
        &request.expected_meta_etag,
        &request.request,
    )
    .await?;
    entry(operation, None)
}

#[tauri::command]
pub(crate) async fn workspace_move_status(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceMoveListRequest,
) -> Result<WorkspaceMovePage, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let after = request
        .after_seq
        .parse::<u64>()
        .map_err(|_| "invalid_sequence")?;
    let page = move_status_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        after,
        request.limit,
    )
    .await?;
    Ok(WorkspaceMovePage {
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
pub(crate) async fn workspace_request_move_abandon(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceMoveAbandonRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    request_move_abandon_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        &request.operation_id,
        #[cfg(test)]
        None,
    )
    .await
}

pub(super) async fn request_move_abandon_for_renderer(
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
            let Some(PendingOutboxOperation::Move(item)) = state
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
                .request_move_abandon_matching(binding, &item)
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
        let Some(PendingOutboxOperation::Move(item)) = state
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
    let mutation = MoveMutation {
        binding: session.binding().clone(),
        operation_id: parsed,
        operation_seq: item.operation_seq,
        node_id: item.node_id,
        expected_meta_etag: item.expected_meta_etag.clone(),
        request: item.request.clone(),
    };
    let lookup = session.move_operation(&mutation).await;
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
                    .request_move_abandon_matching(binding, &item)
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
                .request_move_abandon_matching(binding, &item)
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
    use crate::{RuntimeState, registration_binding, sync_bootstrap};
    use notryx_client_store::{
        ClientStore, OutboxAbandonState, OutboxStatus, PendingOutboxOperation,
    };
    use notryx_protocol::MoveNodeRequest;
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    const OWNER: &str = "owner";
    const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
    const FILE: &str = "019a1000-0000-7000-8000-000000000002";
    const DEST: &str = "019a1000-0000-7000-8000-000000000003";

    async fn offline() -> (
        tempfile::TempDir,
        Arc<Mutex<RuntimeState>>,
        notryx_client_store::RegistrationBinding,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite");
        let binding = notryx_client_store::RegistrationBinding {
            server_origin: "https://example.invalid".into(),
            workspace_id: Uuid::new_v4().to_string(),
            workspace_epoch: Uuid::new_v4().to_string(),
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
        };
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&binding).await.unwrap();
        store.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (1,?,'1')")
            .bind(ROOT)
            .execute(&mut db)
            .await
            .unwrap();
        for (id, parent, name, kind) in [
            (ROOT, None, "", "folder"),
            (FILE, Some(ROOT), "file.md", "file"),
            (DEST, Some(ROOT), "destination", "folder"),
        ] {
            let node = serde_json::json!({"node_id":id,"parent_id":parent,"name":name,"kind":kind,"deleted":false,
                "content_rev":if kind == "file" {Some("content")} else {None},
                "content_etag":if kind == "file" {Some("\"content\"")} else {None},
                "content_hash":if kind == "file" {Some(format!("sha256:{}", "a".repeat(64)))} else {None},
                "size":if kind == "file" {Some("0")} else {None},
                "meta_rev":"meta","meta_etag":"\"meta\"","node_etag":"\"node\"","children_generation":"0","last_change_seq":"1"});
            sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (1,?,?,0)").bind(id).bind(node.to_string()).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        let store = ClientStore::open(&path).await.unwrap();
        let state = Arc::new(Mutex::new(RuntimeState::new(store, OWNER.into())));
        (dir, state, binding)
    }

    fn request() -> MoveNodeRequest {
        MoveNodeRequest {
            expected_parent_id: ROOT.parse().unwrap(),
            parent_id: DEST.parse().unwrap(),
            name: "moved.md".into(),
        }
    }

    async fn connected() -> (
        tempfile::TempDir,
        notryx_server::http::RunningServer,
        crate::Runtime,
        Arc<notryx_client_runtime::Session>,
        notryx_protocol::NodeState,
        notryx_client_store::RegistrationBinding,
    ) {
        let (directory, server, old_runtime, session, _) = sync_bootstrap::tests::setup().await;
        drop(old_runtime);
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("move-client.sqlite"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let mut initial = RuntimeState::new(store, OWNER.into());
        initial.session = Some(session.clone());
        let runtime = crate::Runtime(Arc::new(Mutex::new(initial)));
        let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let file = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        (directory, server, runtime, session, file, binding)
    }

    fn rename(file: &notryx_protocol::NodeState) -> MoveNodeRequest {
        let parent = file.parent_id.unwrap();
        MoveNodeRequest {
            expected_parent_id: parent,
            parent_id: parent,
            name: "renamed.md".into(),
        }
    }

    #[tokio::test]
    async fn queue_checks_owner_binding_and_confirmed_structure_and_status() {
        let (_dir, state, binding) = offline().await;
        let node = FILE.parse().unwrap();
        let mut stale = binding.clone();
        stale.workspace_epoch = Uuid::new_v4().to_string();
        assert!(
            queue_move_for_renderer(&state, "old-owner", &binding, node, "\"meta\"", &request())
                .await
                .is_err()
        );
        assert!(
            queue_move_for_renderer(&state, OWNER, &stale, node, "\"meta\"", &request())
                .await
                .is_err()
        );
        assert!(
            queue_move_for_renderer(&state, OWNER, &binding, node, "\"old\"", &request())
                .await
                .is_err()
        );
        let bad = MoveNodeRequest {
            expected_parent_id: DEST.parse().unwrap(),
            ..request()
        };
        assert!(
            queue_move_for_renderer(&state, OWNER, &binding, node, "\"meta\"", &bad)
                .await
                .is_err()
        );
        let item = queue_move_for_renderer(&state, OWNER, &binding, node, "\"meta\"", &request())
            .await
            .unwrap();
        assert_eq!(item.operation_seq, 1);
        assert_eq!(item.status, OutboxStatus::Queued);
        assert_eq!(item.request, request());
        assert_eq!(item.expected_meta_etag, "\"meta\"");
        assert!(
            move_status_for_renderer(&state, "old-owner", &binding, 0, 10)
                .await
                .is_err()
        );
        assert!(
            move_status_for_renderer(&state, OWNER, &binding, 0, 0)
                .await
                .is_err()
        );
        let page = move_status_for_renderer(&state, OWNER, &binding, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation, item);
        assert!(page.entries[0].receipt.is_none());
        assert!(page.next_after_seq.is_none());
    }

    #[tokio::test]
    async fn offline_cancel_queued_move_persists_without_changing_remote_tree() {
        let (dir, state, binding) = offline().await;
        let item = queue_move_for_renderer(
            &state,
            OWNER,
            &binding,
            FILE.parse().unwrap(),
            "\"meta\"",
            &request(),
        )
        .await
        .unwrap();
        assert!(
            request_move_abandon_for_renderer(
                &state,
                "old-owner",
                &binding,
                &item.operation_id,
                None
            )
            .await
            .is_err()
        );
        request_move_abandon_for_renderer(&state, OWNER, &binding, &item.operation_id, None)
            .await
            .unwrap();
        let guard = state.lock().await;
        drop(guard);
        drop(state);
        let mut store = ClientStore::open(dir.path().join("client.sqlite"))
            .await
            .unwrap();
        let Some(PendingOutboxOperation::Move(saved)) =
            store.next_pending_outbox(&binding).await.unwrap()
        else {
            panic!("move must remain the FIFO head")
        };
        assert_eq!(saved.operation_id, item.operation_id);
        assert_eq!(saved.request, item.request);
        assert_eq!(saved.abandon_state, OutboxAbandonState::Requested);
        let (_, nodes) = store.sync_index(&binding).await.unwrap().unwrap();
        let file = nodes
            .iter()
            .find(|node| node.node_id.to_string() == FILE)
            .unwrap();
        assert_eq!(file.name, "file.md");
        assert_eq!(file.parent_id.unwrap().to_string(), ROOT);
    }

    #[tokio::test]
    async fn offline_cancel_sent_move_requires_server_result_and_preserves_request() {
        let (_dir, state, binding) = offline().await;
        let item = queue_move_for_renderer(
            &state,
            OWNER,
            &binding,
            FILE.parse().unwrap(),
            "\"meta\"",
            &request(),
        )
        .await
        .unwrap();
        {
            let mut guard = state.lock().await;
            assert!(matches!(
                guard.store.prepare_pending_outbox(&binding).await.unwrap(),
                Some(PendingOutboxOperation::Move(_))
            ));
        }
        assert_eq!(
            request_move_abandon_for_renderer(&state, OWNER, &binding, &item.operation_id, None)
                .await
                .unwrap_err(),
            "not_connected"
        );
        let mut guard = state.lock().await;
        let Some(PendingOutboxOperation::Move(saved)) =
            guard.store.next_pending_outbox(&binding).await.unwrap()
        else {
            panic!("move must remain pending")
        };
        assert_eq!(saved.operation_id, item.operation_id);
        assert_eq!(saved.abandon_state, OutboxAbandonState::None);
        assert_eq!(saved.status, OutboxStatus::Sent);
    }

    #[tokio::test]
    async fn connected_queue_respects_actual_server_snapshot_and_draft() {
        let (_directory, server, runtime, session, other) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let nodes = session.inventory(None).await.unwrap().nodes;
        let file = nodes.iter().find(|node| node.name == "move.md").unwrap();
        let root = file.parent_id.unwrap();
        let binding = registration_binding(session.binding());
        let req = MoveNodeRequest {
            expected_parent_id: root,
            parent_id: root,
            name: "renamed.md".into(),
        };
        let item = queue_move_for_renderer(
            &runtime.0,
            OWNER,
            &binding,
            file.node_id,
            &file.meta_etag,
            &req,
        )
        .await
        .unwrap();
        assert_eq!(item.operation_seq, 2);
        let mut guard = runtime.0.lock().await;
        let draft = guard
            .store
            .read_registered_checkpoint(&binding, &other)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        drop(guard);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn connected_cancel_checks_receipt_before_recording_abandon() {
        let (directory, server, runtime, session, file, binding) = connected().await;
        let item = queue_move_for_renderer(
            &runtime.0,
            OWNER,
            &binding,
            file.node_id,
            &file.meta_etag,
            &rename(&file),
        )
        .await
        .unwrap();
        {
            let mut guard = runtime.0.lock().await;
            assert!(matches!(
                guard.store.prepare_pending_outbox(&binding).await.unwrap(),
                Some(PendingOutboxOperation::Move(_))
            ));
        }
        let mutation = notryx_client_runtime::MoveMutation {
            binding: session.binding().clone(),
            operation_id: item.operation_id.parse().unwrap(),
            operation_seq: item.operation_seq,
            node_id: item.node_id,
            expected_meta_etag: item.expected_meta_etag.clone(),
            request: item.request.clone(),
        };
        assert!(matches!(
            session.move_node(&mutation).await.unwrap(),
            notryx_protocol::OperationReceipt::Moved { .. }
        ));
        assert_eq!(
            request_move_abandon_for_renderer(
                &runtime.0,
                OWNER,
                &binding,
                &item.operation_id,
                None
            )
            .await
            .unwrap_err(),
            "operation_has_result"
        );
        let mut guard = runtime.0.lock().await;
        let Some(PendingOutboxOperation::Move(saved)) =
            guard.store.next_pending_outbox(&binding).await.unwrap()
        else {
            panic!("move remains pending for receipt recovery")
        };
        assert_eq!(saved.abandon_state, OutboxAbandonState::None);
        assert_eq!(saved.operation_id, item.operation_id);
        drop(guard);
        assert_eq!(
            std::fs::read(directory.path().join("notes/renamed.md")).unwrap(),
            b"move source\n"
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn late_cancel_lookup_cannot_mutate_replaced_session() {
        let (_directory, server, runtime, _session, file, binding) = connected().await;
        let item = queue_move_for_renderer(
            &runtime.0,
            OWNER,
            &binding,
            file.node_id,
            &file.meta_etag,
            &rename(&file),
        )
        .await
        .unwrap();
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let state = runtime.0.clone();
        let task_state = state.clone();
        let task_binding = binding.clone();
        let operation_id = item.operation_id.clone();
        let task = tokio::spawn(async move {
            request_move_abandon_for_renderer(
                &task_state,
                OWNER,
                &task_binding,
                &operation_id,
                Some((reached_tx, resume_rx)),
            )
            .await
        });
        reached_rx.await.unwrap();
        state.lock().await.session = None;
        resume_tx.send(()).unwrap();
        assert_eq!(task.await.unwrap().unwrap_err(), "stale_registration");
        let mut guard = state.lock().await;
        let Some(PendingOutboxOperation::Move(saved)) =
            guard.store.next_pending_outbox(&binding).await.unwrap()
        else {
            panic!("move remains pending")
        };
        assert_eq!(saved.operation_id, item.operation_id);
        assert_eq!(saved.abandon_state, OutboxAbandonState::None);
        drop(guard);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn status_rejects_corrupt_queued_payload_without_consuming_operation() {
        let (dir, state, binding) = offline().await;
        let item = queue_move_for_renderer(
            &state,
            OWNER,
            &binding,
            FILE.parse().unwrap(),
            "\"meta\"",
            &request(),
        )
        .await
        .unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(dir.path().join("client.sqlite")),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE client_outbox SET payload_hash=? WHERE operation_id=?")
            .bind("0".repeat(64))
            .bind(&item.operation_id)
            .execute(&mut db)
            .await
            .unwrap();
        assert!(
            move_status_for_renderer(&state, OWNER, &binding, 0, 10)
                .await
                .is_err()
        );
        let row: (String, Vec<u8>) =
            sqlx::query_as("SELECT status,bytes FROM client_outbox WHERE operation_id=?")
                .bind(&item.operation_id)
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(row.0, "queued");
        assert_eq!(row.1, serde_json::to_vec(&item.request).unwrap());
        db.close().await.unwrap();
    }
}
