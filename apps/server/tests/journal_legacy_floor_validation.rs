use notryx_domain::ChangeSeq;
use notryx_server::{Error, MoveNode, Outcome, Workspace, validate_backup_state_copy};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{fs, path::Path};
use uuid::Uuid;

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn drop_column_if_present(db: &mut SqliteConnection, column: &str) {
    match column {
        "journal_baseline_hash" => {
            let present: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('workspaces') WHERE name='journal_baseline_hash'")
                .fetch_one(&mut *db).await.unwrap();
            if present != 0 {
                sqlx::query("ALTER TABLE workspaces DROP COLUMN journal_baseline_hash")
                    .execute(&mut *db)
                    .await
                    .unwrap();
            }
        }
        "journal_baseline_count" => {
            let present: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('workspaces') WHERE name='journal_baseline_count'")
                .fetch_one(&mut *db).await.unwrap();
            if present != 0 {
                sqlx::query("ALTER TABLE workspaces DROP COLUMN journal_baseline_count")
                    .execute(&mut *db)
                    .await
                    .unwrap();
            }
        }
        _ => panic!("unexpected synthetic column"),
    }
}

#[tokio::test]
async fn journal_legacy_floor_validation_rejects_missing_history_hidden_by_extra_column() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let workspace_id = workspace.workspace_id();
    let stream_id = workspace.register_stream().await.unwrap();
    let original = b"synthetic original\r\n";
    fs::write(notes.join("Before.md"), original).unwrap();
    let document = workspace.adopt_root_document("Before.md").await.unwrap();
    let receipt = workspace
        .move_node(MoveNode {
            epoch: workspace.epoch(),
            stream_id,
            operation_seq: 1,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_meta_rev: document.meta_rev,
            expected_parent_id: document.parent_id,
            parent_id: document.parent_id,
            name: "After.md".into(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, Outcome::Moved(_)));
    assert_eq!(
        workspace.server_info().await.unwrap().head_seq,
        ChangeSeq::new(2)
    );
    workspace.close().await.unwrap();

    let path = state.join("state.db");
    let mut db = connect(&path).await;
    sqlx::query("DROP TABLE journal_baseline")
        .execute(&mut db)
        .await
        .unwrap();
    drop_column_if_present(&mut db, "journal_baseline_hash").await;
    drop_column_if_present(&mut db, "journal_baseline_count").await;
    sqlx::query("ALTER TABLE workspaces DROP COLUMN journal_floor_seq")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE change_sets DROP COLUMN committed_at")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=9")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    validate_backup_state_copy(&path, workspace_id)
        .await
        .expect("valid synthetic V9 state");

    let mut db = connect(&path).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::query("ALTER TABLE workspaces ADD COLUMN journal_floor_seq TEXT NOT NULL DEFAULT '1'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM node_changes WHERE seq='1'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM change_sets WHERE seq='1'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET children_generation='1' WHERE node_id=?")
        .bind(document.parent_id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE node_changes SET post_state=json_set(post_state,'$.children_generation','1') WHERE seq='2' AND node_id=?")
        .bind(document.parent_id.to_string()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let seqs: Vec<String> = sqlx::query_scalar("SELECT seq FROM change_sets")
        .fetch_all(&mut db)
        .await
        .unwrap();
    assert_eq!(seqs, ["2"]);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut db)
            .await
            .unwrap()
            .is_empty()
    );
    db.close().await.unwrap();

    assert!(matches!(
        validate_backup_state_copy(&path, workspace_id).await,
        Err(Error::InvalidFormat)
    ));
    assert_eq!(fs::read(notes.join("After.md")).unwrap(), original);
    assert!(!notes.join("Before.md").exists());
    let mut db = connect(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        9
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM change_sets WHERE seq='2'")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT journal_floor_seq FROM workspaces")
            .fetch_one(&mut db)
            .await
            .unwrap(),
        "1"
    );
    db.close().await.unwrap();
}
