include!("remote_deleted_copy_confirm_tests.rs");

use base64::{Engine as _, engine::general_purpose::STANDARD};

type ReopenRequest = notryx_protocol::WorkspaceDeletedDraftCopyReopenRequest;

fn reopen_request(rig: &Rig) -> ReopenRequest {
    serde_json::from_value(serde_json::json!({
        "renderer_session_id": OWNER,
        "binding": crate::to_contract(rig.binding.clone()),
        "document_id": rig.node.node_id.to_string(),
        "previous_edit_session_id": rig.edit_session,
    }))
    .unwrap()
}

async fn reopen_deleted_copy(
    rig: &Rig,
    request: ReopenRequest,
) -> Result<serde_json::Value, String> {
    let result =
        crate::remote_deleted_copy_native::reopen_for_renderer(&rig.runtime, request).await?;
    Ok(serde_json::to_value(result).unwrap())
}

async fn assert_reopened_local(rig: &Rig, result: &serde_json::Value) -> String {
    let document = &result["document"];
    assert_eq!(
        document["binding"],
        serde_json::to_value(crate::to_contract(rig.binding.clone())).unwrap()
    );
    assert_eq!(document["document_id"], rig.node.node_id.to_string());
    assert_eq!(document["generation"], "1");
    assert_eq!(
        STANDARD
            .decode(document["bytes_base64"].as_str().unwrap())
            .unwrap(),
        b"local first\n"
    );
    let opened = document["edit_session_id"].as_str().unwrap().to_owned();
    assert_ne!(opened, rig.edit_session);
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"local first\n");
    assert_eq!(draft.base_bytes, b"server source\n");
    assert_eq!(draft.generation, 1);
    assert!(!state.product_sessions.contains_key(&rig.edit_session));
    assert!(state.product_sessions.contains_key(&opened));
    assert!(
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &rig.node.node_id.to_string(),
                &rig.edit_session,
                2,
                b"stale\n"
            )
            .await
            .is_err()
    );
    opened
}

#[tokio::test]
async fn deleted_copy_reopen_without_intent_preserves_draft_and_replaces_only_previous_session() {
    let rig = rig().await;
    let original_outbox = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .list_outbox(&rig.binding, 0, 100)
        .await
        .unwrap();
    let result = reopen_deleted_copy(&rig, reopen_request(&rig))
        .await
        .unwrap();
    assert!(result["state"].is_null());
    let opened = assert_reopened_local(&rig, &result).await;
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap(),
        original_outbox
    );
    assert!(
        reopen_deleted_copy(&rig, reopen_request(&rig))
            .await
            .is_err()
    );
    assert!(
        rig.runtime
            .0
            .lock()
            .await
            .product_sessions
            .contains_key(&opened)
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_reopen_recovers_recorded_intent_when_previous_session_was_retired() {
    let rig = rig().await;
    let deleted = deleted_by_peer(&rig, 1, &rig.node).await;
    let accepted = confirm_deleted_copy(&rig, deleted_copy_request(&rig, &deleted))
        .await
        .unwrap();
    let before = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
        .await
        .unwrap()
        .unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        state
            .store
            .close_registered_document(
                &rig.binding,
                &rig.node.node_id.to_string(),
                &rig.edit_session,
            )
            .await
            .unwrap();
        state.gate.close_session(&rig.edit_session).unwrap();
        state.product_sessions.remove(&rig.edit_session);
    }
    let result = reopen_deleted_copy(&rig, reopen_request(&rig))
        .await
        .unwrap();
    assert_eq!(result["state"]["node_id"], accepted["node_id"]);
    assert_eq!(
        result["state"]["create_operation_id"],
        accepted["create_operation_id"]
    );
    assert_eq!(result["state"]["create_status"], "not_queued");
    assert_reopened_local(&rig, &result).await;
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_deleted_draft_copy(&rig.binding, &rig.node.node_id.to_string())
            .await
            .unwrap(),
        Some(before)
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_reopen_rejects_wrong_identity_without_retiring_sessions() {
    let rig = rig().await;
    let request = reopen_request(&rig);
    for field in ["owner", "binding", "session"] {
        let mut value = serde_json::to_value(&request).unwrap();
        match field {
            "owner" => value["renderer_session_id"] = serde_json::json!("previous-owner"),
            "binding" => {
                value["binding"]["workspace_epoch"] = serde_json::json!(Uuid::new_v4().to_string())
            }
            "session" => value["previous_edit_session_id"] = serde_json::json!("other-session"),
            _ => unreachable!(),
        }
        assert!(
            reopen_deleted_copy(&rig, serde_json::from_value(value).unwrap())
                .await
                .is_err(),
            "{field}"
        );
        assert!(
            rig.runtime
                .0
                .lock()
                .await
                .product_sessions
                .contains_key(&rig.edit_session)
        );
        assert_original_preserved(&rig, b"local first\n").await;
    }
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_copy_reopen_never_closes_a_newer_session_or_other_document() {
    let rig = rig().await;
    let other = rig
        .session
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| {
            node.node_id != rig.node.node_id && node.content_rev.is_some() && !node.deleted
        })
        .unwrap();
    let (new_session, other_session, other_document) = {
        let mut state = rig.runtime.0.lock().await;
        let document = rig.node.node_id.to_string();
        state
            .store
            .close_registered_document(&rig.binding, &document, &rig.edit_session)
            .await
            .unwrap();
        state.gate.close_session(&rig.edit_session).unwrap();
        state.product_sessions.remove(&rig.edit_session);
        let (newer, _) = state
            .store
            .try_open_registered_draft(&rig.binding, &document)
            .await
            .unwrap()
            .unwrap();
        state
            .gate
            .open_session(newer.edit_session_id.clone())
            .unwrap();
        state.product_sessions.insert(
            newer.edit_session_id.clone(),
            ProductSession {
                document_id: document,
                binding: rig.binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        let other_document = other.node_id.to_string();
        let opened = state
            .store
            .open_registered_document(
                &rig.binding,
                &other_document,
                b"other base\n",
                other.content_rev.as_ref().unwrap().as_str(),
            )
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(
                &rig.binding,
                &other_document,
                &opened.edit_session_id,
                1,
                b"other local\n",
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
                document_id: other_document.clone(),
                binding: rig.binding.clone(),
                kind: ProductSessionKind::Active,
            },
        );
        (
            newer.edit_session_id,
            opened.edit_session_id,
            other_document,
        )
    };
    assert!(
        reopen_deleted_copy(&rig, reopen_request(&rig))
            .await
            .is_err()
    );
    let mut state = rig.runtime.0.lock().await;
    assert!(state.product_sessions.contains_key(&new_session));
    assert!(state.product_sessions.contains_key(&other_session));
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.binding, &other_document)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"other local\n"
    );
    drop(state);
    rig.server.shutdown().await.unwrap();
}
