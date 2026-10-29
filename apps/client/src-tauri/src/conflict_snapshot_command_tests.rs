#[tokio::test]
async fn first_conflict_command_selects_412_before_newer_queued_document_work() {
    let rig = rig_with_followup(true).await;
    let followup = {
        let mut state = rig.runtime.0.lock().await;
        let latest = state
            .store
            .document_outbox_state(&rig.binding, &rig.document)
            .await
            .unwrap()
            .latest
            .unwrap();
        state
            .store
            .read_outbox_operation(&rig.binding, &latest.operation_id)
            .await
            .unwrap()
            .unwrap()
    };
    let result = super::prepare_first_conflict_snapshot(
        &rig.runtime,
        OWNER,
        &rig.binding,
        &rig.document,
        &rig.edit_session,
        2,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.binding, rig.binding);
    assert_eq!(result.node_id, rig.initial.node_id);
    assert_eq!(result.operation_id, rig.operation.operation_id);
    assert_eq!(result.operation_seq, rig.operation.operation_seq);
    assert_ne!(result.operation_id, followup.operation_id);
    assert_eq!(result.generation, 2);
    assert_eq!(result.local_bytes, b"newer local\n");
    assert_eq!(result.base_bytes, BASE);
    assert_eq!(result.remote_bytes, REMOTE);
    assert_eq!(
        result.conflict_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    assert_eq!(
        result.remote_revision,
        rig.first_peer.content_rev.as_ref().unwrap().as_str()
    );
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, BASE);
    let page = state.store.list_outbox(&rig.binding, 0, 100).await.unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.entries[0].operation_id, rig.operation.operation_id);
    assert_eq!(page.entries[1].operation_id, followup.operation_id);
    drop(state);
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_conflict_command_rejects_wrong_owner_binding_document_and_edit_session() {
    let rig = rig().await;
    let mut other_binding = rig.binding.clone();
    other_binding.device_id = Uuid::new_v4().to_string();
    assert!(
        super::prepare_first_conflict_snapshot(
            &rig.runtime,
            "old-owner",
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            1
        )
        .await
        .is_err()
    );
    assert!(
        super::prepare_first_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &other_binding,
            &rig.document,
            &rig.edit_session,
            1
        )
        .await
        .is_err()
    );
    assert!(
        super::prepare_first_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &Uuid::now_v7().to_string(),
            &rig.edit_session,
            1
        )
        .await
        .is_err()
    );
    assert!(
        super::prepare_first_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            "old-session",
            1
        )
        .await
        .is_err()
    );
    assert!(
        super::prepare_first_conflict_snapshot(
            &rig.runtime,
            OWNER,
            &rig.binding,
            &rig.document,
            &rig.edit_session,
            2
        )
        .await
        .is_err()
    );
    assert_preserved(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_conflict_command_returns_none_only_for_valid_session_without_a_412() {
    let (_dir, server, runtime, session, document) = setup().await;
    let binding = registration_binding(session.binding());
    let edit_session = {
        let mut state = runtime.0.lock().await;
        let opened = state
            .store
            .open_registered_document(&binding, &document, b"base", "base-revision")
            .await
            .unwrap();
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
        opened.edit_session_id
    };
    assert!(
        super::prepare_first_conflict_snapshot(
            &runtime,
            OWNER,
            &binding,
            &document,
            &edit_session,
            1
        )
        .await
        .unwrap()
        .is_none()
    );
    let mut state = runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&binding, &document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local draft");
    assert!(state.store.next_outbox(&binding).await.unwrap().is_some());
    drop(state);
    server.shutdown().await.unwrap();
}
#[tokio::test]
async fn conflict_response_hashes_identify_the_exact_displayed_bytes() {
    let rig = rig().await;
    let request = notryx_protocol::WorkspaceContentConflictRequest {
        renderer_session_id: OWNER.into(),
        binding: crate::to_contract(rig.binding.clone()),
        document_id: rig.document.clone(),
        edit_session_id: rig.edit_session.clone(),
        generation: "1".into(),
    };
    let response = super::prepare_content_conflict_for_renderer_inner(&rig.runtime, request, None)
        .await
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(
        json["base_hash"],
        "sha256:cd77865662cf343ac50bd06999bc7df6fef875e24724c69fa83276cfe0c93573"
    );
    assert_eq!(
        json["local_hash"],
        "sha256:90a02348f62b287c95bb84ef49e7bd7e5fa5a6867d0a0772eb1e296d9a6d45de"
    );
    assert_eq!(
        json["remote_hash"],
        "sha256:5ae9901bda8cab82169a142e47d550bee29710cd3fd1e61a130936e4b9c21c29"
    );
    assert_eq!(
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            json["base_bytes_base64"].as_str().unwrap()
        )
        .unwrap(),
        BASE
    );
    assert_eq!(
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            json["local_bytes_base64"].as_str().unwrap()
        )
        .unwrap(),
        LOCAL
    );
    assert_eq!(
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            json["remote_bytes_base64"].as_str().unwrap()
        )
        .unwrap(),
        REMOTE
    );
    assert_preserved(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
