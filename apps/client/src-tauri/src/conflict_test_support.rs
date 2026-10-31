// Shared fixtures must not register tests; each scenario belongs to one test module.
pub(crate) mod choose_remote {
    pub(crate) use super::snapshot::*;

    pub(crate) const BASE_HASH: &str =
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573";
    pub(crate) const LOCAL_HASH: &str =
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de";
    pub(crate) const FOLLOWUP_HASH: &str =
        "sha256:3617ea161bd9febb1c19b7411687c2c34c0f264b5efbd7339e0cbba453df8fe2";

    pub(crate) type Request = notryx_protocol::WorkspaceContentConflictChooseRemoteRequest;

    pub(crate) fn choice_request(rig: &Rig, generation: u64, version: Option<u64>) -> Request {
        serde_json::from_value(serde_json::json!({
            "renderer_session_id": OWNER,
            "binding": crate::to_contract(rig.binding.clone()),
            "document_id": rig.document,
            "edit_session_id": rig.edit_session,
            "operation_id": rig.operation.operation_id,
            "generation": generation.to_string(),
            "expected_intent_version": version.map(|v| v.to_string()),
            "base_revision": rig.initial.content_rev.as_ref().unwrap().as_str(),
            "conflict_revision": rig.first_peer.content_rev.as_ref().unwrap().as_str(),
            "remote_revision": rig.first_peer.content_rev.as_ref().unwrap().as_str(),
            "base_hash": BASE_HASH,
            "local_hash": if generation == 1 { LOCAL_HASH } else { FOLLOWUP_HASH },
            "remote_hash": rig.first_peer.content_hash.as_ref().unwrap(),
        }))
        .unwrap()
    }

    pub(crate) async fn choose(rig: &Rig, request: Request) -> Result<serde_json::Value, String> {
        let result =
            crate::conflict_snapshot_native::choose_remote_for_renderer(&rig.runtime, request)
                .await?;
        Ok(serde_json::to_value(result).unwrap())
    }

    pub(crate) fn spawn_choice(rig: &Rig, request: Request) -> tokio::task::JoinHandle<bool> {
        let runtime = Runtime(rig.runtime.0.clone());
        tokio::spawn(async move {
            crate::conflict_snapshot_native::choose_remote_for_renderer(&runtime, request)
                .await
                .is_err()
        })
    }
}

pub(crate) mod complete_pending {
    pub(crate) use super::continue_remote::*;

    pub(crate) async fn ready_for_completion()
    -> (Rig, notryx_client_store::ContentConflictIntent, String) {
        let (rig, intent, followup) = continue_rig().await;
        assert_eq!(
            continue_choice(&rig, intent.version).await.unwrap()["status"],
            "pending"
        );
        let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap();
        assert_eq!(
            result.status,
            notryx_protocol::WorkspaceSendStatus::Abandoned
        );
        assert_eq!(result.operation_id.as_deref(), Some(followup.as_str()));
        let confirmed = rig
            .runtime
            .0
            .lock()
            .await
            .store
            .confirmed_receipt_through(&rig.binding)
            .await
            .unwrap();
        assert_eq!(confirmed, rig.operation.operation_seq + 1);
        (rig, intent, followup)
    }

    pub(crate) async fn complete_pending(
        rig: &Rig,
        version: u64,
    ) -> Result<serde_json::Value, String> {
        let result = crate::conflict_snapshot_native::complete_pending_remote_choice_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            &rig.operation.operation_id,
            version,
        )
        .await?;
        Ok(serde_json::to_value(result).unwrap())
    }

    pub(crate) fn delayed_completion(
        rig: &Rig,
        version: u64,
        phase: &'static str,
        ready: Arc<Notify>,
        resume: Arc<Notify>,
    ) -> tokio::task::JoinHandle<bool> {
        let runtime = Runtime(rig.runtime.0.clone());
        let binding = rig.binding.clone();
        let document = rig.document.clone();
        let edit = rig.edit_session.clone();
        let operation = rig.operation.operation_id.clone();
        tokio::spawn(async move {
            crate::conflict_snapshot_native::complete_pending_remote_choice_for_renderer_with_hook(
                &runtime, OWNER, &binding, &document, &edit, &operation, version, phase, ready,
                resume,
            )
            .await
            .is_err()
        })
    }

    pub(crate) async fn assert_pending_versions(
        rig: &Rig,
        intent: &notryx_client_store::ContentConflictIntent,
        followup: &str,
        local: &[u8],
        status: notryx_client_store::OutboxStatus,
    ) {
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            Some(intent.clone())
        );
        let failed = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        let next = state
            .store
            .read_outbox_operation(&rig.binding, followup)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(failed.bytes, LOCAL);
        assert_eq!(failed.operation_seq, rig.operation.operation_seq);
        assert_eq!(failed.expected_revision, rig.operation.expected_revision);
        assert_eq!(next.status, status);
        assert_eq!(next.operation_seq, failed.operation_seq + 1);
        assert_eq!(next.bytes, b"newer local\n");
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, local);
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(
            draft.base_revision,
            rig.initial.content_rev.as_ref().unwrap().as_str()
        );
    }
}

