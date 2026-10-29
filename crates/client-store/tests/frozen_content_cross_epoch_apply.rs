include!("frozen_document_apply.rs");

use notryx_client_store::FrozenContentCrossEpochConfirmation as CrossEpochConfirmation;

async fn apply_across_epochs(
    store: &mut ClientStore,
    approval: &ApplyApproval,
    confirmation: Option<&CrossEpochConfirmation>,
) -> Result<String, StoreError> {
    match confirmation {
        Some(confirmation) => {
            store
                .apply_frozen_content_across_epochs(approval, confirmation)
                .await
        }
        None => store.apply_frozen_content_approval(approval).await,
    }
}

async fn cross_epoch_rig(successor: bool) -> (ApplyRig, CrossEpochConfirmation) {
    let mut rig = apply_rig_with_successor(successor).await;
    // Synthetic registration epoch change models a prior restored epoch, not an actual server restore.
    let old_epoch = Uuid::new_v4().to_string();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&rig.path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let changed = sqlx::query("UPDATE client_registrations SET workspace_epoch=? WHERE device_stream_id=? AND status='frozen'")
        .bind(&old_epoch).bind(&rig.approval.old.device_stream_id).execute(&mut db).await.unwrap();
    assert_eq!(changed.rows_affected(), 1);
    db.close().await.unwrap();
    rig.approval.old.workspace_epoch = old_epoch.clone();
    let confirmation = CrossEpochConfirmation {
        selected_current_document: DOC_A.into(),
        current_parent: ROOT.into(),
        current_name: "document.md".into(),
        displayed_path: "/document.md".into(),
        old_epoch,
        current_epoch: rig.approval.current.workspace_epoch.clone(),
        remote_revision: rig.approval.remote_revision.clone(),
        remote_hash: rig.approval.remote_hash.clone(),
        explicitly_confirmed: true,
    };
    (rig, confirmation)
}

