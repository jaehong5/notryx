include!("content_conflict_intent.rs");

#[tokio::test]
async fn recording_intent_after_remote_changes_or_deletes_requires_a_new_snapshot() {
    for deleted in [false, true] {
        let (rig, mut store) = rig().await;
        let (mut root, original) = remote_nodes();
        let mut value = serde_json::to_value(original).unwrap();
        value["last_change_seq"] = json!("3");
        let nodes = if deleted {
            value["deleted"] = json!(true);
            value["content_rev"] = json!("deleted-content-rev");
            value["content_etag"] = json!("\"deleted-content-rev\"");
            value["meta_rev"] = json!("deleted-meta");
            value["meta_etag"] = json!("\"deleted-meta\"");
            value["node_etag"] = json!("\"deleted-node\"");
            root.children_generation = ChangeSeq::new(2);
            root.last_change_seq = ChangeSeq::new(3);
            vec![root, serde_json::from_value::<NodeState>(value).unwrap()]
        } else {
            value["content_rev"] = json!("later-rev");
            value["content_etag"] = json!("\"later-rev\"");
            value["node_etag"] = json!("\"later-node\"");
            value["content_hash"] = json!(digest(b"later remote\n"));
            value["size"] = json!("13");
            vec![serde_json::from_value::<NodeState>(value).unwrap()]
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
        assert!(
            record(&mut store, &rig, &rig.request).await.is_err(),
            "deleted={deleted}"
        );
        assert!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .unwrap()
                .is_none()
        );
        assert_originals(&mut store, &rig).await;
    }
}

#[tokio::test]
async fn identical_intent_retry_after_new_local_and_remote_returns_original_decision() {
    let (rig, mut store) = rig().await;
    let intent = record(&mut store, &rig, &rig.request).await.unwrap();
    store
        .save_registered_checkpoint(&rig.binding, DOC, &rig.session, 3, b"newer local draft\n")
        .await
        .unwrap();
    let (_, original) = remote_nodes();
    let mut value = serde_json::to_value(original).unwrap();
    value["content_rev"] = json!("later-rev");
    value["content_etag"] = json!("\"later-rev\"");
    value["node_etag"] = json!("\"later-node\"");
    value["content_hash"] = json!(digest(b"later remote\n"));
    value["size"] = json!("13");
    value["last_change_seq"] = json!("3");
    let current: NodeState = serde_json::from_value(value).unwrap();
    let page = ChangesPage {
        workspace_epoch: info(&rig.binding).workspace_epoch,
        from_exclusive: ChangeSeq::new(2),
        through_seq: ChangeSeq::new(3),
        covered_through: ChangeSeq::new(3),
        has_more: false,
        nodes: vec![current.clone()],
    };
    store
        .apply_changes(&rig.binding, &page, ChangeSeq::new(3))
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&rig.binding, &current, b"later remote\n")
            .await
            .unwrap()
    );
    assert_eq!(
        record(&mut store, &rig, &rig.request).await.unwrap(),
        intent
    );
    assert_eq!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(intent)
    );
    let draft = store
        .read_registered_checkpoint(&rig.binding, DOC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer local draft\n");
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        SENT
    );
    assert_eq!(
        store
            .read_outbox_operation(&rig.binding, &rig.followup)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        LOCAL
    );
    assert_eq!(store.receipt_ack_through(&rig.binding).await.unwrap(), 1);
}

#[tokio::test]
async fn corrupted_persisted_intent_is_rejected_and_originals_are_preserved() {
    for damage in [
        "remote_hash",
        "remote_bytes",
        "base_revision",
        "document_id",
        "generation",
    ] {
        let (rig, mut store) = rig().await;
        record(&mut store, &rig, &rig.request).await.unwrap();
        store
            .close_registered_document(&rig.binding, DOC, &rig.session)
            .await
            .unwrap();
        store.close().await.unwrap();
        let mut db = connect(&rig.path).await;
        let sql = match damage {
            "remote_hash" => {
                "UPDATE content_conflict_intents SET remote_hash='sha256:bad' WHERE operation_id=?"
            }
            "remote_bytes" => {
                "UPDATE content_conflict_intents SET remote_bytes=X'636f7272757074' WHERE operation_id=?"
            }
            "base_revision" => {
                "UPDATE content_conflict_intents SET base_revision='other-rev' WHERE operation_id=?"
            }
            "document_id" => {
                "UPDATE content_conflict_intents SET document_id='019a1000-0000-7000-8000-000000000003' WHERE operation_id=?"
            }
            "generation" => "UPDATE content_conflict_intents SET generation=1 WHERE operation_id=?",
            _ => unreachable!(),
        };
        sqlx::query(sql)
            .bind(&rig.operation)
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        let mut store = ClientStore::open(&rig.path).await.unwrap();
        assert!(
            store
                .read_content_conflict_intent(&rig.binding, &rig.operation)
                .await
                .is_err(),
            "{damage}"
        );
        assert_originals(&mut store, &rig).await;
    }
}
