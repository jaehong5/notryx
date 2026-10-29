include!("content_conflict_intent.rs");

const LATER: &[u8] = b"new checkpoint after review\n";

async fn reviewed(
    store: &mut ClientStore,
    rig: &Rig,
) -> (ContentConflictIntent, ContentConflictIntentRequest) {
    let old = record(store, rig, &rig.request).await.unwrap();
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, LATER)
        .await
        .unwrap();
    let mut request = rig.request.clone();
    request.generation = 3;
    request.choice = ContentConflictChoice::ChooseRemote;
    (old, request)
}

async fn replace(
    store: &mut ClientStore,
    rig: &Rig,
    expected: &ContentConflictIntent,
    request: &ContentConflictIntentRequest,
) -> Result<ContentConflictIntent, StoreError> {
    store
        .replace_pending_content_conflict_intent(
            &rig.binding,
            DOC,
            &rig.operation,
            expected,
            request,
        )
        .await
}

async fn history(store: &mut ClientStore, rig: &Rig) -> Vec<ContentConflictIntent> {
    let mut entries = Vec::new();
    let mut version = 0_u64;
    while let Some(entry) = store
        .read_content_conflict_intent_history(&rig.binding, &rig.operation, version)
        .await
        .unwrap()
    {
        entries.push(entry);
        version = version.checked_add(1).unwrap();
    }
    entries
}

async fn preserved_outbox(store: &mut ClientStore, rig: &Rig) {
    let failed = store
        .read_outbox_operation(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let followup = store
        .read_outbox_operation(&rig.binding, &rig.followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.operation_seq, 1);
    assert_eq!(failed.status, notryx_client_store::OutboxStatus::Conflict);
    assert_eq!(failed.bytes, SENT);
    assert_eq!(failed.expected_revision.as_deref(), Some("base-rev"));
    assert_eq!(followup.operation_seq, 2);
    assert_eq!(followup.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(followup.bytes, LOCAL);
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 3);
    assert_eq!(draft.bytes, LATER);
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(draft.base_revision, "base-rev");
}

#[tokio::test]
async fn replace_preserves_old_merged_choice_and_retries_without_duplicate_history() {
    let (rig, mut store) = rig().await;
    let (old, request) = reviewed(&mut store, &rig).await;
    let newer = replace(&mut store, &rig, &old, &request).await.unwrap();
    assert_eq!(newer.generation, 3);
    assert_eq!(newer.local_bytes, LATER);
    assert_eq!(newer.base_bytes, BASE);
    assert_eq!(newer.remote_bytes, REMOTE);
    assert_eq!(newer.choice, ContentConflictChoice::ChooseRemote);
    assert_eq!(history(&mut store, &rig).await, vec![old.clone()]);
    assert_eq!(
        replace(&mut store, &rig, &old, &request).await.unwrap(),
        newer
    );
    assert_eq!(history(&mut store, &rig).await, vec![old.clone()]);
    preserved_outbox(&mut store, &rig).await;
    store.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(history(&mut store, &rig).await, vec![old.clone()]);
    assert_eq!(
        replace(&mut store, &rig, &old, &request).await.unwrap(),
        newer
    );
    preserved_outbox(&mut store, &rig).await;
}

#[tokio::test]
async fn stale_expected_intent_cannot_replace_a_later_choice_or_erase_history() {
    let (rig, mut store) = rig().await;
    let (old, request) = reviewed(&mut store, &rig).await;
    let newer = replace(&mut store, &rig, &old, &request).await.unwrap();
    let mut another = request.clone();
    another.choice = ContentConflictChoice::KeepLocalCopy;
    let last = replace(&mut store, &rig, &newer, &another).await.unwrap();
    assert!(replace(&mut store, &rig, &old, &request).await.is_err());
    assert!(replace(&mut store, &rig, &old, &another).await.is_err());
    assert_eq!(history(&mut store, &rig).await, vec![old, newer]);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(last)
    );
    preserved_outbox(&mut store, &rig).await;
}