async fn indexed_node(path: &Path) -> String {
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM remote_nodes WHERE node_id=? AND registration_id=(SELECT id FROM client_registrations WHERE status='active')")
        .bind(DOC_A).fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();
    state
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_requires_explicit_current_target_and_preserves_old_data()
{
    let (mut rig, confirmation) = cross_epoch_rig(false).await;
    let before = snapshot(&rig.path).await;
    let node = indexed_node(&rig.path).await;
    assert_ne!(
        rig.approval.old.workspace_epoch,
        rig.approval.current.workspace_epoch
    );
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert!(
        apply_across_epochs(&mut rig.store, &rig.approval, None)
            .await
            .is_err()
    );
    let mut unconfirmed = confirmation.clone();
    unconfirmed.explicitly_confirmed = false;
    assert!(
        apply_across_epochs(&mut rig.store, &rig.approval, Some(&unconfirmed))
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let id = apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
        .await
        .unwrap();
    assert_ne!(id, rig.approval.old_operation_id);
    let new = rig
        .store
        .read_outbox_operation(&rig.approval.current, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.operation_seq, 1);
    assert_eq!(new.status, OutboxStatus::Queued);
    assert_eq!(new.bytes, rig.approval.draft);
    assert_eq!(
        new.expected_revision.as_deref(),
        Some(rig.approval.remote_revision.as_str())
    );
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.current, DOC_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.base_bytes, rig.approval.remote_bytes);
    assert_eq!(draft.base_revision, rig.approval.remote_revision);
    assert_eq!(indexed_node(&rig.path).await, node);
    let after = snapshot(&rig.path).await;
    for row in &before.0 {
        assert!(after.0.contains(row));
    }
    for row in &before.1 {
        assert!(after.1.contains(row));
    }
    for row in &before.2 {
        assert!(after.2.contains(row));
    }
    for row in &before.3 {
        assert!(after.3.contains(row));
    }
    assert_eq!(after.4, before.4);
    assert_eq!(
        apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
            .await
            .unwrap(),
        id
    );
    assert_eq!(snapshot(&rig.path).await, after);
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_same_bytes_and_node_id_do_not_approve_automatically() {
    let (mut rig, mut confirmation) = cross_epoch_rig(false).await;
    rig.approval.remote_bytes = rig.approval.draft.clone();
    rig.approval.remote_size = rig.approval.remote_bytes.len() as u64;
    rig.approval.remote_hash = rig.approval.old_operation_hash.clone();
    confirmation.remote_hash = rig.approval.remote_hash.clone();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::query("UPDATE remote_nodes SET state=json_set(state,'$.content_hash',?,'$.size',?) WHERE node_id=? AND registration_id=(SELECT id FROM client_registrations WHERE status='active')")
        .bind(&rig.approval.remote_hash).bind(rig.approval.remote_size.to_string()).bind(DOC_A).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    let node = indexed_node(&rig.path).await;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert!(
        apply_across_epochs(&mut rig.store, &rig.approval, None)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert_eq!(indexed_node(&rig.path).await, node);
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_rejects_inconsistent_confirmation_and_changed_target() {
    let (mut rig, confirmation) = cross_epoch_rig(false).await;
    let before = snapshot(&rig.path).await;
    let mut bad = Vec::new();
    let mut value = confirmation.clone();
    value.selected_current_document = DOC_B.into();
    bad.push(value);
    let mut value = confirmation.clone();
    value.current_parent = DOC_B.into();
    bad.push(value);
    let mut value = confirmation.clone();
    value.current_name = "different.md".into();
    bad.push(value);
    let mut value = confirmation.clone();
    value.displayed_path = "/different.md".into();
    bad.push(value);
    let mut value = confirmation.clone();
    value.old_epoch = rig.approval.current.workspace_epoch.clone();
    bad.push(value);
    let mut value = confirmation.clone();
    value.current_epoch = rig.approval.old.workspace_epoch.clone();
    bad.push(value);
    let mut value = confirmation.clone();
    value.remote_revision.push('x');
    bad.push(value);
    let mut value = confirmation.clone();
    value.remote_hash.push('x');
    bad.push(value);
    for item in bad {
        assert!(
            apply_across_epochs(&mut rig.store, &rig.approval, Some(&item))
                .await
                .is_err()
        );
    }
    assert_eq!(snapshot(&rig.path).await, before);
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::query("UPDATE remote_nodes SET state=json_set(state,'$.name','renamed.md') WHERE node_id=? AND registration_id=(SELECT id FROM client_registrations WHERE status='active')")
        .bind(DOC_A).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let changed = snapshot(&rig.path).await;
    let node = indexed_node(&rig.path).await;
    assert!(
        apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, changed);
    assert_eq!(indexed_node(&rig.path).await, node);
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_preserves_unreviewed_successor_and_other_workspace() {
    let (mut rig, confirmation) = cross_epoch_rig(true).await;
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    rig.approval.generation = draft.generation;
    rig.approval.draft = draft.bytes;
    let before = snapshot(&rig.path).await;
    assert!(
        apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
            .await
            .is_err()
    );
    let mut foreign = rig.approval.clone();
    foreign.current.workspace_id = Uuid::new_v4().to_string();
    assert!(
        apply_across_epochs(&mut rig.store, &foreign, Some(&confirmation))
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_rejects_open_session_and_rolls_back_failed_insert() {
    let (mut rig, confirmation) = cross_epoch_rig(false).await;
    let session = rig
        .store
        .open_recovery_draft(&rig.approval.old, DOC_A)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
            .await
            .is_err()
    );
    rig.store
        .close_recovery_draft(&rig.approval.old, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
    assert_eq!(snapshot(&rig.path).await, before);
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_cross_epoch_content BEFORE INSERT ON client_outbox WHEN NEW.registration_id=(SELECT id FROM client_registrations WHERE status='active') BEGIN SELECT RAISE(ABORT,'injected'); END;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(
        apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::raw_sql("DROP TRIGGER fail_cross_epoch_content")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(
        !apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn frozen_content_cross_epoch_apply_rejects_deleted_or_changed_remote_before_decision() {
    for mode in ["deleted", "revision"] {
        let (mut rig, confirmation) = cross_epoch_rig(false).await;
        let mut db =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
                .await
                .unwrap();
        match mode {
            "deleted" => sqlx::query("UPDATE remote_nodes SET state=json_set(state,'$.deleted',json('true')) WHERE node_id=? AND registration_id=(SELECT id FROM client_registrations WHERE status='active')")
                .bind(DOC_A).execute(&mut db).await.unwrap(),
            "revision" => sqlx::query("UPDATE remote_nodes SET state=json_set(state,'$.content_rev',?,'$.content_etag',?) WHERE node_id=? AND registration_id=(SELECT id FROM client_registrations WHERE status='active')")
                .bind("later-revision").bind("\"later-revision\"").bind(DOC_A).execute(&mut db).await.unwrap(),
            _ => unreachable!(),
        };
        db.close().await.unwrap();
        let before = snapshot(&rig.path).await;
        let node = indexed_node(&rig.path).await;
        assert!(
            apply_across_epochs(&mut rig.store, &rig.approval, Some(&confirmation))
                .await
                .is_err(),
            "mode {mode}"
        );
        assert_eq!(snapshot(&rig.path).await, before);
        assert_eq!(indexed_node(&rig.path).await, node);
        assert!(
            rig.store
                .next_pending_outbox(&rig.approval.current)
                .await
                .unwrap()
                .is_none()
        );
    }
}