pub(crate) mod continue_remote {
    pub(crate) use super::snapshot::*;

    pub(crate) async fn continue_rig() -> (Rig, notryx_client_store::ContentConflictIntent, String)
    {
        let rig = rig_with_followup(true).await;
        let request = notryx_client_store::ContentConflictIntentRequest {
            generation: 2,
            remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
            remote_hash: rig.first_peer.content_hash.clone().unwrap(),
            remote_bytes: REMOTE.to_vec(),
            choice: notryx_client_store::ContentConflictChoice::ChooseRemote,
        };
        let (intent, followup) = {
            let mut state = rig.runtime.0.lock().await;
            let intent = state
                .store
                .record_content_conflict_intent(
                    &rig.binding,
                    &rig.document,
                    &rig.operation.operation_id,
                    &request,
                )
                .await
                .unwrap();
            let followup = state
                .store
                .document_outbox_state(&rig.binding, &rig.document)
                .await
                .unwrap()
                .latest
                .unwrap()
                .operation_id;
            (intent, followup)
        };
        (rig, intent, followup)
    }

    pub(crate) async fn continue_choice(
        rig: &Rig,
        version: u64,
    ) -> Result<serde_json::Value, String> {
        let result = crate::conflict_snapshot_native::continue_remote_choice_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            &rig.operation.operation_id,
            version,
        )
        .await?;
        Ok(serde_json::to_value(result).unwrap())
    }

    pub(crate) fn delayed_continue(
        rig: &Rig,
        version: u64,
        ready: Arc<Notify>,
        resume: Arc<Notify>,
    ) -> tokio::task::JoinHandle<bool> {
        let runtime = Runtime(rig.runtime.0.clone());
        let binding = rig.binding.clone();
        let document = rig.document.clone();
        let edit = rig.edit_session.clone();
        let operation = rig.operation.operation_id.clone();
        tokio::spawn(async move {
            crate::conflict_snapshot_native::continue_remote_choice_for_renderer_with_hook(
                &runtime, OWNER, &binding, &document, &edit, &operation, version, ready, resume,
            )
            .await
            .is_err()
        })
    }
}

pub(crate) mod copy_confirm {
    pub(crate) use super::choose_remote::*;

    pub(crate) type CopyRequest = notryx_protocol::WorkspaceContentConflictCopyRequest;

    pub(crate) fn copy_request(rig: &Rig, version: Option<u64>) -> CopyRequest {
        CopyRequest {
            selection: choice_request(rig, 2, version),
            parent_id: rig.initial.parent_id.unwrap().to_string(),
            name: "my-copy.md".into(),
        }
    }

    pub(crate) async fn confirm_copy(
        rig: &Rig,
        request: CopyRequest,
    ) -> Result<serde_json::Value, String> {
        let result =
            crate::conflict_snapshot_native::confirm_copy_for_renderer(&rig.runtime, request)
                .await?;
        Ok(serde_json::to_value(result).unwrap())
    }

    pub(crate) fn spawn_copy(rig: &Rig, request: CopyRequest) -> tokio::task::JoinHandle<bool> {
        let runtime = Runtime(rig.runtime.0.clone());
        tokio::spawn(async move {
            crate::conflict_snapshot_native::confirm_copy_for_renderer(&runtime, request)
                .await
                .is_err()
        })
    }

