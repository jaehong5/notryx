use crate::conflict_snapshot_native::test_support::copy_status::*;

fn approved_progress(
    rig: &Rig,
    version: &str,
    approved: Option<&str>,
) -> notryx_protocol::WorkspaceContentConflictCopyProgressRequest {
    notryx_protocol::WorkspaceContentConflictCopyProgressRequest {
        progress: progress_request(rig, version),
        approved_followup_operation_id: approved.map(str::to_owned),
    }
}

async fn progress_with_approval(
    rig: &Rig,
    request: notryx_protocol::WorkspaceContentConflictCopyProgressRequest,
) -> Result<serde_json::Value, String> {
    let result = crate::conflict_snapshot_native::progress_copy_with_approval_for_renderer(
        &rig.runtime,
        request,
    )
    .await?;
    Ok(serde_json::to_value(result).unwrap())
}

#[tokio::test]
async fn copy_progress_requires_explicit_approval_of_exact_followup_id() {
    let rig = rig_with_followup(true).await;
    let confirmed = confirm_copy(&rig, copy_request(&rig, None)).await.unwrap();
    let version = confirmed["version"].as_str().unwrap();
    let id = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .document_outbox_state(&rig.binding, &rig.document)
        .await
        .unwrap()
        .latest
        .unwrap()
        .operation_id;
    let before = copy_status(&rig, copy_status_request(&rig)).await.unwrap();
    assert_eq!(before["copy"]["followup_operation_id"], id);
    let no_approval = progress_with_approval(&rig, approved_progress(&rig, version, None))
        .await
        .unwrap();
    assert_eq!(no_approval["status"], "approval_required");
    assert_eq!(no_approval["followup_operation_id"], id);
    assert!(
        progress_with_approval(
            &rig,
            approved_progress(&rig, version, Some(&Uuid::now_v7().to_string()))
        )
        .await
        .is_err()
    );
    assert_copy_confirm_preserves_sources(&rig).await;
    let requested = progress_with_approval(&rig, approved_progress(&rig, version, Some(&id)))
        .await
        .unwrap();
    assert_eq!(requested["status"], "pending");
    assert_eq!(requested["requested_operation_id"], id);
    assert_eq!(
        progress_with_approval(&rig, approved_progress(&rig, version, Some(&id)))
            .await
            .unwrap(),
        requested
    );
    assert_eq!(
        crate::send::send_one(rig.runtime.0.clone(), OWNER)
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceSendStatus::Abandoned
    );
    assert!(
        progress_with_approval(&rig, approved_progress(&rig, version, Some(&id)))
            .await
            .is_err()
    );
    let state = rig.runtime.0.lock().await;
    drop(state);
    assert!(!rig.config.workspace.notes_root.join("my-copy.md").exists());
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
