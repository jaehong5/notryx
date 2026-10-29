use notryx_domain::ChangeSeq;
use notryx_server::{Error, MoveNode, Outcome, Workspace};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[path = "support/journal_baseline_fixture.rs"]
mod journal_baseline_fixture;

async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

fn objects(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn read(root: &Path, path: &Path, rows: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                read(root, &path, rows);
            } else {
                rows.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    read(root, root, &mut result);
    result
}

async fn snapshot(state: &Path) -> BTreeMap<&'static str, Vec<String>> {
    let mut connection = db(&state.join("state.db")).await;
    let mut tx = connection.begin().await.unwrap();
    let mut result = BTreeMap::new();
    for (name, query) in [
        (
            "workspace",
            "SELECT json_array(singleton,workspace_id,epoch,root_id,head_seq,journal_floor_seq,journal_baseline_count,journal_baseline_hash) FROM workspaces",
        ),
        (
            "baseline",
            "SELECT json_array(node_id,seq,post_state) FROM journal_baseline",
        ),
        (
            "change_sets",
            "SELECT json_array(seq,operation_id,committed_at) FROM change_sets",
        ),
        (
            "node_changes",
            "SELECT json_array(seq,node_id,post_state) FROM node_changes",
        ),
        (
            "nodes",
            "SELECT json_array(node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,deleted) FROM nodes",
        ),
        (
            "receipts",
            "SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts",
        ),
        (
            "streams",
            "SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams",
        ),
        (
            "intents",
            "SELECT json_array(operation_id,intent,observed_hash,recovery_conflict) FROM write_intents",
        ),
        (
            "recovery_cases",
            "SELECT json_array(operation_id,intent,paths,resolved,resolved_head) FROM recovery_cases",
        ),
        (
            "recovery_objects",
            "SELECT json_array(operation_id,hash) FROM recovery_objects",
        ),
    ] {
        let mut rows: Vec<String> = sqlx::query_scalar(query).fetch_all(&mut *tx).await.unwrap();
        rows.sort();
        result.insert(name, rows);
    }
    tx.commit().await.unwrap();
    connection.close().await.unwrap();
    result
}

#[tokio::test]
async fn journal_baseline_missing_rename_rejects_node_that_looks_like_new_creation() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let stream = workspace.register_stream().await.unwrap();
    let content = b"synthetic rename, preserved\r\n";
    fs::write(notes.join("Before.md"), content).unwrap();
    let document = workspace.adopt_root_document("Before.md").await.unwrap();
    let epoch = workspace.epoch();
    let moved = workspace
        .move_node(MoveNode {
            epoch,
            stream_id: stream,
            operation_seq: 1,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_meta_rev: document.meta_rev.clone(),
            expected_parent_id: document.parent_id,
            parent_id: document.parent_id,
            name: "Renamed.md".into(),
        })
        .await
        .unwrap();
    assert!(matches!(moved.outcome, Outcome::Moved(_)));
    assert_eq!(
        workspace.server_info().await.unwrap().head_seq,
        ChangeSeq::new(2)
    );
    workspace.close().await.unwrap();

    let mut connection = db(&state.join("state.db")).await;
    let seq1: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='1'")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let seq2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='2'")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(seq1, 2);
    assert_eq!(seq2, 2);
    let mut tx = connection.begin().await.unwrap();
    sqlx::query("INSERT INTO journal_baseline(node_id,seq,post_state) SELECT node_id,seq,post_state FROM node_changes WHERE seq='1' ON CONFLICT(node_id) DO UPDATE SET seq=excluded.seq,post_state=excluded.post_state")
        .execute(&mut *tx).await.unwrap();
    sqlx::query("DELETE FROM node_changes WHERE seq='1'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM change_sets WHERE seq='1'")
        .execute(&mut *tx)
        .await
        .unwrap();
    journal_baseline_fixture::record_fixture_baseline(&mut tx, "1").await;
    tx.commit().await.unwrap();
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT node_id,seq FROM journal_baseline ORDER BY node_id")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&(document.parent_id.to_string(), "1".into())));
    assert!(rows.contains(&(document.node_id.to_string(), "1".into())));
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .is_empty()
    );
    connection.close().await.unwrap();

    let mut valid = Workspace::open(&notes, &state)
        .await
        .expect("complete floor baseline must reopen");
    assert_eq!(valid.server_info().await.unwrap().workspace_epoch, epoch);
    valid.close().await.unwrap();

    let mut connection = db(&state.join("state.db")).await;
    let changed = sqlx::query("DELETE FROM journal_baseline WHERE node_id=?")
        .bind(document.node_id.to_string())
        .execute(&mut connection)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .is_empty()
    );
    connection.close().await.unwrap();
    let before = snapshot(&state).await;
    let object_before = objects(&state.join("objects"));
    assert!(!object_before.is_empty());
    assert_eq!(fs::read(notes.join("Renamed.md")).unwrap(), content);
    assert!(!notes.join("Before.md").exists());
    assert!(matches!(
        Workspace::open(&notes, &state).await,
        Err(Error::InvalidFormat)
    ));
    assert_eq!(snapshot(&state).await, before);
    assert_eq!(objects(&state.join("objects")), object_before);
    assert_eq!(fs::read(notes.join("Renamed.md")).unwrap(), content);
    assert!(!notes.join("Before.md").exists());
}