    pub(crate) async fn assert_copy_confirm_preserves_sources(rig: &Rig) {
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"newer local\n");
        assert_eq!(draft.base_bytes, BASE);
        let failed = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(failed.bytes, LOCAL);
        let latest = state
            .store
            .document_outbox_state(&rig.binding, &rig.document)
            .await
            .unwrap()
            .latest
            .unwrap();
        let followup = state
            .store
            .read_outbox_operation(&rig.binding, &latest.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(followup.status, notryx_client_store::OutboxStatus::Queued);
        assert_eq!(
            followup.abandon_state,
            notryx_client_store::OutboxAbandonState::None
        );
        assert_eq!(followup.bytes, b"newer local\n");
        assert!(
            state
                .store
                .list_create_operations(&rig.binding, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        drop(state);
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
            REMOTE
        );
        assert!(!rig.config.workspace.notes_root.join("my-copy.md").exists());
    }
}

pub(crate) mod copy_progress {
    pub(crate) use super::copy_confirm::*;

    pub(crate) fn progress_request(
        rig: &Rig,
        version: &str,
    ) -> notryx_protocol::WorkspaceContentConflictProgressRequest {
        notryx_protocol::WorkspaceContentConflictProgressRequest {
            renderer_session_id: OWNER.into(),
            binding: crate::to_contract(rig.binding.clone()),
            document_id: rig.document.clone(),
            edit_session_id: rig.edit_session.clone(),
            operation_id: rig.operation.operation_id.clone(),
            expected_intent_version: version.into(),
        }
    }

    pub(crate) async fn progress_copy(
        rig: &Rig,
        request: notryx_protocol::WorkspaceContentConflictProgressRequest,
    ) -> Result<serde_json::Value, String> {
        let version = request.expected_intent_version.parse::<u64>().unwrap();
        let approved = rig
            .runtime
            .0
            .lock()
            .await
            .store
            .inspect_content_conflict_copy_followup(&rig.binding, &request.operation_id, version)
            .await
            .ok()
            .flatten()
            .map(|item| item.operation_id);
        let result = crate::conflict_snapshot_native::progress_copy_with_approval_for_renderer(
            &rig.runtime,
            notryx_protocol::WorkspaceContentConflictCopyProgressRequest {
                progress: request,
                approved_followup_operation_id: approved,
            },
        )
        .await?;
        Ok(serde_json::to_value(result).unwrap())
    }
}

pub(crate) mod copy_retry {
    pub(crate) use super::copy_progress::*;

    pub(crate) fn retry_request(
        rig: &Rig,
        version: &str,
        old_create: &str,
        name: &str,
    ) -> notryx_protocol::WorkspaceContentConflictCopyRetryRequest {
        notryx_protocol::WorkspaceContentConflictCopyRetryRequest {
            progress: progress_request(rig, version),
            expected_create_operation_id: old_create.into(),
            parent_id: rig.initial.parent_id.unwrap().to_string(),
            name: name.into(),
        }
    }

    pub(crate) async fn retry_copy(
        rig: &Rig,
        request: notryx_protocol::WorkspaceContentConflictCopyRetryRequest,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::to_value(
            crate::conflict_snapshot_native::retry_copy_for_renderer(&rig.runtime, request).await?,
        )
        .unwrap())
    }

    pub(crate) async fn failed_copy(rig: &Rig) -> serde_json::Value {
        let mut choice = copy_request(rig, None);
        choice.name = "example.md".into();
        let confirmed = confirm_copy(rig, choice).await.unwrap();
        let progress = progress_request(rig, confirmed["version"].as_str().unwrap());
        progress_copy(rig, progress.clone()).await.unwrap();
        assert_eq!(
            crate::send::send_one(rig.runtime.0.clone(), OWNER)
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSendStatus::Abandoned
        );
        progress_copy(rig, progress).await.unwrap();
        assert_eq!(
            crate::send::send_one(rig.runtime.0.clone(), OWNER)
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSendStatus::Failed
        );
        confirmed
    }
}

pub(crate) mod copy_status {
    pub(crate) use super::copy_progress::*;

    pub(crate) fn copy_status_request(
        rig: &Rig,
    ) -> notryx_protocol::WorkspaceConflictResolutionRequest {
        notryx_protocol::WorkspaceConflictResolutionRequest {
            renderer_session_id: OWNER.into(),
            binding: crate::to_contract(rig.binding.clone()),
            document_id: rig.document.clone(),
            operation_id: rig.operation.operation_id.clone(),
        }
    }

