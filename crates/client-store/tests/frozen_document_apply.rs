include!("frozen_content_existing_listing.rs");

use notryx_client_store::{FrozenContentApplyApproval as ApplyApproval, StoreError};
use serde_json::json;

async fn apply_confirmed_frozen_draft(
    store: &mut ClientStore,
    approval: &ApplyApproval,
) -> Result<String, StoreError> {
    store.apply_frozen_content_approval(approval).await
}

struct ApplyRig {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: ClientStore,
    approval: ApplyApproval,
    other_operation_id: String,
}

async fn apply_rig() -> ApplyRig {
    apply_rig_with_successor(false).await
}

async fn apply_rig_with_successor(successor: bool) -> ApplyRig {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("frozen-draft-apply.sqlite");
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
        .save_registered_checkpoint(
            &old,
            DOC_A,
            &edit.edit_session_id,
            1,
            b"confirmed local draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&old, DOC_A, &edit.edit_session_id)
        .await
        .unwrap();
    store.queue_checkpoint(&old, DOC_A, 1).await.unwrap();
    let sent = store.prepare_outbox(&old).await.unwrap().unwrap();
    if successor {
        let next_edit = store
            .open_registered_document(&old, DOC_A, b"original base", "base-revision")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &old,
                DOC_A,
                &next_edit.edit_session_id,
                2,
                b"later local draft",
            )
            .await
            .unwrap();
        store
            .close_registered_document(&old, DOC_A, &next_edit.edit_session_id)
            .await
            .unwrap();
        store.queue_checkpoint(&old, DOC_A, 2).await.unwrap();
    }
    let other = queue(&mut store, &old, DOC_B, b"unrelated draft").await;
    store.activate_registration(&current).await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let current_id: i64 =
        sqlx::query_scalar("SELECT id FROM client_registrations WHERE status='active'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"1","last_change_seq":"1"});
    let node = json!({"node_id":DOC_A,"parent_id":ROOT,"name":"document.md","kind":"file","deleted":false,"content_rev":"remote-revision","content_etag":"\"remote-revision\"","meta_rev":"meta-1","meta_etag":"\"meta-1\"","node_etag":"\"node-1\"","content_hash":"sha256:c81662749eb30a2c7f6b6b9b1a561a7b0be5cc115759b72442ddb476e7712751","size":"7","children_generation":"0","last_change_seq":"1"});
    sqlx::query("INSERT INTO remote_index (registration_id,root_id,cursor) VALUES (?,?,'1')")
        .bind(current_id)
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    for (node_id, state) in [(ROOT, root), (DOC_A, node)] {
        sqlx::query("INSERT INTO remote_nodes (registration_id,node_id,state,content_ready) VALUES (?,?,?,0)")
            .bind(current_id).bind(node_id).bind(state.to_string()).execute(&mut db).await.unwrap();
    }
    db.close().await.unwrap();
    ApplyRig {
        _directory: directory,
        path,
        store,
        approval: ApplyApproval {
            old,
            current,
            document: DOC_A.into(),
            old_operation_id: sent.operation_id,
            old_operation_seq: sent.operation_seq,
            old_operation_hash: sent.payload_hash,
            old_operation_status: sent.status,
            generation: 1,
            draft: b"confirmed local draft".to_vec(),
            base: b"original base".to_vec(),
            base_revision: "base-revision".into(),
            remote_revision: "remote-revision".into(),
            remote_hash: "sha256:c81662749eb30a2c7f6b6b9b1a561a7b0be5cc115759b72442ddb476e7712751"
                .into(),
            remote_bytes: b"remote\n".to_vec(),
            remote_size: 7,
            remote_cursor: "1".into(),
            view_fingerprint:
                "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        },
        other_operation_id: other.operation_id,
    }
}

#[tokio::test]
async fn frozen_document_apply_requires_confirmation_and_is_atomic_idempotent_and_preserves_originals()
 {
    let mut rig = apply_rig().await;
    let before = snapshot(&rig.path).await;
    assert!(
        rig.store
            .next_pending_outbox(&rig.approval.current)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    assert_ne!(new_id, rig.approval.old_operation_id);
    let page = rig
        .store
        .list_outbox(&rig.approval.current, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].operation_id, new_id);
    assert_eq!(page.entries[0].operation_seq, 1);
    assert_eq!(page.entries[0].status, OutboxStatus::Queued);
    let new = rig
        .store
        .read_outbox_operation(&rig.approval.current, &new_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.bytes, rig.approval.draft);
    assert_eq!(
        new.expected_revision.as_deref(),
        Some(rig.approval.remote_revision.as_str())
    );
    assert_eq!(new.document_id, DOC_A);
    let old = rig
        .store
        .read_outbox_operation(&rig.approval.old, &rig.approval.old_operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.operation_seq, rig.approval.old_operation_seq);
    assert_eq!(old.payload_hash, rig.approval.old_operation_hash);
    assert_eq!(old.status, rig.approval.old_operation_status);
    assert_eq!(old.bytes, rig.approval.draft);
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, rig.approval.draft);
    assert_eq!(draft.base_bytes, rig.approval.base);
    assert_eq!(draft.base_revision, rig.approval.base_revision);
    let after = snapshot(&rig.path).await;
    assert!(
        after.0.contains(
            before
                .0
                .iter()
                .find(|row| row.0 == rig.other_operation_id)
                .unwrap()
        )
    );
    assert!(
        after.0.contains(
            before
                .0
                .iter()
                .find(|row| row.0 == rig.approval.old_operation_id)
                .unwrap()
        )
    );
    assert!(
        after.1.contains(
            before
                .1
                .iter()
                .find(|row| row.0 == rig.approval.old_operation_id)
                .unwrap()
        )
    );
    for row in &before.2 {
        assert!(after.2.contains(row));
    }
    for row in &before.3 {
        assert!(after.3.contains(row));
    }
    assert_eq!(after.4, before.4);
    assert_eq!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .unwrap(),
        new_id
    );
    assert_eq!(snapshot(&rig.path).await, after);
}

