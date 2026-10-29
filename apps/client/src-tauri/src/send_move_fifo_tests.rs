#[tokio::test]
async fn move_sender_content_move_content_keeps_fifo_and_late_checkpoint() {
    let rig = restore_sender_rig(notryx_protocol::WorkspaceRole::Owner, false).await;
    assert_eq!(
        send_one(rig.state.clone(), "owner").await.unwrap().status,
        WorkspaceSendStatus::Committed
    );
    let session = rig.state.lock().await.session.clone().unwrap();
    let runtime = crate::Runtime(rig.state.clone());
    let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
        .await
        .unwrap();
    let original = session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "Other.md")
        .unwrap();
    let id = original.node_id.to_string();
    let original_revision = original.content_rev.clone().unwrap();
    let request = notryx_protocol::MoveNodeRequest {
        expected_parent_id: original.parent_id.unwrap(),
        parent_id: original.parent_id.unwrap(),
        name: "Moved.md".into(),
    };
    let (edit, first, movement, second) = {
        let mut state = rig.state.lock().await;
        let edit = state
            .store
            .open_registered_document(&rig.binding, &id, b"other base\n", original_revision.as_str())
            .await
            .unwrap();
        state.store.save_registered_checkpoint(&rig.binding, &id, &edit.edit_session_id, 1, b"first edit\n").await.unwrap();
        let first = state.store.queue_checkpoint(&rig.binding, &id, 1).await.unwrap();
        let movement = state.store.queue_move(&rig.binding, original.node_id, &original.meta_etag, &request).await.unwrap();
        state.store.save_registered_checkpoint(&rig.binding, &id, &edit.edit_session_id, 2, b"second edit\n").await.unwrap();
        let second = state.store.queue_checkpoint(&rig.binding, &id, 2).await.unwrap();
        (edit, first, movement, second)
    };
    assert_eq!((first.operation_seq, movement.operation_seq, second.operation_seq), (2, 3, 4));
    assert_ne!(first.operation_id, movement.operation_id);
    assert_ne!(movement.operation_id, second.operation_id);
    assert_ne!(first.operation_id, second.operation_id);

    let (ready, observed) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let snapshot = rig.state.lock().await.gate.begin_send("owner").unwrap();
    let state = rig.state.clone();
    let sending_session = session.clone();
    let sending = tokio::spawn(async move {
        let permit = SendPermit { state: state.clone(), snapshot, active: true };
        let result = send_inner(&state, &sending_session, &snapshot, None, None, Some((ready, resumed))).await;
        permit.finish().await;
        result
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), observed).await.unwrap().unwrap();
    assert_eq!(std::fs::read(rig.notes.join("Other.md")).unwrap(), b"first edit\n");
    assert!(!rig.notes.join("Moved.md").exists());
    let first_revision = session.inventory(None).await.unwrap().nodes.into_iter()
        .find(|node| node.node_id == original.node_id).unwrap().content_rev.unwrap();
    assert_ne!(first_revision, original_revision);
    {
        let mut state = rig.state.lock().await;
        assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
        state.store.save_registered_checkpoint(&rig.binding, &id, &edit.edit_session_id, 3, b"latest draft\n").await.unwrap();
    }
    resume.send(()).unwrap();
    let first_result = tokio::time::timeout(std::time::Duration::from_secs(10), sending).await.unwrap().unwrap().unwrap();
    assert_eq!(first_result.status, WorkspaceSendStatus::Committed);
    assert_eq!(first_result.operation_id.as_deref(), Some(first.operation_id.as_str()));
    {
        let mut state = rig.state.lock().await;
        assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), first.operation_seq);
        assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), first.operation_seq);
        let draft = state.store.read_registered_checkpoint(&rig.binding, &id).await.unwrap().unwrap();
        assert_eq!(draft.generation, 3);
        assert_eq!(draft.bytes, b"latest draft\n");
        assert_eq!(draft.base_bytes, b"first edit\n");
        assert_eq!(draft.base_revision, first_revision.as_str());
        assert!(matches!(state.store.next_pending_outbox(&rig.binding).await.unwrap(), Some(PendingOutboxOperation::Move(ref item)) if item.operation_id == movement.operation_id && item.operation_seq == movement.operation_seq));
    }
    let move_result = send_one(rig.state.clone(), "owner").await.unwrap();
    assert_eq!(move_result.status, WorkspaceSendStatus::Committed);
    assert_eq!(move_result.operation_id.as_deref(), Some(movement.operation_id.as_str()));
    assert!(!rig.notes.join("Other.md").exists());
    assert_eq!(std::fs::read(rig.notes.join("Moved.md")).unwrap(), b"first edit\n");
    let moved = session.inventory(None).await.unwrap().nodes.into_iter().find(|node| node.node_id == original.node_id).unwrap();
    assert_eq!(moved.name, "Moved.md");
    assert_eq!(moved.content_rev.as_ref(), Some(&first_revision));
    {
        let mut state = rig.state.lock().await;
        assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), movement.operation_seq);
        assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), movement.operation_seq);
        let Some(PendingOutboxOperation::Content(item)) = state.store.next_pending_outbox(&rig.binding).await.unwrap() else { panic!("second content must follow move") };
        assert_eq!(item.operation_id, second.operation_id);
        assert_eq!(item.operation_seq, second.operation_seq);
        assert_eq!(item.bytes, b"second edit\n");
        assert_eq!(item.expected_revision.as_deref(), Some(first_revision.as_str()));
    }
    let second_result = send_one(rig.state.clone(), "owner").await.unwrap();
    assert_eq!(second_result.status, WorkspaceSendStatus::Committed);
    assert_eq!(second_result.operation_id.as_deref(), Some(second.operation_id.as_str()));
    assert_eq!(std::fs::read(rig.notes.join("Moved.md")).unwrap(), b"second edit\n");
    let latest = session.inventory(None).await.unwrap().nodes.into_iter().find(|node| node.node_id == original.node_id).unwrap();
    assert_eq!(latest.name, "Moved.md");
    let second_revision = latest.content_rev.unwrap();
    assert_ne!(second_revision, first_revision);
    {
        let mut state = rig.state.lock().await;
        assert_eq!(state.store.receipt_ack_through(&rig.binding).await.unwrap(), second.operation_seq);
        assert_eq!(state.store.confirmed_receipt_through(&rig.binding).await.unwrap(), second.operation_seq);
        assert!(state.store.next_pending_outbox(&rig.binding).await.unwrap().is_none());
        let draft = state.store.read_registered_checkpoint(&rig.binding, &id).await.unwrap().unwrap();
        assert_eq!(draft.generation, 3);
        assert_eq!(draft.bytes, b"latest draft\n");
        assert_eq!(draft.base_bytes, b"second edit\n");
        assert_eq!(draft.base_revision, second_revision.as_str());
        state.store.close_registered_document(&rig.binding, &id, &edit.edit_session_id).await.unwrap();
    }
    rig.server.shutdown().await.unwrap();
}