    pub(crate) async fn copy_status(
        rig: &Rig,
        request: notryx_protocol::WorkspaceConflictResolutionRequest,
    ) -> Result<serde_json::Value, String> {
        let result =
            crate::conflict_snapshot_native::copy_status_for_renderer(&rig.runtime, request)
                .await?;
        Ok(serde_json::to_value(result).unwrap())
    }
}

pub(crate) mod merge_confirm {
    pub(crate) use super::choose_remote::*;

    pub(crate) async fn confirm_merge(
        runtime: &Runtime,
        request: Request,
    ) -> Result<String, String> {
        match crate::conflict_snapshot_native::confirm_merge_for_renderer(runtime, request).await? {
            notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Pending { version } => {
                Ok(version)
            }
            notryx_protocol::WorkspaceContentConflictChooseRemoteResponse::Completed { .. } => {
                Err("unexpected_completed".into())
            }
        }
    }

    pub(crate) fn spawn_merge(
        rig: &Rig,
        request: Request,
    ) -> tokio::task::JoinHandle<Result<String, String>> {
        let runtime = Runtime(rig.runtime.0.clone());
        tokio::spawn(async move { confirm_merge(&runtime, request).await })
    }

    pub(crate) async fn assert_unmodified_sources(rig: &Rig, expected_local: &[u8], frozen: bool) {
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, expected_local);
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(
            draft.base_revision,
            rig.initial.content_rev.as_ref().unwrap().as_str()
        );
        let original = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.status, notryx_client_store::OutboxStatus::Conflict);
        assert_eq!(original.frozen, frozen);
        assert_eq!(original.bytes, LOCAL);
        assert_eq!(original.payload_hash, rig.operation.payload_hash);
        assert_eq!(original.expected_revision, rig.operation.expected_revision);
        let latest = state
            .store
            .document_outbox_state(&rig.binding, &rig.document)
            .await
            .unwrap()
            .latest
            .unwrap();
        if latest.operation_id != original.operation_id {
            let followup = state
                .store
                .read_outbox_operation(&rig.binding, &latest.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(followup.status, notryx_client_store::OutboxStatus::Queued);
            assert_eq!(
                followup.abandon_state,
                notryx_client_store::OutboxAbandonState::None
            );
            assert_eq!(followup.bytes, b"newer local\n");
        }
    }
}

pub(crate) mod merge_queue {
    pub(crate) use super::snapshot::*;

    pub(crate) use notryx_client_store::{
        ContentConflictChoice, ContentConflictIntent, ContentConflictIntentRequest,
        ContentConflictResolutionState, OutboxStatus,
    };
    pub(crate) use sqlx::Connection;

    pub(crate) const MERGED: &[u8] = b"\xef\xbb\xbfmerged \xed\x95\x9c\xea\xb8\x80\n";
    pub(crate) const LATER: &[u8] = b"\xef\xbb\xbfmerged \xed\x95\x9c\xea\xb8\x80\nlater edit\n";

    pub(crate) async fn queue_approved_merge(
        rig: &Rig,
        version: u64,
    ) -> Result<OutboxOperation, String> {
        crate::conflict_snapshot_native::queue_merge_for_renderer(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            &rig.operation.operation_id,
            version,
        )
        .await
    }

