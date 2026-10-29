include!("conflict_choose_remote_command_tests.rs");

type CopyRequest = notryx_protocol::WorkspaceContentConflictCopyRequest;

fn copy_request(rig: &Rig, version: Option<u64>) -> CopyRequest {
    CopyRequest {
        selection: choice_request(rig, 2, version),
        parent_id: rig.initial.parent_id.unwrap().to_string(),
        name: "my-copy.md".into(),
    }
}

async fn confirm_copy(rig: &Rig, request: CopyRequest) -> Result<serde_json::Value, String> {
    let result =
        crate::conflict_snapshot_native::confirm_copy_for_renderer(&rig.runtime, request).await?;
    Ok(serde_json::to_value(result).unwrap())
}

fn spawn_copy(rig: &Rig, request: CopyRequest) -> tokio::task::JoinHandle<bool> {
    let runtime = Runtime(rig.runtime.0.clone());
    tokio::spawn(async move {
        crate::conflict_snapshot_native::confirm_copy_for_renderer(&runtime, request)
            .await
            .is_err()
    })
}

async fn assert_copy_confirm_preserves_sources(rig: &Rig) {
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

#[tokio::test]
async fn copy_confirmation_records_displayed_bytes_destination_and_ids_without_sending() {
    let rig = rig_with_followup(true).await;
    let request = copy_request(&rig, None);
    let head = rig.session.server_info().await.unwrap().head_seq;
    let result = confirm_copy(&rig, request.clone()).await.unwrap();
    assert_eq!(result["status"], "pending");
    assert_eq!(result["version"], "0");
    let mut state = rig.runtime.0.lock().await;
    let saved = state
        .store
        .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["node_id"], saved.node_id);
    assert_eq!(result["create_operation_id"], saved.create_operation_id);
    assert_eq!(saved.parent_id, request.parent_id);
    assert_eq!(saved.name, request.name);
    assert_eq!(saved.local_bytes, b"newer local\n");
    drop(state);
    assert_eq!(confirm_copy(&rig, request).await.unwrap(), result);
    assert_eq!(rig.session.server_info().await.unwrap().head_seq, head);
    assert_copy_confirm_preserves_sources(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copy_confirmation_rejects_invalid_displayed_snapshot_binding_and_destination() {
    for field in [
        "base_hash",
        "local_hash",
        "remote_hash",
        "base_revision",
        "conflict_revision",
        "remote_revision",
        "binding",
        "parent",
        "name",
        "session",
        "version",
    ] {
        let rig = rig_with_followup(true).await;
        let mut request = copy_request(&rig, None);
        match field {
            "base_hash" => request.selection.base_hash = FOLLOWUP_HASH.into(),
            "local_hash" => request.selection.local_hash = BASE_HASH.into(),
            "remote_hash" => request.selection.remote_hash = BASE_HASH.into(),
            "base_revision" => request.selection.base_revision = "other-rev".into(),
            "conflict_revision" => request.selection.conflict_revision = "other-rev".into(),
            "remote_revision" => request.selection.remote_revision = "other-rev".into(),
            "binding" => request.selection.binding.device_stream_id = Uuid::new_v4().to_string(),
            "parent" => request.parent_id = rig.document.clone(),
            "name" => request.name = "../bad.md".into(),
            "session" => request.selection.edit_session_id = Uuid::new_v4().to_string(),
            "version" => request.selection.expected_intent_version = Some("4".into()),
            _ => unreachable!(),
        }
        assert!(confirm_copy(&rig, request).await.is_err(), "{field}");
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .store
                .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        drop(state);
        assert_copy_confirm_preserves_sources(&rig).await;
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn copy_confirmation_rejects_session_owner_registration_and_checkpoint_replacement_during_fetch()
 {
    for change in ["owner", "registration", "session", "checkpoint"] {
        let rig = rig_with_followup(true).await;
        rig.armed.store(true, Ordering::SeqCst);
        let pending = spawn_copy(&rig, copy_request(&rig, None));
        tokio::time::timeout(std::time::Duration::from_secs(10), rig.captured.notified())
            .await
            .unwrap();
        match change {
            "owner" => {
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                crate::search_native::replace_renderer(&mut state, "new-owner".into());
            }
            "session" => {
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
            }
            "registration" => {
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
                let binding = registration_binding(replacement.binding());
                let mut state = rig.runtime.0.lock().await;
                state.store.reset_edit_sessions();
                state.product_sessions.clear();
                state.store.activate_registration(&binding).await.unwrap();
                state.session = Some(replacement);
            }
            "checkpoint" => {
                rig.runtime
                    .0
                    .lock()
                    .await
                    .store
                    .save_registered_checkpoint(
                        &rig.binding,
                        &rig.document,
                        &rig.edit_session,
                        3,
                        b"newer unsent\n",
                    )
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        rig.release.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap(),
            "{change}"
        );
        let mut state = rig.runtime.0.lock().await;
        assert!(
            state
                .store
                .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        let draft = state
            .store
            .read_registered_checkpoint(&rig.binding, &rig.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            draft.bytes,
            if change == "checkpoint" {
                b"newer unsent\n".as_slice()
            } else {
                b"newer local\n"
            }
        );
        assert_eq!(draft.base_bytes, BASE);
        assert_eq!(
            state
                .store
                .read_outbox_operation(&rig.binding, &rig.operation.operation_id)
                .await
                .unwrap()
                .unwrap()
                .bytes,
            LOCAL
        );
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
        assert!(!rig.config.workspace.notes_root.join("my-copy.md").exists());
        rig.proxy.abort();
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn copy_reconsideration_requires_explicit_version_and_preserves_old_choice() {
    let rig = rig_with_followup(true).await;
    let old = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .record_content_conflict_intent(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &notryx_client_store::ContentConflictIntentRequest {
                generation: 2,
                remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
                remote_hash: rig.first_peer.content_hash.clone().unwrap(),
                remote_bytes: REMOTE.to_vec(),
                choice: notryx_client_store::ContentConflictChoice::SaveMerged {
                    bytes: b"old merged\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    let mut invalid = copy_request(&rig, Some(old.version));
    invalid.name = "../bad.md".into();
    assert!(confirm_copy(&rig, invalid).await.is_err());
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(old.clone())
    );
    let mut stale = copy_request(&rig, Some(old.version));
    stale.selection.expected_intent_version = Some((old.version + 1).to_string());
    assert!(confirm_copy(&rig, stale).await.is_err());
    let request = copy_request(&rig, Some(old.version));
    let result = confirm_copy(&rig, request.clone()).await.unwrap();
    assert_eq!(result["status"], "pending");
    assert_eq!(result["version"], (old.version + 1).to_string());
    assert_eq!(confirm_copy(&rig, request).await.unwrap(), result);
    let mut state = rig.runtime.0.lock().await;
    let current = state
        .store
        .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.choice,
        notryx_client_store::ContentConflictChoice::KeepLocalCopy
    );
    assert_eq!(current.version, old.version + 1);
    assert_eq!(
        state
            .store
            .read_content_conflict_intent_history(
                &rig.binding,
                &rig.operation.operation_id,
                old.version
            )
            .await
            .unwrap(),
        Some(old)
    );
    drop(state);
    assert_copy_confirm_preserves_sources(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_copy_record_rolls_back_reconsidered_intent_and_history() {
    use sqlx::Connection;
    let rig = rig_with_followup(true).await;
    let old = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .record_content_conflict_intent(
            &rig.binding,
            &rig.document,
            &rig.operation.operation_id,
            &notryx_client_store::ContentConflictIntentRequest {
                generation: 2,
                remote_revision: rig.first_peer.content_rev.as_ref().unwrap().as_str().into(),
                remote_hash: rig.first_peer.content_hash.clone().unwrap(),
                remote_bytes: REMOTE.to_vec(),
                choice: notryx_client_store::ContentConflictChoice::SaveMerged {
                    bytes: b"old merged\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    let path = rig._directory.path().join("conflict-snapshot.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_copy_intent BEFORE INSERT ON content_conflict_copies BEGIN SELECT RAISE(ABORT, 'injected'); END").execute(&mut db).await.unwrap();
    let request = copy_request(&rig, Some(old.version));
    assert!(confirm_copy(&rig, request.clone()).await.is_err());
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state
            .store
            .read_content_conflict_intent(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap(),
        Some(old.clone())
    );
    assert!(
        state
            .store
            .read_content_conflict_intent_history(
                &rig.binding,
                &rig.operation.operation_id,
                old.version
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        state
            .store
            .read_content_conflict_copy(&rig.binding, &rig.operation.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    drop(state);
    assert_copy_confirm_preserves_sources(&rig).await;
    sqlx::query("DROP TRIGGER fail_copy_intent")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert_eq!(
        confirm_copy(&rig, request).await.unwrap()["version"],
        (old.version + 1).to_string()
    );
    assert_copy_confirm_preserves_sources(&rig).await;
    rig.proxy.abort();
    rig.server.shutdown().await.unwrap();
}
