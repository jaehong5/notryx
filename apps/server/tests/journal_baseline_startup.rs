use notryx_domain::ChangeSeq;
use notryx_server::{DeleteNode, Error, Outcome, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[path = "support/journal_baseline_fixture.rs"]
mod journal_baseline_fixture;

const SAVED: &[u8] = b"\xef\xbb\xbf# synthetic\r\nretained\r\n";

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

fn files_under(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn read(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                read(root, &path, result);
            } else {
                result.insert(
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

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    tables: BTreeMap<&'static str, Vec<String>>,
    note: Vec<u8>,
    deleted_note_exists: bool,
    objects: BTreeMap<PathBuf, Vec<u8>>,
}

async fn snapshot(state: &Path, notes: &Path) -> Snapshot {
    let mut db = connect(&state.join("state.db")).await;
    let mut tx = db.begin().await.unwrap();
    let queries = [
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
    ];
    let mut tables = BTreeMap::new();
    for (name, query) in queries {
        let mut rows: Vec<String> = sqlx::query_scalar(query).fetch_all(&mut *tx).await.unwrap();
        rows.sort();
        tables.insert(name, rows);
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    Snapshot {
        tables,
        note: fs::read(notes.join("Keep.md")).unwrap(),
        deleted_note_exists: notes.join("Deleted.md").exists(),
        objects: files_under(&state.join("objects")),
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
    root: String,
    keep: String,
    deleted: String,
    epoch: notryx_domain::WorkspaceEpoch,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
        let stream = workspace.register_stream().await.unwrap();
        fs::write(notes.join("Keep.md"), b"synthetic initial\r\n").unwrap();
        let keep = workspace.adopt_root_document("Keep.md").await.unwrap();
        fs::write(notes.join("Deleted.md"), b"synthetic deleted\n").unwrap();
        let deleted = workspace.adopt_root_document("Deleted.md").await.unwrap();
        let expected_node_etag = workspace
            .node_state(deleted.node_id)
            .await
            .unwrap()
            .node_etag;
        let delete = workspace
            .delete_node(DeleteNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: deleted.node_id,
                expected_node_etag,
            })
            .await
            .unwrap();
        assert!(matches!(delete.outcome, Outcome::Deleted(_)));
        let save = workspace
            .save(SaveContent {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 2,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: keep.node_id,
                expected_content_rev: keep.content_rev.clone(),
                content_hash: Sha256::digest(SAVED)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                bytes: SAVED.to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(save.outcome, Outcome::Saved(_)));
        assert_eq!(
            workspace.server_info().await.unwrap().head_seq,
            ChangeSeq::new(4)
        );
        let root = keep.parent_id.to_string();
        let keep = keep.node_id.to_string();
        let deleted = deleted.node_id.to_string();
        let epoch = workspace.epoch();
        workspace.close().await.unwrap();

        let mut db = connect(&state.join("state.db")).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='1'")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(count, 2);
        let mut tx = db.begin().await.unwrap();
        for seq in ["1", "2", "3"] {
            sqlx::query("INSERT INTO journal_baseline(node_id,seq,post_state) SELECT node_id,seq,post_state FROM node_changes WHERE seq=? ON CONFLICT(node_id) DO UPDATE SET seq=excluded.seq,post_state=excluded.post_state")
                .bind(seq).execute(&mut *tx).await.unwrap();
        }
        sqlx::query("DELETE FROM node_changes WHERE seq IN ('1','2','3')")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM change_sets WHERE seq IN ('1','2','3')")
            .execute(&mut *tx)
            .await
            .unwrap();
        journal_baseline_fixture::record_fixture_baseline(&mut tx, "3").await;
        tx.commit().await.unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT node_id,seq FROM journal_baseline ORDER BY node_id")
                .fetch_all(&mut db)
                .await
                .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.contains(&(root.clone(), "3".into())));
        assert!(rows.contains(&(keep.clone(), "1".into())));
        assert!(rows.contains(&(deleted.clone(), "3".into())));
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&mut db)
                .await
                .unwrap()
                .is_empty()
        );
        db.close().await.unwrap();
        Self {
            _directory: directory,
            notes,
            state,
            root,
            keep,
            deleted,
            epoch,
        }
    }

    async fn assert_valid_and_close(&self) {
        let mut reopened = Workspace::open(&self.notes, &self.state)
            .await
            .expect("valid baseline must reopen");
        let info = reopened.server_info().await.unwrap();
        assert_eq!(info.workspace_epoch, self.epoch);
        assert_eq!(info.head_seq, ChangeSeq::new(4));
        assert_eq!(info.journal_floor_seq, ChangeSeq::new(3));
        reopened.close().await.unwrap();
    }

    async fn assert_rejected_and_unchanged(&self) {
        let before = snapshot(&self.state, &self.notes).await;
        assert_eq!(before.note, SAVED);
        assert!(!before.deleted_note_exists);
        assert!(!before.objects.is_empty());
        assert!(matches!(
            Workspace::open(&self.notes, &self.state).await,
            Err(Error::InvalidFormat)
        ));
        assert_eq!(snapshot(&self.state, &self.notes).await, before);
    }
}

#[tokio::test]
async fn journal_baseline_startup_reopens_valid_floor_with_tombstone_and_retained_history() {
    let fixture = Fixture::new().await;
    let before = snapshot(&fixture.state, &fixture.notes).await;
    fixture.assert_valid_and_close().await;
    assert_eq!(snapshot(&fixture.state, &fixture.notes).await, before);
}

#[tokio::test]
async fn journal_baseline_startup_rejects_missing_root_or_tombstone_without_modifying_sources() {
    for remove_root in [true, false] {
        let fixture = Fixture::new().await;
        fixture.assert_valid_and_close().await;
        let mut db = connect(&fixture.state.join("state.db")).await;
        sqlx::query("DELETE FROM journal_baseline WHERE node_id=?")
            .bind(if remove_root {
                &fixture.root
            } else {
                &fixture.deleted
            })
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        fixture.assert_rejected_and_unchanged().await;
    }
}

#[tokio::test]
async fn journal_baseline_startup_rejects_corrupt_baseline_and_parent_graph_without_modifying_sources()
 {
    for corrupt_root in [true, false] {
        let fixture = Fixture::new().await;
        fixture.assert_valid_and_close().await;
        let mut db = connect(&fixture.state.join("state.db")).await;
        let mut tx = db.begin().await.unwrap();
        if corrupt_root {
            sqlx::query("UPDATE journal_baseline SET post_state=json_set(post_state,'$.name','invalid root') WHERE node_id=?")
                .bind(&fixture.root).execute(&mut *tx).await.unwrap();
        } else {
            sqlx::query("UPDATE journal_baseline SET post_state=json_set(post_state,'$.parent_id',?) WHERE node_id=?")
                .bind(&fixture.keep).bind(&fixture.keep).execute(&mut *tx).await.unwrap();
        }
        journal_baseline_fixture::record_fixture_baseline(&mut tx, "3").await;
        tx.commit().await.unwrap();
        db.close().await.unwrap();
        fixture.assert_rejected_and_unchanged().await;
    }
}

#[tokio::test]
async fn journal_baseline_startup_rejects_retained_sequence_gap_without_modifying_sources() {
    let fixture = Fixture::new().await;
    fixture.assert_valid_and_close().await;
    let mut db = connect(&fixture.state.join("state.db")).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::query("DELETE FROM node_changes WHERE seq='4'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM change_sets WHERE seq='4'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db.close().await.unwrap();
    fixture.assert_rejected_and_unchanged().await;
}
