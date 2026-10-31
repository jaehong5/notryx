use crate::conflict_snapshot_native::test_support::send_copy_content::*;

#[tokio::test]
async fn copy_queue_waits_for_open_empty_session_then_reuses_untouched_registration() {
    let rig = rig_with_followup(true).await;
    let (copy, _, created) = created_copy(&rig).await;
    let mut state = rig.runtime.0.lock().await;
    let edit = state
        .store
        .open_registered_document(
            &rig.binding,
            &copy.node_id,
            b"",
            created.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    assert!(
        state
            .store
            .queue_content_conflict_copy_content(
                &rig.binding,
                &rig.operation.operation_id,
                copy.intent_version
            )
            .await
            .is_err()
    );
    assert!(
        state
            .store
            .read_registered_checkpoint(&rig.binding, &copy.node_id)
            .await
            .unwrap()
            .is_none()
    );
    state
        .store
        .close_registered_document(&rig.binding, &copy.node_id, &edit.edit_session_id)
        .await
        .unwrap();
    let item = state
        .store
        .queue_content_conflict_copy_content(
            &rig.binding,
            &rig.operation.operation_id,
            copy.intent_version,
        )
        .await
        .unwrap();
    assert_eq!(item.bytes, b"newer local\n");
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &copy.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local\n");
    assert_eq!(draft.base_bytes, b"");
    drop(state);
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_queue_never_overwrites_independent_dirty_copy_draft() {
    let rig = rig_with_followup(true).await;
    let (copy, _, created) = created_copy(&rig).await;
    let mut state = rig.runtime.0.lock().await;
    let edit = state
        .store
        .open_registered_document(
            &rig.binding,
            &copy.node_id,
            b"",
            created.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    state
        .store
        .save_registered_checkpoint(
            &rig.binding,
            &copy.node_id,
            &edit.edit_session_id,
            1,
            b"independent edit\n",
        )
        .await
        .unwrap();
    state
        .store
        .close_registered_document(&rig.binding, &copy.node_id, &edit.edit_session_id)
        .await
        .unwrap();
    assert!(
        state
            .store
            .queue_content_conflict_copy_content(
                &rig.binding,
                &rig.operation.operation_id,
                copy.intent_version
            )
            .await
            .is_err()
    );
    let draft = state
        .store
        .read_registered_checkpoint(&rig.binding, &copy.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"independent edit\n");
    assert_eq!(draft.base_bytes, b"");
    drop(state);
    preserve_original(&rig, &copy).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
