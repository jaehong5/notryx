include!("conflict_merge_queue_native_tests.rs");

use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::time::Duration;

// Replace this test-only failure with the native hook entry point after observing Red.
async fn queue_at_boundary(
    _runtime: &Runtime,
    _binding: &RegistrationBinding,
    _document: &str,
    _edit_session: &str,
    _operation: &str,
    _version: u64,
    _phase: &str,
    _ready: Arc<Notify>,
    _resume: Arc<Notify>,
) -> Result<OutboxOperation, String> {
    Err("not_implemented".into())
}

fn spawn_queue_at_boundary(
    rig: &Rig,
    version: u64,
    phase: &'static str,
    ready: Arc<Notify>,
    resume: Arc<Notify>,
) -> tokio::task::JoinHandle<Result<OutboxOperation, String>> {
    let runtime = Runtime(rig.runtime.0.clone());
    let binding = rig.binding.clone();
    let document = rig.document.clone();
    let edit = rig.edit_session.clone();
    let operation = rig.operation.operation_id.clone();
    tokio::spawn(async move {
        queue_at_boundary(
            &runtime, &binding, &document, &edit, &operation, version, phase, ready, resume,
        )
        .await
    })
}

async fn wait_for_boundary(
    ready: &Notify,
    task: &mut tokio::task::JoinHandle<Result<OutboxOperation, String>>,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            _ = ready.notified() => (),
            result = &mut *task => panic!("queue completed before the requested boundary: {result:?}"),
        }
    }).await.expect("queue did not reach the requested boundary");
}

#[tokio::test]
async fn merge_queue_registration_replacement_after_catchup_preserves_frozen_sources() {
    let rig = rig().await;
    let intent = merged_intent(&rig).await;
    confirm_original_ack(&rig).await;
    let head = rig.session.server_info().await.unwrap().head_seq;
    let pairing = server_test_support::pairing_request(&rig.config).await.0;
    let replacement = Arc::new(
        Session::pair(
            &format!("http://{}", rig.server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let new_binding = registration_binding(replacement.binding());
    assert_ne!(new_binding, rig.binding);
    let ready = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let mut pending = spawn_queue_at_boundary(
        &rig,
        intent.version,
        "after_catchup_before_recheck",
        ready.clone(),
        resume.clone(),
    );
    wait_for_boundary(&ready, &mut pending).await;
    {
        let mut state = rig.runtime.0.lock().await;
        state.store.reset_edit_sessions();
        state.product_sessions.clear();
        state
            .store
            .activate_registration(&new_binding)
            .await
            .unwrap();
        state.session = Some(replacement);
    }
    resume.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    {
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(
            state.store.current_registration().await.unwrap(),
            Some(new_binding.clone())
        );
        assert_eq!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap(),
            Some(intent.clone())
        );
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, intent.generation);
        assert_eq!(draft.bytes, MERGED);
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(draft.base_revision, intent.base_revision);
        let original = state
            .store
            .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.status, OutboxStatus::Conflict);
        assert!(original.frozen);
        assert_eq!(original.bytes, LOCAL);
        assert_eq!(original.operation_seq, rig.operation.operation_seq);
        assert_eq!(original.expected_revision, rig.operation.expected_revision);
        assert_eq!(
            state
                .store
                .list_outbox(&rig.binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(
            state
                .store
                .list_outbox(&new_binding, 0, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(
            state
                .store
                .read_registered_checkpoint(&new_binding, &rig.document)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    assert_eq!(
        std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
        REMOTE
    );
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn merge_queue_cancellation_before_or_after_store_commit_invalidates_session_and_preserves_draft()
 {
    for phase in ["before_store_queue", "after_store_queue"] {
        let rig = rig().await;
        let intent = merged_intent(&rig).await;
        confirm_original_ack(&rig).await;
        let head = rig.session.server_info().await.unwrap().head_seq;
        let ready = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let mut pending =
            spawn_queue_at_boundary(&rig, intent.version, phase, ready.clone(), resume);
        wait_for_boundary(&ready, &mut pending).await;
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled(), "phase={phase}");
        {
            let mut state = rig.runtime.0.lock().await;
            assert!(
                !state.product_sessions.contains_key(&rig.edit_session),
                "phase={phase}"
            );
            assert!(
                state.gate.check_session(&rig.edit_session).is_err(),
                "phase={phase}"
            );
            assert_eq!(
                state
                    .store
                    .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                    .await
                    .unwrap(),
                Some(intent.clone())
            );
            let draft = state
                .store
                .read_registered_checkpoint(&rig.binding, &rig.document)
                .await
                .unwrap()
                .unwrap();
            let (generation, base, revision) = if phase == "before_store_queue" {
                (intent.generation, BASE, intent.base_revision.as_str())
            } else {
                (
                    intent.generation + 1,
                    REMOTE,
                    intent.remote_revision.as_str(),
                )
            };
            assert_eq!(draft.generation, generation, "phase={phase}");
            assert_eq!(draft.bytes, MERGED, "phase={phase}");
            assert_eq!(draft.base_bytes, base, "phase={phase}");
            assert_eq!(draft.base_revision, revision, "phase={phase}");
            assert!(
                state
                    .store
                    .save_registered_checkpoint(
                        &rig.binding,
                        &rig.document,
                        &rig.edit_session,
                        generation + 1,
                        LATER
                    )
                    .await
                    .is_err(),
                "phase={phase}"
            );
            let page = state.store.list_outbox(&rig.binding, 0, 100).await.unwrap();
            assert_eq!(
                page.entries.len(),
                if phase == "before_store_queue" { 1 } else { 2 },
                "phase={phase}"
            );
            let original = state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(original.bytes, LOCAL);
            assert_eq!(original.operation_seq, rig.operation.operation_seq);
            if phase == "after_store_queue" {
                let queued = page
                    .entries
                    .iter()
                    .find(|entry| entry.operation_id != rig.operation.operation_id)
                    .unwrap();
                let queued = state
                    .store
                    .read_outbox_operation(&rig.binding, &queued.operation_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(queued.operation_seq, rig.operation.operation_seq + 1);
                assert_eq!(queued.status, OutboxStatus::Queued);
                assert_eq!(queued.bytes, MERGED);
                assert_eq!(
                    queued.expected_revision.as_deref(),
                    Some(intent.remote_revision.as_str())
                );
            }
        }
        let (status, reopened) =
            crate::conflict_snapshot_native::reopen_conflict_checkpoint_for_renderer(
                &rig.runtime,
                OWNER,
                &rig.binding,
                &rig.document,
                &rig.operation.operation_id,
                &rig.edit_session,
            )
            .await
            .unwrap();
        assert!(
            matches!(status, ContentConflictResolutionState::Pending { version, .. } if version == intent.version)
        );
        assert_ne!(reopened.edit_session_id, rig.edit_session);
        assert_eq!(STANDARD.decode(&reopened.bytes_base64).unwrap(), MERGED);
        assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
        assert_eq!(
            std::fs::read(rig.config.workspace.notes_root.join("example.md")).unwrap(),
            REMOTE
        );
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}
