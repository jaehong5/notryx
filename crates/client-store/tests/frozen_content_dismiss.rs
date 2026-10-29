include!("frozen_document_apply.rs");

use notryx_client_store::OutboxAbandonState;

use notryx_client_store::FrozenContentDismissApproval as DismissApproval;

async fn dismiss_frozen_content(
    store: &mut ClientStore,
    approval: &DismissApproval,
) -> Result<(), StoreError> {
    store.dismiss_frozen_content(approval).await
}

async fn dismissal_status(
    store: &mut ClientStore,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    document: &str,
    operation_id: &str,
) -> Result<bool, StoreError> {
    store
        .frozen_content_dismissal_status(current, old, document, operation_id)
        .await
}

struct DismissRig {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: ClientStore,
    approval: DismissApproval,
    other_id: String,
    successor_id: Option<String>,
}

async fn dismiss_rig(mode: &str, successor: bool) -> DismissRig {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("frozen-dismiss.sqlite");
    let workspace = Uuid::new_v4().to_string();
    let epoch = Uuid::new_v4().to_string();
    let old = binding(&workspace, &epoch);
    let current = binding(&workspace, &epoch);
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    let edit = store
        .open_registered_document(&old, DOC_A, b"original base", "base-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&old, DOC_A, &edit.edit_session_id, 1, b"original operation")
        .await
        .unwrap();
    let queued = store.queue_checkpoint(&old, DOC_A, 1).await.unwrap();
    match mode {
        "queued" => {}
        "sent" => {
            store.prepare_outbox(&old).await.unwrap().unwrap();
        }
        "abandon_requested" => {
            store
                .request_outbox_abandon(&old, &queued.operation_id)
                .await
                .unwrap();
        }
        "abandon_sent" => {
            store
                .request_outbox_abandon(&old, &queued.operation_id)
                .await
                .unwrap();
            store.prepare_outbox(&old).await.unwrap().unwrap();
        }
        _ => panic!("unknown test mode"),
    }
    store
        .save_registered_checkpoint(&old, DOC_A, &edit.edit_session_id, 2, b"newer checkpoint")
        .await
        .unwrap();
    let successor_id = if successor {
        assert_eq!(mode, "sent");
        Some(
            store
                .queue_checkpoint(&old, DOC_A, 2)
                .await
                .unwrap()
                .operation_id,
        )
    } else {
        None
    };
    store
        .close_registered_document(&old, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    let other = queue(&mut store, &old, DOC_B, b"other document").await;
    store.activate_registration(&current).await.unwrap();
    let operation = store
        .read_outbox_operation(&old, &queued.operation_id)
        .await
        .unwrap()
        .unwrap();
    let draft = store
        .read_registered_checkpoint(&old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    DismissRig {
        _directory: directory,
        path,
        store,
        approval: DismissApproval {
            current,
            old,
            document: DOC_A.into(),
            operation_id: operation.operation_id,
            operation_seq: operation.operation_seq,
            operation_generation: operation.generation,
            operation_status: operation.status,
            abandon_state: operation.abandon_state,
            expected_revision: operation.expected_revision,
            dependency: None,
            payload_hash: operation.payload_hash,
            payload: operation.bytes,
            draft_generation: draft.generation,
            draft: draft.bytes,
            base: draft.base_bytes,
            base_revision: draft.base_revision,
            view_fingerprint:
                "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        },
        other_id: other.operation_id,
        successor_id,
    }
}

async fn receipt_and_generation(
    path: &Path,
) -> Vec<(
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let rows = sqlx::query_as("SELECT operation_id,generation,storage_id,receipt_json,committed_revision,conflict_revision FROM client_outbox ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

#[tokio::test]
async fn frozen_content_dismiss_is_explicit_idempotent_and_preserves_all_sources() {
    for mode in ["queued", "sent", "abandon_requested", "abandon_sent"] {
        let mut rig = dismiss_rig(mode, false).await;
        let before = snapshot(&rig.path).await;
        let before_receipts = receipt_and_generation(&rig.path).await;
        assert!(
            !dismissal_status(
                &mut rig.store,
                &rig.approval.current,
                &rig.approval.old,
                DOC_A,
                &rig.approval.operation_id
            )
            .await
            .unwrap()
        );
        assert!(
            rig.store
                .next_pending_outbox(&rig.approval.current)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(snapshot(&rig.path).await, before);
        dismiss_frozen_content(&mut rig.store, &rig.approval)
            .await
            .unwrap();
        assert!(
            dismissal_status(
                &mut rig.store,
                &rig.approval.current,
                &rig.approval.old,
                DOC_A,
                &rig.approval.operation_id
            )
            .await
            .unwrap()
        );
        assert_eq!(snapshot(&rig.path).await, before);
        assert_eq!(receipt_and_generation(&rig.path).await, before_receipts);
        assert!(before.0.iter().any(|row| row.0 == rig.other_id));
        let item = rig
            .store
            .read_outbox_operation(&rig.approval.old, &rig.approval.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(item.status, rig.approval.operation_status);
        assert_eq!(item.abandon_state, rig.approval.abandon_state);
        assert_eq!(item.bytes, rig.approval.payload);
        dismiss_frozen_content(&mut rig.store, &rig.approval)
            .await
            .unwrap();
        let mut changed = rig.approval.clone();
        changed.view_fingerprint =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222".into();
        assert!(
            dismiss_frozen_content(&mut rig.store, &changed)
                .await
                .is_err()
        );
        assert_eq!(snapshot(&rig.path).await, before);
        rig.store.close().await.unwrap();
        let mut reopened = ClientStore::open(&rig.path).await.unwrap();
        assert!(
            dismissal_status(
                &mut reopened,
                &rig.approval.current,
                &rig.approval.old,
                DOC_A,
                &rig.approval.operation_id
            )
            .await
            .unwrap()
        );
        dismiss_frozen_content(&mut reopened, &rig.approval)
            .await
            .unwrap();
        assert_eq!(snapshot(&rig.path).await, before);
    }
}

#[tokio::test]
async fn frozen_content_dismiss_keeps_unreviewed_draft_and_successor_blocked() {
    let mut rig = dismiss_rig("sent", true).await;
    let successor = rig.successor_id.clone().unwrap();
    let before = snapshot(&rig.path).await;
    dismiss_frozen_content(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    assert!(
        dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id
        )
        .await
        .unwrap()
    );
    assert!(
        !dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &successor
        )
        .await
        .unwrap()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let successor = rig
        .store
        .read_outbox_operation(&rig.approval.old, &successor)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(successor.bytes, b"newer checkpoint");
    let edit = rig
        .store
        .open_registered_document(
            &rig.approval.current,
            DOC_A,
            b"current base",
            "current-revision",
        )
        .await
        .unwrap();
    rig.store
        .save_registered_checkpoint(
            &rig.approval.current,
            DOC_A,
            &edit.edit_session_id,
            1,
            b"new current draft",
        )
        .await
        .unwrap();
    rig.store
        .close_registered_document(&rig.approval.current, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    let after = snapshot(&rig.path).await;
    assert!(matches!(
        rig.store
            .queue_checkpoint(&rig.approval.current, DOC_A, 1)
            .await,
        Err(StoreError::ReconciliationRequired)
    ));
    assert_eq!(snapshot(&rig.path).await, after);
}

#[tokio::test]
async fn frozen_content_dismiss_rejects_stale_snapshot_or_wrong_workspace_without_mutation() {
    let mut rig = dismiss_rig("sent", false).await;
    let before = snapshot(&rig.path).await;
    let mut variants = Vec::new();
    let mut value = rig.approval.clone();
    value.operation_seq += 1;
    variants.push(value);
    let mut value = rig.approval.clone();
    value.operation_generation += 1;
    variants.push(value);
    let mut value = rig.approval.clone();
    value.operation_status = OutboxStatus::Queued;
    variants.push(value);
    let mut value = rig.approval.clone();
    value.abandon_state = OutboxAbandonState::Requested;
    variants.push(value);
    let mut value = rig.approval.clone();
    value.expected_revision = None;
    variants.push(value);
    let mut value = rig.approval.clone();
    value.payload_hash.push('x');
    variants.push(value);
    let mut value = rig.approval.clone();
    value.payload.push(b'x');
    variants.push(value);
    let mut value = rig.approval.clone();
    value.draft_generation += 1;
    variants.push(value);
    let mut value = rig.approval.clone();
    value.draft.push(b'x');
    variants.push(value);
    let mut value = rig.approval.clone();
    value.base.push(b'x');
    variants.push(value);
    let mut value = rig.approval.clone();
    value.base_revision.push('x');
    variants.push(value);
    let mut value = rig.approval.clone();
    value.old = binding(&Uuid::new_v4().to_string(), &value.old.workspace_epoch);
    variants.push(value);
    for variant in variants {
        assert!(
            dismiss_frozen_content(&mut rig.store, &variant)
                .await
                .is_err()
        );
    }
    assert_eq!(snapshot(&rig.path).await, before);
    assert!(
        !dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id
        )
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn frozen_content_dismiss_rejects_changed_durable_draft_after_comparison() {
    let mut rig = dismiss_rig("sent", false).await;
    let edit = rig
        .store
        .open_recovery_draft(&rig.approval.old, DOC_A)
        .await
        .unwrap();
    rig.store
        .save_recovery_checkpoint(
            &rig.approval.old,
            DOC_A,
            &edit.edit_session_id,
            3,
            b"edited after comparison",
        )
        .await
        .unwrap();
    rig.store
        .close_recovery_draft(&rig.approval.old, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        dismiss_frozen_content(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert!(
        !dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id
        )
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn frozen_content_dismiss_cannot_reverse_an_existing_apply_decision() {
    let mut rig = apply_rig().await;
    apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let original = rig
        .store
        .read_outbox_operation(&rig.approval.old, &rig.approval.old_operation_id)
        .await
        .unwrap()
        .unwrap();
    let approval = DismissApproval {
        current: rig.approval.current.clone(),
        old: rig.approval.old.clone(),
        document: DOC_A.into(),
        operation_id: original.operation_id,
        operation_seq: original.operation_seq,
        operation_generation: original.generation,
        operation_status: original.status,
        abandon_state: original.abandon_state,
        expected_revision: original.expected_revision,
        dependency: None,
        payload_hash: original.payload_hash,
        payload: original.bytes,
        draft_generation: rig.approval.generation,
        draft: rig.approval.draft.clone(),
        base: rig.approval.base.clone(),
        base_revision: rig.approval.base_revision.clone(),
        view_fingerprint: rig.approval.view_fingerprint.clone(),
    };
    let before = snapshot(&rig.path).await;
    assert!(
        dismiss_frozen_content(&mut rig.store, &approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(
        rig.store
            .list_outbox(&rig.approval.current, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
}

#[tokio::test]
async fn frozen_content_dismiss_cannot_be_replaced_by_apply() {
    let mut rig = apply_rig().await;
    let original = rig
        .store
        .read_outbox_operation(&rig.approval.old, &rig.approval.old_operation_id)
        .await
        .unwrap()
        .unwrap();
    let approval = DismissApproval {
        current: rig.approval.current.clone(),
        old: rig.approval.old.clone(),
        document: DOC_A.into(),
        operation_id: original.operation_id,
        operation_seq: original.operation_seq,
        operation_generation: original.generation,
        operation_status: original.status,
        abandon_state: original.abandon_state,
        expected_revision: original.expected_revision,
        dependency: None,
        payload_hash: original.payload_hash,
        payload: original.bytes,
        draft_generation: rig.approval.generation,
        draft: rig.approval.draft.clone(),
        base: rig.approval.base.clone(),
        base_revision: rig.approval.base_revision.clone(),
        view_fingerprint: rig.approval.view_fingerprint.clone(),
    };
    dismiss_frozen_content(&mut rig.store, &approval)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert!(
        dismissal_status(
            &mut rig.store,
            &approval.current,
            &approval.old,
            DOC_A,
            &approval.operation_id
        )
        .await
        .unwrap()
    );
    assert!(
        rig.store
            .next_pending_outbox(&approval.current)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn frozen_content_dismiss_database_write_failure_preserves_decision_and_originals() {
    let mut rig = dismiss_rig("sent", false).await;
    let before = snapshot(&rig.path).await;
    let before_receipts = receipt_and_generation(&rig.path).await;
    let mut blocker =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
            .await
            .unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut blocker)
        .await
        .unwrap();
    assert!(
        dismiss_frozen_content(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    sqlx::query("ROLLBACK").execute(&mut blocker).await.unwrap();
    blocker.close().await.unwrap();
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(receipt_and_generation(&rig.path).await, before_receipts);
    assert!(
        !dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id
        )
        .await
        .unwrap()
    );
    dismiss_frozen_content(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    assert!(
        dismissal_status(
            &mut rig.store,
            &rig.approval.current,
            &rig.approval.old,
            DOC_A,
            &rig.approval.operation_id
        )
        .await
        .unwrap()
    );
    assert_eq!(snapshot(&rig.path).await, before);
}

#[tokio::test]
async fn frozen_content_dismiss_alone_does_not_resolve_the_latest_draft() {
    let mut rig = dismiss_rig("sent", false).await;
    dismiss_frozen_content(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    let old_draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_draft.bytes, b"newer checkpoint");
    let edit = rig
        .store
        .open_registered_document(
            &rig.approval.current,
            DOC_A,
            b"current base",
            "current-revision",
        )
        .await
        .unwrap();
    rig.store
        .save_registered_checkpoint(
            &rig.approval.current,
            DOC_A,
            &edit.edit_session_id,
            1,
            b"new current draft",
        )
        .await
        .unwrap();
    rig.store
        .close_registered_document(&rig.approval.current, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    assert!(matches!(
        rig.store
            .queue_checkpoint(&rig.approval.current, DOC_A, 1)
            .await,
        Err(StoreError::ReconciliationRequired)
    ));
    assert_eq!(snapshot(&rig.path).await, before);
}