#[tokio::test]
async fn frozen_document_apply_rejects_changed_draft_or_remote_snapshot_and_open_session() {
    let mut rig = apply_rig().await;
    let before = snapshot(&rig.path).await;
    let mut stale = rig.approval.clone();
    stale.remote_revision = "stale-revision".into();
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &stale)
            .await
            .is_err()
    );
    let mut stale = rig.approval.clone();
    stale.generation += 1;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &stale)
            .await
            .is_err()
    );
    let mut stale = rig.approval.clone();
    stale.draft = b"other approved bytes".to_vec();
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &stale)
            .await
            .is_err()
    );
    let mut stale = rig.approval.clone();
    stale.old_operation_hash = "sha256:incorrect".into();
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &stale)
            .await
            .is_err()
    );
    let mut stale = rig.approval.clone();
    stale.remote_hash = "sha256:incorrect".into();
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &stale)
            .await
            .is_err()
    );
    let mut stale = rig.approval.clone();
    stale.remote_size = 8;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &stale)
            .await
            .is_err()
    );
    let mut stale = rig.approval.clone();
    stale.remote_cursor = "0".into();
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &stale)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let session = rig
        .store
        .open_recovery_draft(&rig.approval.old, DOC_A)
        .await
        .unwrap();
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    rig.store
        .close_recovery_draft(&rig.approval.old, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
    assert_eq!(snapshot(&rig.path).await, before);
}

#[tokio::test]
async fn frozen_document_apply_rolls_back_when_new_operation_insert_fails() {
    let mut rig = apply_rig().await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_active_outbox BEFORE INSERT ON client_outbox WHEN NEW.registration_id=(SELECT id FROM client_registrations WHERE status='active') BEGIN SELECT RAISE(ABORT,'injected'); END;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&rig.path))
        .await
        .unwrap();
    sqlx::raw_sql("DROP TRIGGER fail_active_outbox")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let new_id = apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
        .await
        .unwrap();
    assert!(!new_id.is_empty());
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
async fn frozen_document_apply_rejects_unreviewed_successor_and_preserves_both() {
    let mut rig = apply_rig_with_successor(true).await;
    let before = snapshot(&rig.path).await;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
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
async fn frozen_document_apply_rejects_later_durable_recovery_edit() {
    let mut rig = apply_rig().await;
    let session = rig
        .store
        .open_recovery_draft(&rig.approval.old, DOC_A)
        .await
        .unwrap();
    rig.store
        .save_recovery_checkpoint(
            &rig.approval.old,
            DOC_A,
            &session.edit_session_id,
            2,
            b"later durable edit",
        )
        .await
        .unwrap();
    rig.store
        .close_recovery_draft(&rig.approval.old, DOC_A, &session.edit_session_id)
        .await
        .unwrap();
    let before = snapshot(&rig.path).await;
    assert!(
        apply_confirmed_frozen_draft(&mut rig.store, &rig.approval)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&rig.path).await, before);
    let draft = rig
        .store
        .read_registered_checkpoint(&rig.approval.old, DOC_A)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"later durable edit");
}
