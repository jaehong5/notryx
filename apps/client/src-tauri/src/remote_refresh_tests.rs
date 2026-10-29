use crate::{
    ProductSession, ProductSessionKind, Runtime, registration_binding,
    remote_refresh_native::{begin_prepare, commit_candidate, finish_prepare},
    sync_bootstrap, sync_catchup, sync_content,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_client_runtime::{ContentMutation, Error as NetworkError, Session};
use notryx_client_store::{OutboxOperation, OutboxPage, PersistedDraft, RegistrationBinding};
use notryx_protocol::{
    M0CloseRequest, NodeState, WorkspaceRemoteRefreshCandidate,
    WorkspaceRemoteRefreshCommitRequest, WorkspaceRemoteRefreshRequest,
};
use notryx_server::{
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use std::sync::Arc;
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    server: RunningServer,
    runtime: Runtime,
    session: Arc<Session>,
    writer: Arc<Session>,
    binding: RegistrationBinding,
    node: NodeState,
    request: WorkspaceRemoteRefreshRequest,
    other_document: String,
    initial_other_draft: PersistedDraft,
    initial_outbox: OutboxPage,
    initial_operation: OutboxOperation,
    writer_seq: u64,
}

impl Fixture {
    async fn new() -> Self {
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
        let original = session.content(&node).await.unwrap();
        assert_eq!(original, b"move source\n");
        let binding = registration_binding(session.binding());
        let (request, initial_other_draft, initial_outbox, initial_operation) = {
            let mut state = runtime.0.lock().await;
            let opened = state
                .store
                .open_registered_document(
                    &binding,
                    &node.node_id.to_string(),
                    &original,
                    node.content_rev.as_ref().unwrap().as_str(),
                )
                .await
                .unwrap();
            state
                .gate
                .open_session(opened.edit_session_id.clone())
                .unwrap();
            state.product_sessions.insert(
                opened.edit_session_id.clone(),
                ProductSession {
                    document_id: node.node_id.to_string(),
                    binding: binding.clone(),
                    kind: ProductSessionKind::Active,
                },
            );
            let other = state
                .store
                .read_registered_checkpoint(&binding, &other_document)
                .await
                .unwrap()
                .unwrap();
            let outbox = state.store.list_outbox(&binding, 0, 100).await.unwrap();
            assert!(!outbox.entries.is_empty());
            let operation = state
                .store
                .read_outbox_operation(&binding, &outbox.entries[0].operation_id)
                .await
                .unwrap()
                .unwrap();
            (
                WorkspaceRemoteRefreshRequest {
                    renderer_session_id: "owner".into(),
                    document_id: node.node_id.to_string(),
                    edit_session_id: opened.edit_session_id,
                },
                other,
                outbox,
                operation,
            )
        };
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
        let pairing = sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let writer = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let mut fixture = Self {
            _directory: directory,
            server,
            runtime,
            session,
            writer,
            binding,
            node,
            request,
            other_document,
            initial_other_draft,
            initial_outbox,
            initial_operation,
            writer_seq: 0,
        };
        fixture.remote_edit(b"remote source\n").await;
        fixture.catch_up().await;
        fixture
    }

    async fn remote_edit(&mut self, bytes: &[u8]) {
        self.writer_seq += 1;
        let current = self
            .writer
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == self.node.node_id)
            .unwrap();
        self.writer
            .put_content(&ContentMutation {
                binding: self.writer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: self.writer_seq,
                node_id: current.node_id,
                expected_revision: current.content_rev.unwrap(),
                bytes: bytes.to_vec(),
            })
            .await
            .unwrap();
    }

    async fn catch_up(&self) {
        let snapshot = self.runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_catchup::catch_up_metadata(&self.runtime, &snapshot, &self.session)
            .await
            .unwrap();
    }

    async fn candidate(&self) -> WorkspaceRemoteRefreshCandidate {
        let preparation = begin_prepare(&self.runtime, &self.request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sync_content::download_and_cache(
                &self.runtime,
                &preparation.session,
                &preparation.node
            )
            .await,
            Ok(true)
        );
        finish_prepare(&self.runtime, &self.request, preparation)
            .await
            .unwrap()
            .unwrap()
    }

    fn commit(
        &self,
        candidate: &WorkspaceRemoteRefreshCandidate,
    ) -> WorkspaceRemoteRefreshCommitRequest {
        WorkspaceRemoteRefreshCommitRequest {
            renderer_session_id: self.request.renderer_session_id.clone(),
            document_id: self.request.document_id.clone(),
            edit_session_id: self.request.edit_session_id.clone(),
            token: candidate.token.clone(),
        }
    }

    async fn assert_preserved(&self, draft: Option<PersistedDraft>) {
        let mut state = self.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&self.binding, &self.request.document_id)
                .await
                .unwrap(),
            draft
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&self.binding, &self.other_document)
                .await
                .unwrap()
                .as_ref(),
            Some(&self.initial_other_draft)
        );
        assert_eq!(
            state
                .store
                .list_outbox(&self.binding, 0, 100)
                .await
                .unwrap(),
            self.initial_outbox
        );
        let operation = state
            .store
            .read_outbox_operation(&self.binding, &self.initial_operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(operation == self.initial_operation);
    }

    async fn assert_live_base_unchanged(&self) {
        let mut state = self.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &self.binding,
                &self.request.document_id,
                &self.request.edit_session_id,
                1,
                b"local after rejection\n",
            )
            .await
            .unwrap();
        let draft = state
            .store
            .read_registered_checkpoint(&self.binding, &self.request.document_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local after rejection\n");
        assert_eq!(draft.base_bytes, b"move source\n");
        assert_eq!(
            draft.base_revision,
            self.node.content_rev.as_ref().unwrap().as_str()
        );
        assert_eq!(
            state
                .store
                .list_outbox(&self.binding, 0, 100)
                .await
                .unwrap(),
            self.initial_outbox
        );
    }
}

