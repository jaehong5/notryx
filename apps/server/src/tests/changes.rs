use super::*;

#[tokio::test]
async fn changes_preserves_delete_and_parent_post_states() {
    let mut rig = Rig::new().await;
    let original = NodeSnapshot::from(&rig.initial);
    let delete = DeleteNode {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 1,
        operation_id: op_id(),
        node_id: original.node_id,
        expected_node_etag: original.etag().unwrap(),
    };
    let Outcome::Deleted(tombstone) = rig.workspace.delete_node(delete).await.unwrap().outcome
    else {
        panic!("expected tombstone");
    };
    let latest_parent = rig
        .workspace
        .node_state(rig.workspace.state.root_id)
        .await
        .unwrap();
    let page = rig
        .workspace
        .changes(ChangeSeq::new(1), Some(ChangeSeq::new(2)))
        .await
        .unwrap();
    assert_eq!(page.covered_through.get(), 2);
    assert!(!page.has_more);
    assert_eq!(page.nodes.len(), 2);
    assert!(page.nodes.contains(&tombstone.representation().unwrap()));
    assert!(page.nodes.contains(&latest_parent));
    assert!(
        page.nodes
            .iter()
            .all(|node| node.last_change_seq.get() == 2)
    );
}

#[tokio::test]
async fn changes_never_splits_a_large_changeset_or_exceeds_the_serialized_limit() {
    let mut rig = Rig::new().await;
    let meta_rev = "x".repeat(170 * 1024);
    sqlx::query("UPDATE node_changes SET post_state=json_set(post_state, '$.meta_rev', ?)")
        .bind(&meta_rev)
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES('2',NULL)")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO node_changes SELECT '2',node_id,json_set(post_state, '$.last_change_seq', '2') FROM node_changes WHERE seq='1'")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    sqlx::query("UPDATE workspaces SET head_seq='2'")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let first = rig
        .workspace
        .changes(ChangeSeq::new(0), Some(ChangeSeq::new(2)))
        .await
        .unwrap();
    assert_eq!(first.covered_through.get(), 1);
    assert!(first.has_more);
    assert_eq!(first.nodes.len(), 2);
    assert!(serde_json::to_vec(&first).unwrap().len() <= 1024 * 1024);
    let next = rig
        .workspace
        .changes(first.covered_through, Some(first.through_seq))
        .await
        .unwrap();
    assert_eq!(next.covered_through.get(), 2);
    assert!(!next.has_more);
    assert_eq!(next.nodes.len(), 2);
    assert!(serde_json::to_vec(&next).unwrap().len() <= 1024 * 1024);

    sqlx::query(
        "UPDATE node_changes SET post_state=json_set(post_state, '$.meta_rev', ?) WHERE seq='1'",
    )
    .bind("y".repeat(300 * 1024))
    .execute(&mut rig.workspace.state.connection)
    .await
    .unwrap();
    assert_eq!(
        rig.workspace
            .changes(ChangeSeq::new(0), Some(ChangeSeq::new(2)))
            .await,
        Err(Error::ChangesPageLimit)
    );
}