    pub(crate) async fn merged_intent(rig: &Rig) -> ContentConflictIntent {
        rig.runtime
            .0
            .lock()
            .await
            .store
            .save_registered_checkpoint(&rig.binding, &rig.document, &rig.edit_session, 2, MERGED)
            .await
            .unwrap();
        let shown = prepare_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.document,
            &rig.edit_session,
            2,
            &rig.operation.operation_id,
        )
        .await
        .unwrap();
        assert_eq!(shown.local_bytes, MERGED);
        rig.runtime
            .0
            .lock()
            .await
            .store
            .record_content_conflict_intent(
                &rig.binding,
                &rig.document,
                &rig.operation.operation_id,
                &ContentConflictIntentRequest {
                    generation: shown.generation,
                    remote_revision: shown.remote_revision,
                    remote_hash: rig.first_peer.content_hash.clone().unwrap(),
                    remote_bytes: REMOTE.to_vec(),
                    choice: ContentConflictChoice::SaveMerged {
                        bytes: MERGED.to_vec(),
                    },
                },
            )
            .await
            .unwrap()
    }

    pub(crate) async fn assert_merge_queue_sources(rig: &Rig) {
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, 2);
        assert_eq!(draft.bytes, MERGED);
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(
            draft.base_revision,
            rig.initial.content_rev.as_ref().unwrap().as_str()
        );
        let page = state.store.list_outbox(&rig.binding, 0, 100).await.unwrap();
        assert_eq!(page.entries.len(), 1);
        let original = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.status, OutboxStatus::Conflict);
        assert_eq!(original.bytes, LOCAL);
        assert_eq!(original.payload_hash, rig.operation.payload_hash);
        assert_eq!(original.expected_revision, rig.operation.expected_revision);
        assert_eq!(original.operation_seq, rig.operation.operation_seq);
    }

    pub(crate) async fn confirm_original_ack(rig: &Rig) {
        let ack = rig
            .session
            .acknowledge(rig.operation.operation_seq)
            .await
            .unwrap();
        rig.runtime
            .0
            .lock()
            .await
            .store
            .confirm_receipt_ack(&rig.binding, &ack)
            .await
            .unwrap();
    }
}

pub(crate) mod merge_recovery {
    pub(crate) use super::merge_queue::*;

    pub(crate) async fn read_queued_merge(
        rig: &Rig,
        owner: &str,
        binding: &RegistrationBinding,
        document_id: &str,
        operation_id: &str,
        version: u64,
    ) -> Result<Option<OutboxOperation>, String> {
        crate::conflict_snapshot_native::read_queued_merge_for_renderer(
            &rig.runtime,
            owner,
            binding,
            document_id,
            operation_id,
            version,
        )
        .await
    }
}

