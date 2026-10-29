include!("journal_retention_budget.rs");

#[derive(Debug, PartialEq, Eq)]
struct JournalRows {
    change_sets: Vec<(String, Option<String>, Option<i64>)>,
    node_changes: Vec<(String, String, String)>,
}

async fn journal_rows(state: &Path) -> JournalRows {
    let mut connection = connect(&state.join("state.db")).await;
    let mut tx = connection.begin().await.unwrap();
    let change_sets = sqlx::query_as(
        "SELECT seq,operation_id,committed_at FROM change_sets ORDER BY length(seq),seq",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let node_changes = sqlx::query_as(
        "SELECT seq,node_id,post_state FROM node_changes ORDER BY length(seq),seq,node_id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    connection.close().await.unwrap();
    JournalRows {
        change_sets,
        node_changes,
    }
}

#[tokio::test]
async fn journal_retention_unexpired_large_budget_preserves_every_journal_row() {
    let mut fixture = Fixture::new().await;
    fixture.make_recent_changes().await;
    let before = journal_rows(&fixture.state).await;
    assert_eq!(before.change_sets.len(), 4);
    assert!(before.change_sets.iter().all(|row| row.2 == Some(NOW)));
    assert_eq!(
        before
            .node_changes
            .iter()
            .filter(|row| row.0 == "1")
            .count(),
        2
    );
    let protected_before = protected(&fixture.state, &fixture.notes).await;
    assert_eq!(protected_before.receipts.len(), 2);
    assert_eq!(protected_before.note.as_deref(), Some(SAVED_BYTES));
    let epoch = fixture.workspace.epoch();

    retain_journal(&mut fixture.workspace, LARGE_BUDGET)
        .await
        .unwrap();
    assert_eq!(journal_rows(&fixture.state).await, before);
    assert_eq!(
        protected(&fixture.state, &fixture.notes).await,
        protected_before
    );
    let info = fixture.workspace.server_info().await.unwrap();
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(info.head_seq, ChangeSeq::new(4));
    assert_eq!(info.workspace_epoch, epoch);

    fixture.workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert_eq!(journal_rows(&fixture.state).await, before);
    assert_eq!(
        protected(&fixture.state, &fixture.notes).await,
        protected_before
    );
    let info = reopened.server_info().await.unwrap();
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(info.head_seq, ChangeSeq::new(4));
    assert_eq!(info.workspace_epoch, epoch);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn journal_retention_unknown_time_observation_never_changes_journal_payloads() {
    let mut fixture = Fixture::new().await;
    fixture.make_recent_changes().await;
    let mut connection = connect(&fixture.state.join("state.db")).await;
    let updated = sqlx::query("UPDATE change_sets SET committed_at=NULL WHERE seq IN ('1','2')")
        .execute(&mut connection)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(updated, 2);
    connection.close().await.unwrap();
    let before = journal_rows(&fixture.state).await;
    assert_eq!(before.change_sets.len(), 4);
    assert_eq!(before.change_sets[0].2, None);
    assert_eq!(before.change_sets[1].2, None);
    let protected_before = protected(&fixture.state, &fixture.notes).await;
    let epoch = fixture.workspace.epoch();

    retain_journal(&mut fixture.workspace, LARGE_BUDGET)
        .await
        .unwrap();
    let observed = journal_rows(&fixture.state).await;
    assert_eq!(observed.node_changes, before.node_changes);
    assert_eq!(observed.change_sets.len(), before.change_sets.len());
    for (old, current) in before.change_sets.iter().zip(&observed.change_sets) {
        assert_eq!((&old.0, &old.1), (&current.0, &current.1));
        match old.2 {
            Some(time) => assert_eq!(current.2, Some(time)),
            None => assert!(current.2.is_none_or(|time| time >= NOW)),
        }
    }
    assert_eq!(
        protected(&fixture.state, &fixture.notes).await,
        protected_before
    );
    let info = fixture.workspace.server_info().await.unwrap();
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(info.head_seq, ChangeSeq::new(4));
    assert_eq!(info.workspace_epoch, epoch);

    retain_journal(&mut fixture.workspace, LARGE_BUDGET)
        .await
        .unwrap();
    assert_eq!(journal_rows(&fixture.state).await, observed);
    fixture.workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert_eq!(journal_rows(&fixture.state).await, observed);
    assert_eq!(
        protected(&fixture.state, &fixture.notes).await,
        protected_before
    );
    let info = reopened.server_info().await.unwrap();
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(info.head_seq, ChangeSeq::new(4));
    assert_eq!(info.workspace_epoch, epoch);
    reopened.close().await.unwrap();
}