#[tokio::test]
async fn changes_counts_utf8_json_bytes_exactly_at_response_limit() {
    use sqlx::Connection;

    const LIMIT: usize = 1024 * 1024;
    let mut rig = Rig::new().await;
    let mut template = NodeSnapshot::from(&rig.initial);
    template.last_change_seq = ChangeSeq::new(2);
    template.name = format!("한\"{}", Uuid::now_v7());
    let mut expected = notryx_protocol::ChangesPage {
        workspace_epoch: rig.workspace.epoch(),
        from_exclusive: ChangeSeq::new(1),
        through_seq: ChangeSeq::new(2),
        covered_through: ChangeSeq::new(2),
        has_more: false,
        nodes: Vec::new(),
    };
    let empty_bytes = serde_json::to_vec(&expected).unwrap().len();
    let min_node_bytes = serde_json::to_vec(&template.representation().unwrap())
        .unwrap()
        .len();
    let max_padding = 255 - template.name.len();
    let target_sum = LIMIT - empty_bytes + 1;
    let count = target_sum.div_ceil(min_node_bytes + max_padding + 1);
    let mut padding = target_sum - count * (min_node_bytes + 1);
    assert!(padding <= count * max_padding);

    let mut snapshots = Vec::with_capacity(count);
    for _ in 0..count {
        let mut node = template.clone();
        node.node_id = Uuid::now_v7().to_string().parse().unwrap();
        let extra = padding.min(max_padding);
        padding -= extra;
        node.name = format!("한\"{}{}", node.node_id, "x".repeat(extra));
        expected.nodes.push(node.representation().unwrap());
        snapshots.push(node);
    }
    assert_eq!(padding, 0);
    assert_eq!(serde_json::to_vec(&expected).unwrap().len(), LIMIT);
    let mut tx = rig.workspace.state.connection.begin().await.unwrap();
    sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES('2',NULL)")
        .execute(&mut *tx)
        .await
        .unwrap();
    for node in &snapshots {
        sqlx::query("INSERT INTO nodes SELECT ?,parent_id,?,?,kind,content_rev,meta_rev,content_hash,size,children_generation,'2',0 FROM nodes WHERE node_id=?")
            .bind(node.node_id.to_string())
            .bind(&node.name)
            .bind(notryx_domain::mutation::normalized_name_key(&node.name))
            .bind(rig.initial.node_id.to_string())
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO node_changes VALUES('2',?,?)")
            .bind(node.node_id.to_string())
            .bind(serde_json::to_string(&crate::state::node::node_post_state(node)).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE workspaces SET head_seq='2'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let page = rig
        .workspace
        .changes(ChangeSeq::new(1), None)
        .await
        .unwrap();
    assert!(!page.has_more);
    assert_eq!(serde_json::to_vec(&page).unwrap().len(), LIMIT);
    assert_eq!(page.nodes.len(), count);

    let last = snapshots.last_mut().unwrap();
    last.name.push('x');
    sqlx::query("UPDATE node_changes SET post_state=? WHERE seq='2' AND node_id=?")
        .bind(serde_json::to_string(&crate::state::node::node_post_state(last)).unwrap())
        .bind(last.node_id.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    expected.nodes.last_mut().unwrap().name.push('x');
    assert_eq!(serde_json::to_vec(&expected).unwrap().len(), LIMIT + 1);
    assert_eq!(
        rig.workspace.changes(ChangeSeq::new(1), None).await,
        Err(Error::ChangesPageLimit)
    );

    sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES('3',NULL)")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO node_changes SELECT '3',node_id,json_set(post_state, '$.last_change_seq', '3') FROM node_changes WHERE seq='1' LIMIT 1")
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    sqlx::query("UPDATE workspaces SET head_seq='3'")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    expected.through_seq = ChangeSeq::new(3);
    expected.has_more = true;
    assert_eq!(serde_json::to_vec(&expected).unwrap().len(), LIMIT);
    let page = rig
        .workspace
        .changes(ChangeSeq::new(1), None)
        .await
        .unwrap();
    assert!(page.has_more);
    assert_eq!(page.covered_through.get(), 2);
    assert_eq!(serde_json::to_vec(&page).unwrap().len(), LIMIT);
}

#[tokio::test]
async fn changes_pages_are_ordered_by_numeric_seq_and_at_most_one_hundred_sets() {
    use sqlx::Connection;

    let mut rig = Rig::new().await;
    let post_state: String =
        sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq='1' AND node_id=?")
            .bind(rig.initial.node_id.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    let mut tx = rig.workspace.state.connection.begin().await.unwrap();
    for seq in 2..=102_u64 {
        sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES(?, NULL)")
            .bind(seq.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO node_changes VALUES(?,?,json_set(?, '$.last_change_seq', ?))")
            .bind(seq.to_string())
            .bind(rig.initial.node_id.to_string())
            .bind(&post_state)
            .bind(seq.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE workspaces SET head_seq='102'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let page = rig
        .workspace
        .changes(ChangeSeq::new(0), None)
        .await
        .unwrap();
    assert_eq!(page.through_seq.get(), 102);
    assert_eq!(page.covered_through.get(), 100);
    assert!(page.has_more);
    assert_eq!(page.nodes.len(), 101);
    let seqs: Vec<_> = page
        .nodes
        .iter()
        .map(|node| node.last_change_seq.get())
        .collect();
    assert_eq!(seqs, std::iter::once(1).chain(1..=100).collect::<Vec<_>>());
    sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES('103', NULL)")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO node_changes VALUES('103',?,json_set(?, '$.last_change_seq', '103'))")
        .bind(rig.initial.node_id.to_string())
        .bind(&post_state)
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("UPDATE workspaces SET head_seq='103'")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let last = rig
        .workspace
        .changes(page.covered_through, Some(page.through_seq))
        .await
        .unwrap();
    assert_eq!(last.covered_through.get(), 102);
    assert!(!last.has_more);
    assert_eq!(
        last.nodes
            .iter()
            .map(|node| node.last_change_seq.get())
            .collect::<Vec<_>>(),
        vec![101, 102]
    );
}

#[tokio::test]
async fn changes_defers_oversized_record_after_complete_sets() {
    let mut rig = Rig::new().await;
    sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES('2',NULL)")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO node_changes SELECT '2',node_id,json_set(post_state, '$.last_change_seq', '2', '$.meta_rev', ?) FROM node_changes WHERE seq='1' LIMIT 1")
        .bind("z".repeat(1024 * 1024 + 1))
        .execute(&mut rig.workspace.state.connection).await.unwrap();
    sqlx::query("UPDATE workspaces SET head_seq='2'")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let first = rig
        .workspace
        .changes(ChangeSeq::new(0), None)
        .await
        .unwrap();
    assert_eq!(first.covered_through.get(), 1);
    assert!(first.has_more);
    assert_eq!(
        rig.workspace
            .changes(first.covered_through, Some(first.through_seq))
            .await,
        Err(Error::ChangesPageLimit)
    );
}

#[tokio::test]
async fn changes_rejects_missing_and_empty_sets() {
    for empty in [false, true] {
        let mut rig = Rig::new().await;
        if empty {
            sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES('2',NULL)")
                .execute(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE workspaces SET head_seq='2'")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(
            rig.workspace.changes(ChangeSeq::new(0), None).await,
            Err(Error::InvalidFormat)
        );
        assert_eq!(rig.workspace.state.head_seq().await.unwrap().get(), 2);
    }
}

#[tokio::test]
async fn changes_preserves_full_u64_sequence_precision() {
    let mut rig = Rig::new().await;
    let post_state: String =
        sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq='1' AND node_id=?")
            .bind(rig.initial.node_id.to_string())
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    for seq in [u64::MAX - 2, u64::MAX - 1, u64::MAX] {
        sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES(?, NULL)")
            .bind(seq.to_string())
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO node_changes VALUES(?,?,json_set(?, '$.last_change_seq', ?))")
            .bind(seq.to_string())
            .bind(rig.initial.node_id.to_string())
            .bind(&post_state)
            .bind(seq.to_string())
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE workspaces SET head_seq=?")
        .bind(u64::MAX.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let page = rig
        .workspace
        .changes(ChangeSeq::new(u64::MAX - 3), None)
        .await
        .unwrap();
    assert_eq!(page.through_seq.get(), u64::MAX);
    assert_eq!(page.covered_through.get(), u64::MAX);
    assert!(!page.has_more);
    assert_eq!(
        page.nodes
            .iter()
            .map(|node| node.last_change_seq.get())
            .collect::<Vec<_>>(),
        vec![u64::MAX - 2, u64::MAX - 1, u64::MAX]
    );
}

#[tokio::test]
async fn changes_rejects_corrupt_journal_records_instead_of_advancing_cursor() {
    for corruption in [
        "UPDATE node_changes SET post_state='not json' WHERE seq='1'",
        "UPDATE node_changes SET post_state=json_set(post_state, '$.last_change_seq', '2') WHERE seq='1'",
        "UPDATE node_changes SET post_state=json_set(post_state, '$.node_id', '00000000-0000-7000-8000-000000000123') WHERE seq='1'",
    ] {
        let mut rig = Rig::new().await;
        sqlx::query(corruption)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(
            rig.workspace.changes(ChangeSeq::new(0), None).await,
            Err(Error::InvalidFormat)
        );
        assert_eq!(rig.workspace.state.head_seq().await.unwrap().get(), 1);
    }
}

#[tokio::test]
async fn changes_rejects_empty_journal_node_id_alongside_valid_records() {
    let mut rig = Rig::new().await;
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO node_changes(seq,node_id,post_state) VALUES('1','','{}')")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.changes(ChangeSeq::new(0), None).await,
        Err(Error::InvalidFormat)
    );
}