pub(crate) mod snapshot {
    pub(crate) use crate::{
        ProductSession, ProductSessionKind, Runtime, RuntimeState,
        conflict_snapshot_native::prepare_conflict_snapshot,
        registration_binding,
        sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
    };
    pub(crate) use notryx_client_runtime::{ContentMutation, DeleteMutation, Session};
    pub(crate) use notryx_client_store::{
        ClientStore, OutboxConflict, OutboxOperation, RegistrationBinding,
    };
    pub(crate) use notryx_protocol::{NodeState, OperationReceipt};
    pub(crate) use notryx_server::{
        config::{Config, ServerConfig, WorkspaceConfig},
        http::RunningServer,
    };
    pub(crate) use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    pub(crate) use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Mutex, Notify},
    };
    pub(crate) use uuid::Uuid;

    pub(crate) const OWNER: &str = "owner";
    pub(crate) const BASE: &[u8] = b"server source\n";
    pub(crate) const LOCAL: &[u8] = b"local draft\n";
    pub(crate) const REMOTE: &[u8] = b"peer first\n";

    pub(crate) async fn content_proxy(
        target: std::net::SocketAddr,
    ) -> (
        std::net::SocketAddr,
        Arc<AtomicBool>,
        Arc<Notify>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let captured = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task = tokio::spawn({
            let (armed, captured, release) = (armed.clone(), captured.clone(), release.clone());
            async move {
                loop {
                    let (mut incoming, _) = listener.accept().await.unwrap();
                    let (armed, captured, release) =
                        (armed.clone(), captured.clone(), release.clone());
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut buffer = [0u8; 4096];
                        let end = loop {
                            let size = incoming.read(&mut buffer).await.unwrap();
                            if size == 0 {
                                return;
                            }
                            request.extend_from_slice(&buffer[..size]);
                            if let Some(index) =
                                request.windows(4).position(|part| part == b"\r\n\r\n")
                            {
                                break index;
                            }
                            assert!(request.len() < 65536);
                        };
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        let gate = headers.starts_with("GET /v1/nodes/")
                            && headers.lines().next().unwrap().contains("/content ")
                            && armed.swap(false, Ordering::SeqCst);
                        let mut forwarded = headers
                            .lines()
                            .filter(|line| !line.to_ascii_lowercase().starts_with("connection:"))
                            .map(|line| format!("{line}\r\n"))
                            .collect::<String>()
                            .into_bytes();
                        forwarded.extend_from_slice(b"Connection: close\r\n\r\n");
                        forwarded.extend_from_slice(&request[end + 4..]);
                        let mut server = TcpStream::connect(target).await.unwrap();
                        server.write_all(&forwarded).await.unwrap();
                        if gate {
                            let mut response = Vec::new();
                            server.read_to_end(&mut response).await.unwrap();
                            captured.notify_one();
                            release.notified().await;
                            let _ = incoming.write_all(&response).await;
                        } else {
                            let _ = tokio::io::copy_bidirectional(&mut incoming, &mut server).await;
                        }
                    });
                }
            }
        });
        (address, armed, captured, release, task)
    }

    pub(crate) struct Rig {
        pub(crate) _directory: tempfile::TempDir,
        pub(crate) server: RunningServer,
        pub(crate) runtime: Runtime,
        pub(crate) session: Arc<Session>,
        pub(crate) peer: Session,
        pub(crate) binding: RegistrationBinding,
        pub(crate) document: String,
        pub(crate) edit_session: String,
        pub(crate) initial: NodeState,
        pub(crate) first_peer: NodeState,
        pub(crate) operation: OutboxOperation,
        pub(crate) armed: Arc<AtomicBool>,
        pub(crate) captured: Arc<Notify>,
        pub(crate) release: Arc<Notify>,
        pub(crate) proxy: tokio::task::JoinHandle<()>,
        pub(crate) config: Config,
    }

    pub(crate) async fn rig() -> Rig {
        rig_with_followup(false).await
    }

    pub(crate) async fn rig_with_followup(followup: bool) -> Rig {
        let (directory, server, _unused_runtime, original, document) = setup().await;
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
        let initial = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        let (address, armed, captured, release, proxy) = content_proxy(server.local_addr()).await;
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
        let pairing = server_test_support::pairing_request(&config).await.0;
        let peer = Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap();
        let binding = registration_binding(session.binding());
        let mut store = ClientStore::open(directory.path().join("conflict-snapshot.sqlite"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let opened = store
            .open_registered_document(
                &binding,
                &document,
                BASE,
                initial.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, &document, &opened.edit_session_id, 1, LOCAL)
            .await
            .unwrap();
        store
            .queue_checkpoint(&binding, &document, 1)
            .await
            .unwrap();
        let mut state = RuntimeState::new(store, OWNER.into());
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
        let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let peer_result = peer
            .put_content(&ContentMutation {
                binding: peer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: initial.node_id,
                expected_revision: initial.content_rev.clone().unwrap(),
                bytes: REMOTE.to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(peer_result, OperationReceipt::Committed { .. }));
        let operation = runtime
            .0
            .lock()
            .await
            .store
            .prepare_outbox(&binding)
            .await
            .unwrap()
            .unwrap();
        if followup {
            let mut state = runtime.0.lock().await;
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    2,
                    b"newer local\n",
                )
                .await
                .unwrap();
            state
                .store
                .queue_checkpoint(&binding, &document, 2)
                .await
                .unwrap();
        }
        let receipt = session
            .put_content(&ContentMutation {
                binding: session.binding().clone(),
                operation_id: operation.operation_id.parse().unwrap(),
                operation_seq: operation.operation_seq,
                node_id: initial.node_id,
                expected_revision: notryx_domain::ContentRevision::new(
                    operation.expected_revision.clone().unwrap(),
                )
                .unwrap(),
                bytes: operation.bytes.clone(),
            })
            .await
            .unwrap();
        let OperationReceipt::PreconditionFailed {
            current_content_rev,
            current_content_etag,
            ..
        } = receipt
        else {
            panic!("expected 412")
        };
        runtime
            .0
            .lock()
            .await
            .store
            .apply_outbox_conflict(
                &binding,
                &OutboxConflict {
                    operation_id: operation.operation_id.clone(),
                    operation_seq: operation.operation_seq,
                    workspace_epoch: binding.workspace_epoch.clone(),
                    current_revision: current_content_rev.as_str().into(),
                    current_etag: current_content_etag,
                },
            )
            .await
            .unwrap();
        let snapshot = runtime.0.lock().await.gate.snapshot(OWNER).unwrap();
        crate::sync_catchup::catch_up_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let first_peer = runtime
            .0
            .lock()
            .await
            .store
            .current_remote_file(&binding, initial.node_id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first_peer.content_rev, initial.content_rev);
        Rig {
            _directory: directory,
            server,
            runtime,
            session,
            peer,
            binding,
            document,
            edit_session: opened.edit_session_id,
            initial,
            first_peer,
            operation,
            armed,
            captured,
            release,
            proxy,
            config,
        }
    }

    pub(crate) async fn assert_preserved(rig: &Rig) {
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, LOCAL);
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(
            draft.base_revision,
            rig.initial.content_rev.as_ref().unwrap().as_str()
        );
        let page = state.store.list_outbox(&rig.binding, 0, 100).await.unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation_id, rig.operation.operation_id);
        assert_eq!(
            page.entries[0].status,
            notryx_client_store::OutboxStatus::Conflict
        );
        let saved = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.bytes, rig.operation.bytes);
        assert_eq!(saved.payload_hash, rig.operation.payload_hash);
        assert_eq!(saved.expected_revision, rig.operation.expected_revision);
        assert_eq!(saved.operation_seq, rig.operation.operation_seq);
    }

    pub(crate) async fn prepare(
        rig: &Rig,
    ) -> Result<crate::conflict_snapshot_native::ConflictSnapshot, String> {
        prepare_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.document,
            &rig.edit_session,
            1,
            &rig.operation.operation_id,
        )
        .await
    }

    pub(crate) async fn write_external_and_observe(rig: &Rig, bytes: &[u8]) -> NodeState {
        let path = rig.config.workspace.notes_root.join("example.md");
        std::fs::write(&path, bytes).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let inventory = rig.peer.inventory(None).await.unwrap();
                if let Some(node) = inventory.nodes.into_iter().find(|node| {
                    node.node_id == rig.initial.node_id
                        && node.content_rev != rig.first_peer.content_rev
                        && node.size.as_deref() == Some(bytes.len().to_string().as_str())
                }) {
                    return node;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("watcher did not publish the external synthetic document")
    }

    pub(crate) async fn prepare_after_external_change(
        rig: &Rig,
    ) -> Result<crate::conflict_snapshot_native::ConflictSnapshot, String> {
        // Inventory can publish the new revision before a concurrent object recheck finishes.
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                match prepare(rig).await {
                    Err(error) if error == "server_not_ready" => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    result => return result,
                }
            }
        })
        .await
        .expect("server did not settle after the external synthetic change")
    }

    pub(crate) async fn cache_observation(rig: &Rig) -> (i64, i64) {
        use sqlx::Connection;
        let path = rig._directory.path().join("conflict-snapshot.sqlite");
        let mut db = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .read_only(true),
        )
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cached_content WHERE registration_id=1 AND node_id=?",
        )
        .bind(&rig.document)
        .fetch_one(&mut db)
        .await
        .unwrap();
        let ready: i64 = sqlx::query_scalar(
            "SELECT content_ready FROM remote_nodes WHERE registration_id=1 AND node_id=?",
        )
        .bind(&rig.document)
        .fetch_one(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
        (count, ready)
    }
}

