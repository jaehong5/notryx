use crate::{RuntimeState, gate::SendSnapshot, registration_binding};
use notryx_client_runtime::{AbandonMutation, ContentMutation, OperationLookup, Session};
use notryx_client_store::{
    OutboxAbandoned, OutboxCommit, OutboxConflict, OutboxFailure, OutboxFailureKind,
    OutboxOperation, OutboxStatus, RegistrationBinding, StoreError,
};
use notryx_domain::ContentRevision;
use notryx_protocol::{OperationReceipt, WorkspaceSendResult, WorkspaceSendStatus};
use std::sync::Arc;
use tokio::sync::Mutex;

struct SendPermit {
    state: Arc<Mutex<RuntimeState>>,
    snapshot: SendSnapshot,
    active: bool,
}

impl SendPermit {
    async fn finish(mut self) {
        let _ = self.state.lock().await.gate.finish_send(&self.snapshot);
        self.active = false;
    }
}

impl Drop for SendPermit {
    fn drop(&mut self) {
        if self.active {
            let state = self.state.clone();
            let snapshot = self.snapshot;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = state.lock().await.gate.finish_send(&snapshot);
                });
            }
        }
    }
}

fn outcome(
    binding: &RegistrationBinding,
    status: WorkspaceSendStatus,
    item: Option<&OutboxOperation>,
) -> WorkspaceSendResult {
    WorkspaceSendResult {
        binding: crate::to_contract(binding.clone()),
        status,
        operation_id: item.map(|item| item.operation_id.clone()),
        document_id: item.map(|item| item.document_id.clone()),
        generation: item.map(|item| item.generation.to_string()),
    }
}

fn ack_error_status(error: notryx_client_runtime::Error) -> WorkspaceSendStatus {
    match error {
        notryx_client_runtime::Error::Unauthorized => WorkspaceSendStatus::AuthRequired,
        notryx_client_runtime::Error::GatewayAuthRequired => {
            WorkspaceSendStatus::GatewayAuthRequired
        }
        notryx_client_runtime::Error::Network | notryx_client_runtime::Error::Unavailable => {
            WorkspaceSendStatus::AckPending
        }
        _ => WorkspaceSendStatus::NeedsReview,
    }
}

fn send_error_status(error: notryx_client_runtime::Error) -> WorkspaceSendStatus {
    match error {
        notryx_client_runtime::Error::Network | notryx_client_runtime::Error::Unavailable => {
            WorkspaceSendStatus::Pending
        }
        other => ack_error_status(other),
    }
}

async fn request_failure(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    item: &OutboxOperation,
    error: notryx_client_runtime::Error,
) -> Result<WorkspaceSendResult, String> {
    let state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    Ok(outcome(binding, send_error_status(error), Some(item)))
}

async fn acknowledge_and_record(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
    binding: &RegistrationBinding,
    through: u64,
) -> Result<Option<WorkspaceSendStatus>, String> {
    let ack = match session.acknowledge(through).await {
        Ok(ack) => ack,
        Err(error) => {
            let state = state.lock().await;
            valid_session(&state, session, snapshot)?;
            return Ok(Some(ack_error_status(error)));
        }
    };
    let mut state = state.lock().await;
    valid_session(&state, session, snapshot)?;
    match state.store.confirm_receipt_ack(binding, &ack).await {
        Ok(()) => Ok(None),
        Err(StoreError::Storage) => Ok(Some(WorkspaceSendStatus::AckPending)),
        Err(StoreError::StaleRegistration) => Err("stale_registration".into()),
        Err(_) => Ok(Some(WorkspaceSendStatus::NeedsReview)),
    }
}

fn valid_session(
    state: &RuntimeState,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
) -> Result<(), String> {
    state.gate.validate_send(snapshot).map_err(str::to_owned)?;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, session))
    {
        return Err("stale_registration".into());
    }
    Ok(())
}

fn mutation(session: &Session, item: &OutboxOperation) -> Result<ContentMutation, String> {
    if item.binding != registration_binding(session.binding()) || item.frozen {
        return Err("stale_registration".into());
    }
    Ok(ContentMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        node_id: item.document_id.parse().map_err(|_| "invalid_document")?,
        expected_revision: ContentRevision::new(
            item.expected_revision.as_deref().ok_or("outbox_blocked")?,
        )
        .map_err(|_| "invalid_revision")?,
        bytes: item.bytes.clone(),
    })
}

