async fn prepare_move_sender(
    rig: &RestoreSenderRig,
) -> (Arc<Session>, notryx_client_store::MoveOutboxOperation) {
    let session = rig.state.lock().await.session.clone().unwrap();
    let runtime = crate::Runtime(rig.state.clone());
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let node = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "Other.md")
        .unwrap();
    let request = notryx_protocol::MoveNodeRequest {
        expected_parent_id: node.parent_id.unwrap(),
        parent_id: node.parent_id.unwrap(),
        name: "Moved.md".into(),
    };
    let item = rig
        .state
        .lock()
        .await
        .store
        .queue_move(&rig.binding, node.node_id, &node.meta_etag, &request)
        .await
        .unwrap();
    (session, item)
}

fn move_request_for(
    session: &Session,
    item: &notryx_client_store::MoveOutboxOperation,
) -> notryx_client_runtime::MoveMutation {
    notryx_client_runtime::MoveMutation {
        binding: session.binding().clone(),
        operation_id: item.operation_id.parse().unwrap(),
        operation_seq: item.operation_seq,
        node_id: item.node_id,
        expected_meta_etag: item.expected_meta_etag.clone(),
        request: item.request.clone(),
    }
}

#[tokio::test]
async fn move_sender_commits_real_file_and_preserves_dirty_checkpoint() {
    let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::Committed);
    let (session, item) = prepare_move_sender(&rig).await;
    let id = item.node_id.to_string();
    let revision = session.inventory(None).await.unwrap().nodes.into_iter()
        .find(|node| node.node_id == item.node_id).unwrap().content_rev.unwrap();
    {
        let mut state = rig.state.lock().await;
        let edit = state.store.open_registered_document(&rig.binding, &id, b"other base\n", revision.as_str()).await.unwrap();
        state.store.save_registered_checkpoint(&rig.binding, &id, &edit.edit_session_id, 1, b"unsent draft\n").await.unwrap();
        state.store.close_registered_document(&rig.binding, &id, &edit.edit_session_id).await.unwrap();
    }
    let result = send_one(rig.state.clone(), "owner").await.unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Committed);
    assert_eq!(result.operation_id.as_deref(), Some(item.operation_id.as_str()));
    assert!(!rig.notes.join("Other.md").exists());
    assert_eq!(std::fs::read(rig.notes.join("Moved.md")).unwrap(), b"other base\n");
    let moved = session.inventory(None).await.unwrap().nodes.into_iter().find(|node| node.node_id == item.node_id).unwrap();
    assert_eq!(moved.name, "Moved.md");
    assert_eq!(moved.content_rev.unwrap(), revision);
    let mut state = rig.state.lock().await;
    let draft = state.store.read_registered_checkpoint(&rig.binding, &id).await.unwrap().unwrap();
    assert_eq!(draft.bytes, b"unsent draft\n");
    assert_eq!(draft.base_bytes, b"other base\n");
    assert_eq!(draft.base_revision, revision.as_str());
    assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), item.operation_seq);
    assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), item.operation_seq);
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_sender_recovers_lost_response_and_acks_original_receipt() {
    let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::Committed);
    let (session, item) = prepare_move_sender(&rig).await;
    {
        let mut state = rig.state.lock().await;
        assert!(matches!(state.store.prepare_pending_outbox(&rig.binding).await.unwrap(), Some(PendingOutboxOperation::Move(_))));
    }
    let request = move_request_for(&session, &item);
    let receipt = session.move_node(&request).await.unwrap();
    assert!(matches!(receipt, OperationReceipt::Moved { .. }));
    let head = session.server_info().await.unwrap().head_seq;
    let result = send_one(rig.state.clone(), "owner").await.unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Committed);
    assert_eq!(result.operation_id.as_deref(), Some(item.operation_id.as_str()));
    assert_eq!(session.server_info().await.unwrap().head_seq, head);
    assert_eq!(std::fs::read(rig.notes.join("Moved.md")).unwrap(), b"other base\n");
    let mut state = rig.state.lock().await;
    assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), item.operation_seq);
    assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), item.operation_seq);
    assert!(state.store.next_pending_outbox(&rig.binding).await.unwrap().is_none());
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_sender_blocks_retired_receipt_without_replaying() {
    let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::Committed);
    let (session, item) = prepare_move_sender(&rig).await;
    rig.state.lock().await.store.prepare_pending_outbox(&rig.binding).await.unwrap();
    let request = move_request_for(&session, &item);
    assert!(matches!(session.move_node(&request).await.unwrap(), OperationReceipt::Moved { .. }));
    session.acknowledge(item.operation_seq).await.unwrap();
    assert_eq!(session.move_operation(&request).await.unwrap(), OperationLookup::Retired);
    let head = session.server_info().await.unwrap().head_seq;
    let (snapshot, generation) = {
        let mut state = rig.state.lock().await;
        let snapshot = state.gate.begin_send("owner").unwrap();
        let generation = state.transmission_gate.response_generation(&rig.binding);
        (snapshot, generation)
    };
    let result = send_move(&rig.state, &session, &snapshot, &rig.binding, generation, item.clone(), None, None, None).await.unwrap();
    rig.state.lock().await.gate.finish_send(&snapshot).unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::NeedsReview);
    let mut state = rig.state.lock().await;
    assert!(state.transmission_gate.has_block(&rig.binding, TransmissionBlock::UnverifiedResponse));
    assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
    assert!(matches!(state.store.next_pending_outbox(&rig.binding).await.unwrap(), Some(PendingOutboxOperation::Move(ref pending)) if pending.operation_id == item.operation_id && pending.status == OutboxStatus::Sent));
    drop(state);
    assert_eq!(session.server_info().await.unwrap().head_seq, head);
    assert_eq!(std::fs::read(rig.notes.join("Moved.md")).unwrap(), b"other base\n");
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_sender_reader_can_abandon_after_permission_denial() {
    let mut rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::Committed);
    let AdminResponse::Ok { result: AdminResult::Principal { principal_id } } = admin::request(&rig.config.server.admin_socket, &AdminRequest::CreatePrincipal { role: notryx_protocol::WorkspaceRole::Reader }).await.unwrap() else { panic!("reader") };
    let AdminResponse::Ok { result: AdminResult::Pairing { pairing_secret, .. } } = admin::request(&rig.config.server.admin_socket, &AdminRequest::IssuePairing { principal_id }).await.unwrap() else { panic!("pairing") };
    let session = Arc::new(Session::pair(&format!("http://{}", rig.server.local_addr()), rig.workspace_id, &pairing_secret).await.unwrap());
    let binding = registration_binding(session.binding());
    {
        let mut state = rig.state.lock().await;
        let pair = state.gate.begin_pair("owner").unwrap();
        state.store.activate_registration(&binding).await.unwrap();
        state.gate.commit_pair(&pair).unwrap();
        state.session = Some(session);
    }
    rig.binding = binding;
    let (session, item) = prepare_move_sender(&rig).await;
    let request = move_request_for(&session, &item);
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::PermissionDenied);
    assert_eq!(session.move_operation(&request).await.unwrap(), OperationLookup::Missing);
    {
        let mut state = rig.state.lock().await;
        let Some(PendingOutboxOperation::Move(sent)) = state.store.next_pending_outbox(&rig.binding).await.unwrap() else { panic!("move") };
        assert_eq!(sent.operation_id, item.operation_id);
        assert!(state.store.request_move_abandon_matching(&rig.binding, &sent).await.unwrap());
    }
    let result = send_one(rig.state.clone(), "owner").await.unwrap();
    assert_eq!(result.status, WorkspaceSendStatus::Abandoned);
    assert_eq!(result.operation_id.as_deref(), Some(item.operation_id.as_str()));
    let mut state = rig.state.lock().await;
    assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), item.operation_seq);
    assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), item.operation_seq);
    assert!(state.store.next_pending_outbox(&rig.binding).await.unwrap().is_none());
    drop(state);
    assert_eq!(std::fs::read(rig.notes.join("Other.md")).unwrap(), b"other base\n");
    assert!(!rig.notes.join("Moved.md").exists());
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_sender_rechecks_abandon_after_missing_lookup() {
    let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::Committed);
    let (session, item) = prepare_move_sender(&rig).await;
    let request = move_request_for(&session, &item);
    let (ready, observed) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let snapshot = rig.state.lock().await.gate.begin_send("owner").unwrap();
    let state = rig.state.clone();
    let sending = tokio::spawn(async move {
        let permit = SendPermit { state: state.clone(), snapshot, active: true };
        let result = send_inner(&state, &session, &snapshot, None, Some((ready, resumed)), None).await;
        permit.finish().await;
        result
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), observed).await.unwrap().unwrap();
    {
        let mut state = rig.state.lock().await;
        let Some(PendingOutboxOperation::Move(queued)) = state.store.next_pending_outbox(&rig.binding).await.unwrap() else { panic!("move") };
        assert!(state.store.request_move_abandon_matching(&rig.binding, &queued).await.unwrap());
    }
    resume.send(()).unwrap();
    assert_eq!(sending.await.unwrap().unwrap().status, WorkspaceSendStatus::Pending);
    assert_eq!(std::fs::read(rig.notes.join("Other.md")).unwrap(), b"other base\n");
    assert!(!rig.notes.join("Moved.md").exists());
    let session = rig.state.lock().await.session.clone().unwrap();
    assert_eq!(session.move_operation(&request).await.unwrap(), OperationLookup::Missing);
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::Abandoned);
    let mut state = rig.state.lock().await;
    assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), item.operation_seq);
    drop(state);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn move_sender_re_registration_freezes_unapplied_receipt() {
    let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(send_one(rig.state.clone(), "owner").await.unwrap().status, WorkspaceSendStatus::Committed);
    let (session, item) = prepare_move_sender(&rig).await;
    let (ready, observed) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let snapshot = rig.state.lock().await.gate.begin_send("owner").unwrap();
    let state = rig.state.clone();
    let sending = tokio::spawn(async move {
        let permit = SendPermit { state: state.clone(), snapshot, active: true };
        let result = send_inner(&state, &session, &snapshot, None, None, Some((ready, resumed))).await;
        permit.finish().await;
        result
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), observed).await.unwrap().unwrap();
    {
        let mut state = rig.state.lock().await;
        assert!(matches!(state.gate.begin_pair("owner"), Err("transmission_in_progress")));
        assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
    }
    sending.abort();
    assert!(sending.await.unwrap_err().is_cancelled());
    drop(resume);
    wait_until_send_gate_idle(&rig.state).await;
    let AdminResponse::Ok { result: AdminResult::Pairing { pairing_secret, .. } } = admin::request(&rig.config.server.admin_socket, &AdminRequest::IssuePairing { principal_id: rig.owner_principal_id.clone() }).await.unwrap() else { panic!("pairing") };
    let new_session = Arc::new(Session::pair(&format!("http://{}", rig.server.local_addr()), rig.workspace_id, &pairing_secret).await.unwrap());
    let replacement = registration_binding(new_session.binding());
    {
        let mut state = rig.state.lock().await;
        let pair = state.gate.begin_pair("owner").unwrap();
        state.store.activate_registration(&replacement).await.unwrap();
        state.gate.commit_pair(&pair).unwrap();
        state.session = Some(new_session);
        assert!(matches!(state.store.next_pending_outbox(&rig.binding).await, Err(StoreError::StaleRegistration)));
        assert!(state.store.next_pending_outbox(&replacement).await.unwrap().is_none());
        assert_eq!(state.store.receipt_ack_through(&replacement).await.unwrap(), 0);
    }
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(rig._directory.path().join("client.sqlite3"))).await.unwrap();
    let (registration_status, status, receipt): (String, String, Option<String>) = sqlx::query_as("SELECT r.status, o.status, o.receipt_json FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id=?")
        .bind(&item.operation_id).fetch_one(&mut db).await.unwrap();
    assert_eq!(registration_status, "frozen");
    assert_eq!(status, "sent");
    assert!(receipt.is_none());
    db.close().await.unwrap();
    assert!(!rig.notes.join("Other.md").exists());
    assert_eq!(std::fs::read(rig.notes.join("Moved.md")).unwrap(), b"other base\n");
    rig.server.shutdown().await.unwrap();
}