pub(crate) mod send_copy_content {
    pub(crate) use super::send_copy::*;

    pub(crate) async fn created_copy(
        rig: &Rig,
    ) -> (
        notryx_client_store::ContentConflictCopyIntent,
        notryx_client_store::CreateOutboxOperation,
        NodeState,
    ) {
        let (copy, create) = queue_confirmed_copy(rig, "copy.md").await;
        advance_followup(rig).await;
        let result = crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap();
        assert_eq!(result.status, WorkspaceSendStatus::Committed);
        let mut state = rig.runtime.0.lock().await;
        let entries = state
            .store
            .list_create_operations(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries;
        let Some(OperationReceipt::Created { node, .. }) = entries[0].receipt.clone() else {
            panic!("expected created receipt")
        };
        (copy, create, node)
    }
}

pub(crate) mod send_copy_history {
    pub(crate) use super::send_copy_status::*;

    pub(crate) async fn saved_copy_and_later_edit() -> (
        Rig,
        notryx_client_store::ContentConflictCopyIntent,
        OutboxOperation,
        OutboxOperation,
    ) {
        let rig = rig_with_followup(true).await;
        let (copy, _, _) = created_copy(&rig).await;
        let first = rig
            .runtime
            .0
            .lock()
            .await
            .store
            .queue_content_conflict_copy_content(
                &rig.binding,
                &rig.operation.operation_id,
                copy.intent_version,
            )
            .await
            .unwrap();
        assert_eq!(
            crate::send::send_one(rig.runtime.0.clone(), OWNER)
                .await
                .unwrap()
                .status,
            WorkspaceSendStatus::Committed
        );
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &copy.node_id)
            .await
            .unwrap()
            .unwrap();
        let opened = state
            .store
            .open_registered_document(
                &rig.binding,
                &copy.node_id,
                &draft.base_bytes,
                &draft.base_revision,
            )
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &copy.node_id,
                &opened.edit_session_id,
                2,
                b"later copy edit\n",
            )
            .await
            .unwrap();
        state
            .store
            .close_registered_document(&rig.binding, &copy.node_id, &opened.edit_session_id)
            .await
            .unwrap();
        let later = state
            .store
            .queue_checkpoint(&rig.binding, &copy.node_id, 2)
            .await
            .unwrap();
        drop(state);
        (rig, copy, first, later)
    }
}