#[tokio::test]
async fn pristine_remote_commit_is_single_use_and_next_checkpoint_uses_new_base() {
    let fixture = Fixture::new().await;
    let candidate = fixture.candidate().await;
    assert_eq!(
        STANDARD.decode(&candidate.bytes_base64).unwrap(),
        b"remote source\n"
    );
    let request = fixture.commit(&candidate);
    assert_eq!(
        commit_candidate(&fixture.runtime, request.clone()).await,
        Ok(true)
    );
    assert_eq!(commit_candidate(&fixture.runtime, request).await, Ok(false));
    fixture.assert_preserved(None).await;
    let expected_revision = fixture
        .writer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == fixture.node.node_id)
        .unwrap()
        .content_rev
        .unwrap();
    let mut state = fixture.runtime.0.lock().await;
    state
        .store
        .save_registered_checkpoint(
            &fixture.binding,
            &fixture.request.document_id,
            &fixture.request.edit_session_id,
            1,
            b"local after refresh\n",
        )
        .await
        .unwrap();
    let draft = state
        .store
        .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local after refresh\n");
    assert_eq!(draft.base_bytes, b"remote source\n");
    assert_eq!(draft.base_revision, expected_revision.as_str());
    assert_eq!(
        state
            .store
            .list_outbox(&fixture.binding, 0, 100)
            .await
            .unwrap(),
        fixture.initial_outbox
    );
    drop(state);
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn candidate_is_rejected_after_owner_or_edit_session_changes() {
    let fixture = Fixture::new().await;
    let candidate = fixture.candidate().await;
    fixture
        .runtime
        .0
        .lock()
        .await
        .gate
        .replace_owner("replacement-owner".into());
    assert!(
        commit_candidate(&fixture.runtime, fixture.commit(&candidate))
            .await
            .is_err()
    );
    fixture.assert_preserved(None).await;
    fixture.assert_live_base_unchanged().await;
    fixture.server.shutdown().await.unwrap();

    let fixture = Fixture::new().await;
    let candidate = fixture.candidate().await;
    fixture
        .runtime
        .0
        .lock()
        .await
        .close_local_document(&M0CloseRequest {
            document_id: fixture.request.document_id.clone(),
            edit_session_id: fixture.request.edit_session_id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        commit_candidate(&fixture.runtime, fixture.commit(&candidate)).await,
        Ok(false)
    );
    fixture.assert_preserved(None).await;
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_candidate_cannot_mutate_after_registration_is_replaced() {
    let fixture = Fixture::new().await;
    let candidate = fixture.candidate().await;
    {
        let mut state = fixture.runtime.0.lock().await;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state
            .store
            .activate_registration(&registration_binding(fixture.writer.binding()))
            .await
            .unwrap();
        state.session = Some(fixture.writer.clone());
    }
    assert_eq!(
        commit_candidate(&fixture.runtime, fixture.commit(&candidate)).await,
        Ok(false)
    );
    let mut state = fixture.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.other_document)
            .await
            .unwrap(),
        Some(fixture.initial_other_draft)
    );
    let old_outbox = state
        .store
        .list_outbox(&fixture.binding, 0, 100)
        .await
        .unwrap();
    assert_eq!(
        old_outbox.entries.len(),
        fixture.initial_outbox.entries.len()
    );
    assert!(old_outbox.entries.iter().all(|entry| entry.frozen));
    let old_operation = state
        .store
        .read_outbox_operation(&fixture.binding, &fixture.initial_operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    let mut expected = fixture.initial_operation.clone();
    expected.frozen = true;
    assert!(old_operation == expected);
    drop(state);
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_metadata_and_replaced_cache_reject_prepared_candidate() {
    let mut fixture = Fixture::new().await;
    let candidate = fixture.candidate().await;
    fixture.remote_edit(b"newer source\n").await;
    fixture.catch_up().await;
    let newer = fixture
        .session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == fixture.node.node_id)
        .unwrap();
    assert_eq!(
        sync_content::download_and_cache(&fixture.runtime, &fixture.session, &newer).await,
        Ok(true)
    );
    assert_eq!(
        commit_candidate(&fixture.runtime, fixture.commit(&candidate)).await,
        Ok(false)
    );
    fixture.assert_preserved(None).await;
    fixture.assert_live_base_unchanged().await;
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn checkpoint_created_during_fetch_blocks_candidate_and_preserves_original_base() {
    let fixture = Fixture::new().await;
    let preparation = begin_prepare(&fixture.runtime, &fixture.request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        sync_content::download_and_cache(&fixture.runtime, &preparation.session, &preparation.node)
            .await,
        Ok(true)
    );
    let draft = {
        let mut state = fixture.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &fixture.binding,
                &fixture.request.document_id,
                &fixture.request.edit_session_id,
                1,
                b"local concurrent edit\n",
            )
            .await
            .unwrap();
        state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(draft.bytes, b"local concurrent edit\n");
    assert_eq!(draft.base_bytes, b"move source\n");
    assert_eq!(
        draft.base_revision,
        fixture.node.content_rev.as_ref().unwrap().as_str()
    );
    assert!(
        finish_prepare(&fixture.runtime, &fixture.request, preparation)
            .await
            .unwrap()
            .is_none()
    );
    fixture.assert_preserved(Some(draft)).await;
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn checkpoint_created_after_candidate_blocks_commit_and_keeps_draft() {
    let fixture = Fixture::new().await;
    let candidate = fixture.candidate().await;
    let draft = {
        let mut state = fixture.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &fixture.binding,
                &fixture.request.document_id,
                &fixture.request.edit_session_id,
                1,
                b"saved while ready\n",
            )
            .await
            .unwrap();
        state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(draft.bytes, b"saved while ready\n");
    assert_eq!(draft.base_bytes, b"move source\n");
    assert_eq!(
        draft.base_revision,
        fixture.node.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(
        commit_candidate(&fixture.runtime, fixture.commit(&candidate)).await,
        Ok(false)
    );
    fixture.assert_preserved(Some(draft)).await;
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_conditional_fetch_rejects_server_revision_changed_since_prepare() {
    let mut fixture = Fixture::new().await;
    let preparation = begin_prepare(&fixture.runtime, &fixture.request)
        .await
        .unwrap()
        .unwrap();
    fixture.remote_edit(b"newer while fetching\n").await;
    assert_eq!(
        sync_content::download_and_cache(&fixture.runtime, &preparation.session, &preparation.node)
            .await,
        Err(sync_content::ContentFetchError::Network(
            NetworkError::Changed
        ))
    );
    fixture.assert_preserved(None).await;
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirmed_local_save_reopens_peer_change_and_next_edit_uses_new_base() {
    let mut fixture = Fixture::new().await;
    let candidate = fixture.candidate().await;
    assert!(
        commit_candidate(&fixture.runtime, fixture.commit(&candidate))
            .await
            .unwrap()
    );
    {
        let mut state = fixture.runtime.0.lock().await;
        state
            .store
            .save_registered_checkpoint(
                &fixture.binding,
                &fixture.request.document_id,
                &fixture.request.edit_session_id,
                1,
                b"saved on this device\n",
            )
            .await
            .unwrap();
        state
            .store
            .queue_checkpoint(&fixture.binding, &fixture.request.document_id, 1)
            .await
            .unwrap();
    }
    for _ in 0..4 {
        crate::send::send_one(fixture.runtime.0.clone(), "owner")
            .await
            .unwrap();
    }
    let preserved = fixture
        .runtime
        .0
        .lock()
        .await
        .store
        .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preserved.bytes, b"saved on this device\n");
    assert_eq!(preserved.bytes, preserved.base_bytes);
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
    let pairing = sync_bootstrap::server_test_support::pairing_request(&config)
        .await
        .0;
    let peer_session = Arc::new(
        Session::pair(
            &format!("http://{}", fixture.server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let peer_binding = registration_binding(peer_session.binding());
    let mut peer_store =
        notryx_client_store::ClientStore::open(fixture._directory.path().join("peer.sqlite"))
            .await
            .unwrap();
    peer_store
        .activate_registration(&peer_binding)
        .await
        .unwrap();
    let mut peer_state = crate::RuntimeState::new(peer_store, "peer-owner".into());
    peer_state.session = Some(peer_session.clone());
    let peer_runtime = Runtime(Arc::new(tokio::sync::Mutex::new(peer_state)));
    let snapshot = peer_runtime
        .0
        .lock()
        .await
        .gate
        .snapshot("peer-owner")
        .unwrap();
    sync_bootstrap::bootstrap_metadata(&peer_runtime, &snapshot, &peer_session)
        .await
        .unwrap();
    {
        let current = peer_session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id == fixture.node.node_id)
            .unwrap();
        let bytes = peer_session.content(&current).await.unwrap();
        let mut state = peer_runtime.0.lock().await;
        let opened = state
            .store
            .open_registered_document(
                &peer_binding,
                &fixture.request.document_id,
                &bytes,
                current.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(
                &peer_binding,
                &fixture.request.document_id,
                &opened.edit_session_id,
                1,
                b"saved on peer\n",
            )
            .await
            .unwrap();
        state
            .store
            .queue_checkpoint(&peer_binding, &fixture.request.document_id, 1)
            .await
            .unwrap();
    }
    let sent = crate::send::send_one(peer_runtime.0.clone(), "peer-owner")
        .await
        .unwrap();
    assert_eq!(sent.status, notryx_protocol::WorkspaceSendStatus::Committed);
    crate::send::send_one(peer_runtime.0.clone(), "peer-owner")
        .await
        .unwrap();
    fixture.catch_up().await;
    let candidate = fixture.candidate().await;
    assert_eq!(
        STANDARD.decode(&candidate.bytes_base64).unwrap(),
        b"saved on peer\n"
    );
    assert!(
        commit_candidate(&fixture.runtime, fixture.commit(&candidate))
            .await
            .unwrap()
    );
    let current_node = fixture
        .writer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id == fixture.node.node_id)
        .unwrap();
    for online in [true, false] {
        let outbox = {
            let mut state = fixture.runtime.0.lock().await;
            state
                .close_local_document(&M0CloseRequest {
                    document_id: fixture.request.document_id.clone(),
                    edit_session_id: fixture.request.edit_session_id.clone(),
                })
                .await
                .unwrap();
            if !online {
                state.session = None;
            }
            state
                .store
                .list_outbox(&fixture.binding, 0, 100)
                .await
                .unwrap()
        };
        let crate::WorkspaceOpenStart::Draft(opened) = crate::begin_workspace_open(
            &fixture.runtime,
            &notryx_protocol::WorkspaceOpenRequest {
                renderer_session_id: "owner".into(),
                node: current_node.clone(),
            },
            None,
        )
        .await
        .unwrap() else {
            panic!("confirmed cached content must open locally");
        };
        assert!(!opened.readonly, "confirmed content must remain editable");
        assert!(!opened.local_draft);
        assert_eq!(opened.has_session, online);
        assert_eq!(opened.generation, "1");
        assert!(opened.open_notice.is_none());
        assert_eq!(
            STANDARD.decode(&opened.bytes_base64).unwrap(),
            b"saved on peer\n"
        );
        fixture.request.edit_session_id = opened.edit_session_id;
        let mut state = fixture.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .list_outbox(&fixture.binding, 0, 100)
                .await
                .unwrap(),
            outbox
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
                .await
                .unwrap()
                .as_ref(),
            Some(&preserved)
        );
        state.session = Some(fixture.session.clone());
    }
    {
        let mut state = fixture.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
                .await
                .unwrap(),
            Some(preserved)
        );
        state
            .store
            .save_registered_checkpoint(
                &fixture.binding,
                &fixture.request.document_id,
                &fixture.request.edit_session_id,
                2,
                b"peer plus next local edit\n",
            )
            .await
            .unwrap();
        let draft = state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.request.document_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.base_bytes, b"saved on peer\n");
        state
            .store
            .queue_checkpoint(&fixture.binding, &fixture.request.document_id, 2)
            .await
            .unwrap();
    }
    let result = crate::send::send_one(fixture.runtime.0.clone(), "owner")
        .await
        .unwrap();
    assert_eq!(
        result.status,
        notryx_protocol::WorkspaceSendStatus::Committed
    );
    let node = fixture
        .writer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|n| n.node_id == fixture.node.node_id)
        .unwrap();
    assert_eq!(
        fixture.writer.content(&node).await.unwrap(),
        b"peer plus next local edit\n"
    );
    fixture.server.shutdown().await.unwrap();
}
