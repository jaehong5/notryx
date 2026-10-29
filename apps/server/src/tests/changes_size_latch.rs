use super::*;
use sqlx::Connection;

const LIMIT: usize = 1024 * 1024;

async fn receipts(rig: &mut Rig) -> Vec<(String, String, String, String)> {
    sqlx::query_as("SELECT operation_id,stream_id,operation_seq,outcome FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut rig.workspace.state.connection)
        .await
        .unwrap()
}

#[tokio::test]
async fn changes_size_latch_page_boundary_does_not_block_later_valid_page_or_save() {
    let mut rig = Rig::new().await;
    let initial_receipt = rig
        .workspace
        .save(rig.command(b"synthetic saved\n"))
        .await
        .unwrap();
    let Outcome::Saved(saved) = initial_receipt.outcome else {
        panic!("synthetic save must succeed")
    };
    assert_eq!(saved.last_change_seq.get(), 2);
    let mut template = NodeSnapshot::from(&saved);
    template.last_change_seq = ChangeSeq::new(3);
    template.name = format!("한\"{}", Uuid::now_v7());
    let mut parent = rig
        .workspace
        .state
        .node(saved.parent_id)
        .await
        .unwrap()
        .unwrap();
    parent.children_generation = parent.children_generation.checked_add(1).unwrap();
    parent.last_change_seq = ChangeSeq::new(3);
    let mut expected = notryx_protocol::ChangesPage {
        workspace_epoch: rig.workspace.epoch(),
        from_exclusive: ChangeSeq::new(2),
        through_seq: ChangeSeq::new(3),
        covered_through: ChangeSeq::new(3),
        has_more: false,
        nodes: vec![parent.representation().unwrap()],
    };
    let empty_bytes = serde_json::to_vec(&expected).unwrap().len();
    let min_node_bytes = serde_json::to_vec(&template.representation().unwrap())
        .unwrap()
        .len();
    let max_padding = 255 - template.name.len();
    let target_sum = LIMIT + 1 - empty_bytes;
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
    assert_eq!(serde_json::to_vec(&expected).unwrap().len(), LIMIT + 1);
    let mut tx = rig.workspace.state.connection.begin().await.unwrap();
    sqlx::query("INSERT INTO change_sets(seq,operation_id) VALUES('3',NULL)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET children_generation=?,last_change_seq='3' WHERE node_id=?")
        .bind(parent.children_generation.to_string())
        .bind(parent.node_id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO node_changes VALUES('3',?,?)")
        .bind(parent.node_id.to_string())
        .bind(serde_json::to_string(&crate::state::node::node_post_state(&parent)).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    for node in &snapshots {
        sqlx::query("INSERT INTO nodes SELECT ?,parent_id,?,?,kind,content_rev,meta_rev,content_hash,size,children_generation,'3',0 FROM nodes WHERE node_id=?")
            .bind(node.node_id.to_string())
            .bind(&node.name)
            .bind(notryx_domain::mutation::normalized_name_key(&node.name))
            .bind(saved.node_id.to_string())
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO node_changes VALUES('3',?,?)")
            .bind(node.node_id.to_string())
            .bind(serde_json::to_string(&crate::state::node::node_post_state(node)).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE workspaces SET head_seq='3'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let before_notes = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let before_receipts = receipts(&mut rig).await;
    assert_eq!(before_receipts.len(), 1);
    let before_posts: Vec<(String, String)> = sqlx::query_as(
        "SELECT node_id,post_state FROM node_changes WHERE seq='3' ORDER BY node_id",
    )
    .fetch_all(&mut rig.workspace.state.connection)
    .await
    .unwrap();
    assert_eq!(before_posts.len(), count + 1);
    assert!(
        before_posts
            .iter()
            .any(|(id, _)| id == &parent.node_id.to_string())
    );
    assert_eq!(
        rig.workspace.changes(ChangeSeq::new(2), None).await,
        Err(Error::ChangesPageLimit)
    );
    assert!(
        rig.workspace.ready(),
        "an oversized response is not confirmed journal corruption"
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        before_notes
    );
    assert_eq!(receipts(&mut rig).await, before_receipts);
    let after_posts: Vec<(String, String)> = sqlx::query_as(
        "SELECT node_id,post_state FROM node_changes WHERE seq='3' ORDER BY node_id",
    )
    .fetch_all(&mut rig.workspace.state.connection)
    .await
    .unwrap();
    assert_eq!(after_posts, before_posts);

    let command = SaveContent {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: 2,
        operation_id: op_id(),
        node_id: saved.node_id,
        expected_content_rev: saved.content_rev.clone(),
        content_hash: digest(b"synthetic next\n"),
        bytes: b"synthetic next\n".to_vec(),
    };
    let Outcome::Saved(next) = rig.workspace.save(command).await.unwrap().outcome else {
        panic!("subsequent save must succeed")
    };
    assert_eq!(next.last_change_seq.get(), 4);
    expected.through_seq = ChangeSeq::new(4);
    expected.has_more = true;
    assert_eq!(serde_json::to_vec(&expected).unwrap().len(), LIMIT);
    let page = rig
        .workspace
        .changes(ChangeSeq::new(2), None)
        .await
        .unwrap();
    assert_eq!(page.covered_through.get(), 3);
    assert!(page.has_more);
    assert_eq!(page.nodes.len(), count + 1);
    assert_eq!(serde_json::to_vec(&page).unwrap().len(), LIMIT);
    let after_receipts = receipts(&mut rig).await;
    assert_eq!(after_receipts.len(), 2);
    assert!(after_receipts.contains(&before_receipts[0]));
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        b"synthetic next\n"
    );
}

#[tokio::test]
async fn changes_size_latch_malformed_post_state_still_blocks_writes() {
    let mut rig = Rig::new().await;
    let initial_bytes = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    let receipts_before = receipts(&mut rig).await;
    sqlx::query("UPDATE node_changes SET post_state='not json' WHERE seq='1' AND node_id=?")
        .bind(rig.initial.node_id.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(
        rig.workspace.changes(ChangeSeq::new(0), None).await,
        Err(Error::InvalidFormat)
    );
    assert!(!rig.workspace.ready());
    assert_eq!(
        rig.workspace.save(rig.command(b"must not replace\n")).await,
        Err(Error::RecoveryRequired)
    );
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        initial_bytes
    );
    assert_eq!(receipts(&mut rig).await, receipts_before);
}