pub(crate) mod send_copy_status {
    pub(crate) use super::send_copy_content::*;

    pub(crate) use notryx_client_store::ContentConflictCopyContentState;
    pub(crate) use sqlx::Connection;

    pub(crate) async fn status(
        rig: &Rig,
        version: u64,
    ) -> Result<ContentConflictCopyContentState, notryx_client_store::StoreError> {
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_content_conflict_copy_content_state(
                &rig.binding,
                &rig.operation.operation_id,
                version,
            )
            .await
    }
}

pub(crate) mod send_copy {
    pub(crate) use super::snapshot::*;

    pub(crate) use notryx_client_runtime::CreateMutation;
    pub(crate) use notryx_client_store::{
        ContentConflictChoice, ContentConflictIntentRequest, OutboxStatus, PendingOutboxOperation,
    };
    pub(crate) use notryx_protocol::WorkspaceSendStatus;

    pub(crate) async fn queue_confirmed_copy(
        rig: &Rig,
        name: &str,
    ) -> (
        notryx_client_store::ContentConflictCopyIntent,
        notryx_client_store::CreateOutboxOperation,
    ) {
        let request = ContentConflictIntentRequest {
            generation: 2,
            remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
            remote_hash: rig.first_peer.content_hash.clone().unwrap(),
            remote_bytes: REMOTE.to_vec(),
            choice: ContentConflictChoice::KeepLocalCopy,
        };
        let mut state = rig.runtime.0.lock().await;
        let copy = state
            .store
            .record_content_conflict_copy(
                &rig.binding,
                &rig.document,
                &rig.operation.operation_id,
                &request,
                &rig.initial.parent_id.unwrap().to_string(),
                name,
                None,
            )
            .await
            .unwrap();
        let queued = state
            .store
            .queue_content_conflict_copy(
                &rig.binding,
                &rig.operation.operation_id,
                copy.intent_version,
            )
            .await
            .unwrap();
        (copy, queued)
    }

    pub(crate) async fn preserve_original(
        rig: &Rig,
        copy: &notryx_client_store::ContentConflictCopyIntent,
    ) {
        let mut state = rig.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"newer local\n");
        assert_eq!(draft.base_bytes, BASE);
        let conflict = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(conflict.status, OutboxStatus::Conflict);
        assert_eq!(conflict.bytes, LOCAL);
        assert_eq!(
            state
                .store
                .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            Some(copy.clone())
        );
        assert!(matches!(
            state
                .store
                .read_content_conflict_resolution_state(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            notryx_client_store::ContentConflictResolutionState::Pending {
                choice: notryx_client_store::ContentConflictChoiceKind::KeepLocalCopy,
                ..
            }
        ));
        drop(state);
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
            REMOTE
        );
    }

    pub(crate) async fn advance_followup(rig: &Rig) {
        let blocked = crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap();
        assert_eq!(blocked.status, WorkspaceSendStatus::NeedsReview);
        assert!(!rig.config.workspace.notes_root.join("copy.md").exists());
        let later = rig
            .runtime
            .0
            .lock()
            .await
            .store
            .request_content_conflict_followup_abandon(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        let abandoned = crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap();
        assert_eq!(abandoned.status, WorkspaceSendStatus::Abandoned);
        assert_eq!(
            abandoned.operation_id.as_deref(),
            Some(later.operation_id.as_str())
        );
    }
}