fn abandon(session: &Session, item: &OutboxOperation) -> Result<AbandonMutation, String> {
    if item.binding != registration_binding(session.binding()) || item.frozen {
        return Err("stale_registration".into());
    }
    Ok(AbandonMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().map_err(|_| "invalid_operation")?,
        operation_seq: item.operation_seq,
        node_id: item.document_id.parse().map_err(|_| "invalid_document")?,
        bytes: item.bytes.clone(),
    })
}

async fn apply_receipt(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
    item: &OutboxOperation,
    receipt: &OperationReceipt,
) -> Result<WorkspaceSendStatus, String> {
    let operation_id = item.operation_id.clone();
    let operation_seq = item.operation_seq;
    let workspace_epoch = binding.workspace_epoch.clone();
    let status = match receipt {
        OperationReceipt::Committed {
            content_rev,
            content_hash,
            node_id,
            ..
        } => {
            state
                .store
                .apply_outbox_commit(
                    binding,
                    &OutboxCommit {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                        document_id: node_id.to_string(),
                        content_revision: content_rev.as_str().to_owned(),
                        content_hash: content_hash.clone(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Committed
        }
        OperationReceipt::PreconditionFailed {
            current_content_rev,
            current_content_etag,
            ..
        } => {
            state
                .store
                .apply_outbox_conflict(
                    binding,
                    &OutboxConflict {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                        current_revision: current_content_rev.as_str().to_owned(),
                        current_etag: current_content_etag.clone(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Conflict
        }
        OperationReceipt::NodeDeleted { .. }
        | OperationReceipt::NotFound { .. }
        | OperationReceipt::RecoveryConflict { .. }
        | OperationReceipt::NotApplied { .. } => {
            let kind = match receipt {
                OperationReceipt::NodeDeleted { .. } => OutboxFailureKind::NodeDeleted,
                OperationReceipt::NotFound { .. } => OutboxFailureKind::NotFound,
                OperationReceipt::RecoveryConflict { .. } => OutboxFailureKind::RecoveryConflict,
                _ => OutboxFailureKind::NotApplied,
            };
            state
                .store
                .apply_outbox_failure(
                    binding,
                    &OutboxFailure {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                        kind,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Failed
        }
        OperationReceipt::Abandoned { .. } => {
            state
                .store
                .apply_outbox_abandoned(
                    binding,
                    &OutboxAbandoned {
                        operation_id,
                        operation_seq,
                        workspace_epoch,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            WorkspaceSendStatus::Abandoned
        }
        OperationReceipt::Pending { .. } => WorkspaceSendStatus::Pending,
        _ => return Err("invalid_receipt".into()),
    };
    Ok(status)
}

pub async fn send_one(
    state: Arc<Mutex<RuntimeState>>,
    owner: &str,
) -> Result<WorkspaceSendResult, String> {
    let (snapshot, session) = {
        let mut state = state.lock().await;
        let session = state.session.clone().ok_or("not_connected")?;
        let snapshot = state.gate.begin_send(owner).map_err(str::to_owned)?;
        (snapshot, session)
    };
    let permit = SendPermit {
        state: state.clone(),
        snapshot,
        active: true,
    };
    let result = send_inner(&state, &session, &snapshot).await;
    permit.finish().await;
    result
}

async fn send_inner(
    state: &Arc<Mutex<RuntimeState>>,
    session: &Arc<Session>,
    snapshot: &SendSnapshot,
) -> Result<WorkspaceSendResult, String> {
    let binding = registration_binding(session.binding());
    let ack_through = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        state
            .store
            .receipt_ack_through(&binding)
            .await
            .map_err(|error| error.to_string())?
    };
    if ack_through > 0
        && let Some(status) =
            acknowledge_and_record(state, session, snapshot, &binding, ack_through).await?
    {
        return Ok(outcome(&binding, status, None));
    }
    let item = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        state
            .store
            .prepare_outbox(&binding)
            .await
            .map_err(|error| error.to_string())?
    };
    let Some(item) = item else {
        return Ok(outcome(&binding, WorkspaceSendStatus::Idle, None));
    };
    let receipt = if item.status == OutboxStatus::Abandoning {
        match session.abandon(&abandon(session, &item)?).await {
            Ok(receipt) => receipt,
            Err(error) => {
                return request_failure(state, session, snapshot, &binding, &item, error).await;
            }
        }
    } else if item.status == OutboxStatus::Sent {
        let request = mutation(session, &item)?;
        let lookup = match session.operation(&request).await {
            Ok(lookup) => lookup,
            Err(error) => {
                return request_failure(state, session, snapshot, &binding, &item, error).await;
            }
        };
        match lookup {
            OperationLookup::Receipt(receipt) => *receipt,
            OperationLookup::Missing => {
                {
                    let state = state.lock().await;
                    valid_session(&state, session, snapshot)?;
                }
                match session.put_content(&request).await {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        return request_failure(state, session, snapshot, &binding, &item, error)
                            .await;
                    }
                }
            }
            OperationLookup::Retired => {
                let state = state.lock().await;
                valid_session(&state, session, snapshot)?;
                return Ok(outcome(
                    &binding,
                    WorkspaceSendStatus::NeedsReview,
                    Some(&item),
                ));
            }
        }
    } else {
        return Err("invalid_outbox_state".into());
    };
    let (status, through) = {
        let mut state = state.lock().await;
        valid_session(&state, session, snapshot)?;
        let status = apply_receipt(&mut state, &binding, &item, &receipt).await?;
        let through = state
            .store
            .receipt_ack_through(&binding)
            .await
            .map_err(|error| error.to_string())?;
        (status, through)
    };
    if status != WorkspaceSendStatus::Pending
        && let Some(ack_status) =
            acknowledge_and_record(state, session, snapshot, &binding, through).await?
    {
        return Ok(outcome(&binding, ack_status, Some(&item)));
    }
    Ok(outcome(&binding, status, Some(&item)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProductSession, ProductSessionKind, RuntimeState, registration_binding};
    use notryx_client_runtime::Session;
    use notryx_client_store::ClientStore;
    use notryx_protocol::{
        AdminRequest, AdminResponse, AdminResult, WorkspaceQueueRequest, WorkspaceSendStatus,
    };
    use notryx_server::{
        Workspace, admin,
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    use tokio::sync::Mutex;

    #[test]
    fn ack_failures_report_authentication_and_protocol_errors_distinctly() {
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::Unauthorized),
            WorkspaceSendStatus::AuthRequired
        );
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::GatewayAuthRequired),
            WorkspaceSendStatus::GatewayAuthRequired
        );
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::Network),
            WorkspaceSendStatus::AckPending
        );
        assert_eq!(
            ack_error_status(notryx_client_runtime::Error::Binding),
            WorkspaceSendStatus::NeedsReview
        );
    }

    #[test]
    fn send_failures_report_authentication_protocol_and_network_separately() {
        use notryx_client_runtime::Error;
        assert_eq!(
            send_error_status(Error::Unauthorized),
            WorkspaceSendStatus::AuthRequired
        );
        assert_eq!(
            send_error_status(Error::GatewayAuthRequired),
            WorkspaceSendStatus::GatewayAuthRequired
        );
        assert_eq!(
            send_error_status(Error::Network),
            WorkspaceSendStatus::Pending
        );
        assert_eq!(
            send_error_status(Error::Unavailable),
            WorkspaceSendStatus::Pending
        );
        assert_eq!(
            send_error_status(Error::EpochMismatch),
            WorkspaceSendStatus::NeedsReview
        );
        assert_eq!(
            send_error_status(Error::Contract),
            WorkspaceSendStatus::NeedsReview
        );
        assert_eq!(
            send_error_status(Error::IdempotencyMismatch),
            WorkspaceSendStatus::NeedsReview
        );
        assert_eq!(
            send_error_status(Error::OperationGap),
            WorkspaceSendStatus::NeedsReview
        );
    }

    #[tokio::test]
    async fn revoked_device_before_first_send_reports_auth_and_preserves_the_outbox() {
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
                principal_id: owner_principal_id,
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
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let id = node.node_id.to_string();
        let edit = store
            .open_registered_document(&binding, &id, b"base\n", node.content_rev.unwrap().as_str())
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"unsent\n")
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        state
            .gate
            .open_session(edit.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            edit.edit_session_id.clone(),
            ProductSession {
                document_id: id.clone(),
                binding: binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let request = WorkspaceQueueRequest {
            renderer_session_id: "owner".into(),
            document_id: id.clone(),
            edit_session_id: edit.edit_session_id.clone(),
            expected_generation: "1".into(),
        };
        let mut injection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_queue BEFORE INSERT ON client_outbox BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&mut injection).await.unwrap();
        assert_eq!(state.queue_document(&request).await.unwrap_err(), "Storage");
        let stored = state
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.bytes, b"unsent\n");
        assert_eq!(stored.base_bytes, b"base\n");
        assert_eq!(state.store.receipt_ack_through(&binding).await.unwrap(), 0);
        let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_outbox")
            .fetch_one(&mut injection)
            .await
            .unwrap();
        assert_eq!(outbox_count, 0);
        sqlx::query("DROP TRIGGER fail_queue")
            .execute(&mut injection)
            .await
            .unwrap();
        injection.close().await.unwrap();
        let queued = state.queue_document(&request).await.unwrap();
        assert_eq!(queued.generation, "1");
        assert!(matches!(
            admin::request(
                &config.server.admin_socket,
                &AdminRequest::RevokeDevice {
                    device_id: binding.device_id.clone()
                }
            )
            .await
            .unwrap(),
            AdminResponse::Ok { .. }
        ));
        let state = Arc::new(Mutex::new(state));
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::AuthRequired);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(queued.operation_id.as_str())
        );
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 0);
        let stored = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.bytes, b"unsent\n");
        assert_eq!(stored.base_bytes, b"base\n");
        let operation = locked
            .store
            .read_outbox_operation(&binding, &queued.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.status, OutboxStatus::Sent);
        assert_eq!(operation.bytes, b"unsent\n");
        drop(locked);
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"base\n");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn durable_checkpoint_is_sent_to_real_server_and_receipt_is_recorded() {
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
        let mut store = ClientStore::open(directory.path().join("client.sqlite3"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let id = node.node_id.to_string();
        let edit = store
            .open_registered_document(&binding, &id, b"base\n", node.content_rev.unwrap().as_str())
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 1, b"sent\n")
            .await
            .unwrap();
        let first = store.queue_checkpoint(&binding, &id, 1).await.unwrap();
        let mut state = RuntimeState::new(store, "owner".into());
        state.session = Some(session);
        state
            .gate
            .open_session(edit.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            edit.edit_session_id.clone(),
            ProductSession {
                document_id: id.clone(),
                binding: binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let state = Arc::new(Mutex::new(state));
        let mut injection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_confirm BEFORE UPDATE OF confirmed_through ON outbox_streams BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&mut injection).await.unwrap();
        injection.close().await.unwrap();
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::AckPending);
        assert_eq!(result.generation.as_deref(), Some("1"));
        assert_eq!(result.binding, crate::to_contract(binding.clone()));
        let earlier_binding = result.binding.clone();
        assert_eq!(std::fs::read(notes.join("Example.md")).unwrap(), b"sent\n");
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 1);
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            0
        );
        let status = locked.document_state(&binding, &id).await.unwrap();
        assert_eq!(status.binding, crate::to_contract(binding.clone()));
        assert_eq!(status.confirmed_through, "0");
        assert_eq!(status.latest.unwrap().generation, "1");
        drop(locked);
        let mut injection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("client.sqlite3")),
        )
        .await
        .unwrap();
        sqlx::query("DROP TRIGGER fail_confirm")
            .execute(&mut injection)
            .await
            .unwrap();
        injection.close().await.unwrap();
        assert_eq!(
            send_one(state.clone(), "owner").await.unwrap().status,
            WorkspaceSendStatus::Idle
        );
        let mut locked = state.lock().await;
        assert_eq!(
            locked
                .store
                .confirmed_receipt_through(&binding)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            locked
                .document_state(&binding, &id)
                .await
                .unwrap()
                .confirmed_through,
            "1"
        );
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.base_bytes, b"sent\n");
        let queue_request = WorkspaceQueueRequest {
            renderer_session_id: "owner".into(),
            document_id: id.clone(),
            edit_session_id: edit.edit_session_id.clone(),
            expected_generation: "1".into(),
        };
        let reused = locked.queue_document(&queue_request).await.unwrap();
        assert!(reused.committed);
        assert_eq!(reused.operation_id, first.operation_id);
        locked
            .store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 2, b"second\n")
            .await
            .unwrap();
        assert_eq!(
            locked.queue_document(&queue_request).await,
            Err("stale_checkpoint".into())
        );
        let next = locked
            .queue_document(&WorkspaceQueueRequest {
                expected_generation: "2".into(),
                ..queue_request
            })
            .await
            .unwrap();
        assert!(!next.committed);
        let pending = locked
            .store
            .prepare_outbox(&binding)
            .await
            .unwrap()
            .unwrap();
        let session = locked.session.clone().unwrap();
        let request = mutation(&session, &pending).unwrap();
        drop(locked);
        let receipt = session.put_content(&request).await.unwrap();
        assert!(matches!(receipt, OperationReceipt::Committed { .. }));
        let mut locked = state.lock().await;
        locked
            .store
            .save_registered_checkpoint(&binding, &id, &edit.edit_session_id, 3, b"newer\n")
            .await
            .unwrap();
        drop(locked);
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        assert_eq!(result.generation.as_deref(), Some("2"));
        let mut locked = state.lock().await;
        let draft = locked
            .store
            .read_registered_checkpoint(&binding, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, 3);
        assert_eq!(draft.bytes, b"newer\n");
        assert_eq!(draft.base_bytes, b"second\n");
        let queued = locked
            .store
            .queue_checkpoint(&binding, &id, 3)
            .await
            .unwrap();
        locked
            .store
            .request_outbox_abandon(&binding, &queued.operation_id)
            .await
            .unwrap();
        drop(locked);
        let result = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Abandoned);
        assert_eq!(result.generation.as_deref(), Some("3"));
        assert_eq!(
            std::fs::read(notes.join("Example.md")).unwrap(),
            b"second\n"
        );
        let mut locked = state.lock().await;
        assert_eq!(locked.store.receipt_ack_through(&binding).await.unwrap(), 3);
        assert_eq!(
            locked
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"newer\n"
        );
        locked
            .store
            .close_registered_document(&binding, &id, &edit.edit_session_id)
            .await
            .unwrap();
        locked.gate.close_session(&edit.edit_session_id).unwrap();
        locked.product_sessions.remove(&edit.edit_session_id);
        drop(locked);
        assert!(matches!(
            admin::request(
                &config.server.admin_socket,
                &AdminRequest::RevokeDevice {
                    device_id: binding.device_id.clone()
                }
            )
            .await
            .unwrap(),
            AdminResponse::Ok { .. }
        ));
        let revoked = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(revoked.status, WorkspaceSendStatus::AuthRequired);
        assert_eq!(revoked.binding, earlier_binding);
        let mut locked = state.lock().await;
        assert_eq!(
            locked
                .store
                .read_registered_checkpoint(&binding, &id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            b"newer\n"
        );
        drop(locked);
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
            panic!("pairing grant")
        };
        let replacement = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                workspace_id,
                &pairing_secret,
            )
            .await
            .unwrap(),
        );
        let next_binding = registration_binding(replacement.binding());
        assert_ne!(earlier_binding, crate::to_contract(next_binding.clone()));
        let node = replacement
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == document.node_id)
            .unwrap();
        let mut locked = state.lock().await;
        locked
            .store
            .activate_registration(&next_binding)
            .await
            .unwrap();
        locked.session = Some(replacement);
        let next = locked
            .store
            .open_registered_document(
                &next_binding,
                &id,
                b"second\n",
                node.content_rev.unwrap().as_str(),
            )
            .await
            .unwrap();
        locked
            .store
            .save_registered_checkpoint(
                &next_binding,
                &id,
                &next.edit_session_id,
                1,
                b"new registration\n",
            )
            .await
            .unwrap();
        locked
            .gate
            .open_session(next.edit_session_id.clone())
            .unwrap();
        locked.product_sessions.insert(
            next.edit_session_id.clone(),
            ProductSession {
                document_id: id.clone(),
                binding: next_binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let blocked = locked
            .queue_document(&WorkspaceQueueRequest {
                renderer_session_id: "owner".into(),
                document_id: id.clone(),
                edit_session_id: next.edit_session_id,
                expected_generation: "1".into(),
            })
            .await;
        assert_eq!(blocked, Err("ReconciliationRequired".into()));
        assert!(
            locked
                .store
                .list_outbox(&next_binding, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        drop(locked);
        let idle = send_one(state.clone(), "owner").await.unwrap();
        assert_eq!(idle.status, WorkspaceSendStatus::Idle);
        assert_eq!(idle.binding, crate::to_contract(next_binding));
        assert_ne!(idle.binding, earlier_binding);
        assert_eq!(
            std::fs::read(notes.join("Example.md")).unwrap(),
            b"second\n"
        );
        server.shutdown().await.unwrap();
    }
}