#[tokio::test]
async fn replacement_revalidates_generation_and_current_remote_or_deletion() {
    for case in ["generation", "changed", "deleted"] {
        let (rig, mut store) = rig().await;
        let (old, request) = reviewed(&mut store, &rig).await;
        if case == "generation" {
            store
                .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 4, b"even later\n")
                .await
                .unwrap();
        } else {
            let (mut root, original) = remote_nodes();
            let mut node = serde_json::to_value(original).unwrap();
            node["last_change_seq"] = json!("3");
            let nodes = if case == "deleted" {
                node["deleted"] = json!(true);
                node["content_rev"] = json!("deleted-content-rev");
                node["content_etag"] = json!("\"deleted-content-rev\"");
                node["meta_rev"] = json!("deleted-meta");
                node["meta_etag"] = json!("\"deleted-meta\"");
                node["node_etag"] = json!("\"deleted-node\"");
                root.children_generation = ChangeSeq::new(2);
                root.last_change_seq = ChangeSeq::new(3);
                vec![root, serde_json::from_value::<NodeState>(node).unwrap()]
            } else {
                node["content_rev"] = json!("later-rev");
                node["content_etag"] = json!("\"later-rev\"");
                node["node_etag"] = json!("\"later-node\"");
                node["content_hash"] = json!(digest(b"later remote\n"));
                node["size"] = json!("13");
                vec![serde_json::from_value::<NodeState>(node).unwrap()]
            };
            let page = ChangesPage {
                workspace_epoch: info(&rig.binding).workspace_epoch,
                from_exclusive: ChangeSeq::new(2),
                through_seq: ChangeSeq::new(3),
                covered_through: ChangeSeq::new(3),
                has_more: false,
                nodes,
            };
            store
                .apply_changes(&rig.binding, &page, ChangeSeq::new(3))
                .await
                .unwrap();
        }
        assert!(
            replace(&mut store, &rig, &old, &request).await.is_err(),
            "{case}"
        );
        assert_eq!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap(),
            Some(old)
        );
        assert!(history(&mut store, &rig).await.is_empty());
        let failed = store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap();
        let followup = store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((failed.operation_seq, followup.operation_seq), (1, 2));
        assert_eq!(failed.bytes, SENT);
        assert_eq!(followup.bytes, LOCAL);
        let draft = store
            .read_registered_checkpoint(&rig.binding, DOC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            draft.bytes,
            if case == "generation" {
                b"even later\n".as_slice()
            } else {
                LATER
            }
        );
        assert_eq!(draft.base_bytes, BASE);
    }
}

#[tokio::test]
async fn failed_current_update_rolls_back_inserted_history_and_keeps_outbox() {
    let (rig, mut store) = rig().await;
    let (old, request) = reviewed(&mut store, &rig).await;
    let mut db = connect(&rig.path).await;
    sqlx::raw_sql("CREATE TRIGGER reject_replace BEFORE UPDATE ON content_conflict_intents BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(replace(&mut store, &rig, &old, &request).await.is_err());
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(old.clone())
    );
    assert!(history(&mut store, &rig).await.is_empty());
    preserved_outbox(&mut store, &rig).await;
    store.close().await.unwrap();
    let mut db = connect(&rig.path).await;
    sqlx::query("DROP TRIGGER reject_replace")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(
        replace(&mut store, &rig, &old, &request)
            .await
            .unwrap()
            .local_bytes,
        LATER
    );
    assert_eq!(history(&mut store, &rig).await, vec![old]);
    preserved_outbox(&mut store, &rig).await;
}

#[tokio::test]
async fn replacement_registration_freezes_both_current_and_history_without_reattachment() {
    let (rig, mut store) = rig().await;
    let (old, request) = reviewed(&mut store, &rig).await;
    let newer = replace(&mut store, &rig, &old, &request).await.unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let mut replacement = binding();
    replacement.workspace_id = rig.binding.workspace_id.clone();
    replacement.workspace_epoch = rig.binding.workspace_epoch.clone();
    store.activate_registration(&replacement).await.unwrap();
    assert!(replace(&mut store, &rig, &newer, &request).await.is_err());
    assert!(
        store
            .replace_pending_content_conflict_intent(
                &replacement,
                DOC,
                &rig.operation,
                &newer,
                &request
            )
            .await
            .is_err()
    );
    assert_eq!(history(&mut store, &rig).await, vec![old]);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(newer)
    );
    assert!(
        store
            .read_content_conflict_intent_history(&replacement, &rig.operation, 0)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .frozen
    );
    assert!(
        store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap()
            .frozen
    );
    assert!(
        store
            .next_pending_outbox(&replacement)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn completed_remote_choice_cannot_be_replaced() {
    let (rig, mut store) = rig().await;
    let (old, request) = reviewed(&mut store, &rig).await;
    let newer = replace(&mut store, &rig, &old, &request).await.unwrap();
    let marked = store
        .request_content_conflict_followup_abandon(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    let sent = store
        .prepare_outbox_matching(&rig.binding, &marked)
        .await
        .unwrap()
        .unwrap();
    store
        .apply_outbox_abandoned(
            &rig.binding,
            &notryx_client_store::OutboxAbandoned {
                operation_id: sent.operation_id,
                operation_seq: sent.operation_seq,
                workspace_epoch: rig.binding.workspace_epoch.clone(),
            },
        )
        .await
        .unwrap();
    let ack = serde_json::from_value(json!({"workspace_epoch":rig.binding.workspace_epoch,
        "device_stream_id":rig.binding.device_stream_id,"last_seq":"2","retired_through":"2"}))
    .unwrap();
    store.confirm_receipt_ack(&rig.binding, &ack).await.unwrap();
    store
        .close_registered_document(&rig.binding, DOC, &rig.session)
        .await
        .unwrap();
    let completed = store
        .complete_content_conflict_choose_remote(&rig.binding, &rig.operation)
        .await
        .unwrap();
    let mut changed = request.clone();
    changed.generation = completed.generation;
    changed.choice = ContentConflictChoice::KeepLocalCopy;
    assert!(replace(&mut store, &rig, &newer, &changed).await.is_err());
    assert!(replace(&mut store, &rig, &old, &request).await.is_err());
    assert_eq!(history(&mut store, &rig).await, vec![old]);
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(newer)
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, REMOTE);
    assert_eq!(draft.base_bytes, REMOTE);
    assert_eq!(draft.base_revision, "current-rev");
}
