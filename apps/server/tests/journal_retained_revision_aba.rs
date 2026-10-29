use notryx_domain::ChangeSeq;
use notryx_server::{
    DeleteNode, Document, Error, MoveNode, Outcome, RestoreNode, SaveContent, Workspace,
};
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

const A: &[u8] = b"synthetic original\n";
const B: &[u8] = b"synthetic changed body\n";
const C: &[u8] = b"synthetic third body differs\n";
const LAST: &[u8] = b"synthetic final body remains\n";

async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn save(
    workspace: &mut Workspace,
    stream: Uuid,
    seq: u64,
    document: &Document,
    bytes: &[u8],
) -> Document {
    let receipt = workspace
        .save(SaveContent {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: seq,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_content_rev: document.content_rev.clone(),
            content_hash: Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: bytes.to_vec(),
        })
        .await
        .unwrap();
    let Outcome::Saved(result) = receipt.outcome else {
        panic!("synthetic save failed")
    };
    result
}

async fn move_to(
    workspace: &mut Workspace,
    stream: Uuid,
    seq: u64,
    document: &Document,
    meta_rev: notryx_domain::MetaRevision,
    name: &str,
) -> notryx_domain::MetaRevision {
    let receipt = workspace
        .move_node(MoveNode {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: seq,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_meta_rev: meta_rev,
            expected_parent_id: document.parent_id,
            parent_id: document.parent_id,
            name: name.into(),
        })
        .await
        .unwrap();
    let Outcome::Moved(result) = receipt.outcome else {
        panic!("synthetic move failed")
    };
    result.meta_rev
}

fn object_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    collect(root, root, &mut result);
    result
}

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
    id: String,
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
        fs::write(notes.join("Before.md"), A).unwrap();
        let initial = workspace.adopt_root_document("Before.md").await.unwrap();
        let second = save(&mut workspace, stream, 1, &initial, B).await;
        let reverted = save(&mut workspace, stream, 2, &second, A).await;
        assert_ne!(initial.content_rev, reverted.content_rev);
        let third = save(&mut workspace, stream, 3, &reverted, C).await;
        let middle = move_to(
            &mut workspace,
            stream,
            4,
            &third,
            third.meta_rev.clone(),
            "Middle.md",
        )
        .await;
        let reverted_name = move_to(&mut workspace, stream, 5, &third, middle, "Before.md").await;
        assert_ne!(initial.meta_rev, reverted_name);
        move_to(&mut workspace, stream, 6, &third, reverted_name, "Final.md").await;
        let (current, _) = workspace.read(third.node_id).await.unwrap();
        save(&mut workspace, stream, 7, &current, LAST).await;
        assert_eq!(
            workspace.server_info().await.unwrap().head_seq,
            ChangeSeq::new(8)
        );
        workspace.close().await.unwrap();

        let mut connection = db(&state.join("state.db")).await;
        let mut tx = connection.begin().await.unwrap();
        sqlx::query("INSERT INTO journal_baseline(node_id,seq,post_state) SELECT node_id,seq,post_state FROM node_changes WHERE seq='1' ON CONFLICT(node_id) DO UPDATE SET seq=excluded.seq,post_state=excluded.post_state").execute(&mut *tx).await.unwrap();
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
        connection.close().await.unwrap();
        let mut valid = Workspace::open(&notes, &state)
            .await
            .expect("valid retained history must reopen");
        assert_eq!(
            valid.server_info().await.unwrap().journal_floor_seq,
            ChangeSeq::new(1)
        );
        valid.close().await.unwrap();
        assert_eq!(fs::read(notes.join("Final.md")).unwrap(), LAST);
        Self {
            _directory: directory,
            notes,
            state,
            id: initial.node_id.to_string(),
        }
    }

    async fn corrupt(&self, sql: &'static str) {
        let mut connection = db(&self.state.join("state.db")).await;
        let changed = sqlx::query(sql)
            .bind(&self.id)
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
    }

    async fn preserved(
        &self,
    ) -> (
        Vec<String>,
        Vec<String>,
        Vec<String>,
        BTreeMap<PathBuf, Vec<u8>>,
    ) {
        let mut connection = db(&self.state.join("state.db")).await;
        let history = sqlx::query_scalar(
            "SELECT json_array(seq,node_id,post_state) FROM node_changes ORDER BY seq,node_id",
        )
        .fetch_all(&mut connection)
        .await
        .unwrap();
        let receipts = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id").fetch_all(&mut connection).await.unwrap();
        let streams = sqlx::query_scalar("SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id").fetch_all(&mut connection).await.unwrap();
        connection.close().await.unwrap();
        (
            history,
            receipts,
            streams,
            object_files(&self.state.join("objects")),
        )
    }

    async fn assert_rejected(&self) {
        let before = self.preserved().await;
        assert!(!before.3.is_empty());
        assert!(matches!(
            Workspace::open(&self.notes, &self.state).await,
            Err(Error::InvalidFormat)
        ));
        assert_eq!(self.preserved().await, before);
        assert_eq!(fs::read(self.notes.join("Final.md")).unwrap(), LAST);
    }
}

#[tokio::test]
async fn journal_retained_revision_aba_rejects_original_content_revision_for_same_returned_bytes() {
    let fixture = Fixture::new().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.content_rev', (SELECT json_extract(post_state,'$.content_rev') FROM journal_baseline WHERE node_id=?1)) WHERE seq='3' AND node_id=?1").await;
    fixture.assert_rejected().await;
}

#[tokio::test]
async fn journal_retained_revision_aba_rejects_original_content_revision_for_other_bytes() {
    let fixture = Fixture::new().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.content_rev', (SELECT json_extract(post_state,'$.content_rev') FROM journal_baseline WHERE node_id=?1)) WHERE seq='4' AND node_id=?1").await;
    fixture.assert_rejected().await;
}

#[tokio::test]
async fn journal_retained_revision_aba_rejects_original_meta_revision_for_same_returned_name() {
    let fixture = Fixture::new().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.meta_rev', (SELECT json_extract(post_state,'$.meta_rev') FROM journal_baseline WHERE node_id=?1)) WHERE seq='6' AND node_id=?1").await;
    fixture.assert_rejected().await;
}

#[tokio::test]
async fn journal_retained_revision_aba_rejects_original_meta_revision_for_other_name() {
    let fixture = Fixture::new().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.meta_rev', (SELECT json_extract(post_state,'$.meta_rev') FROM journal_baseline WHERE node_id=?1)) WHERE seq='7' AND node_id=?1").await;
    fixture.assert_rejected().await;
}

async fn lifecycle_fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let stream = workspace.register_stream().await.unwrap();
    fs::write(notes.join("Before.md"), A).unwrap();
    let initial = workspace.adopt_root_document("Before.md").await.unwrap();
    let expected_node_etag = workspace
        .node_state(initial.node_id)
        .await
        .unwrap()
        .node_etag;
    let deleted = workspace
        .delete_node(DeleteNode {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: 1,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: initial.node_id,
            expected_node_etag,
        })
        .await
        .unwrap();
    let Outcome::Deleted(deleted) = deleted.outcome else {
        panic!("synthetic delete failed")
    };
    assert_ne!(deleted.content_rev.as_ref(), Some(&initial.content_rev));
    let restored = workspace
        .restore_node(RestoreNode {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: 2,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: initial.node_id,
            expected_meta_rev: deleted.meta_rev,
            parent_id: initial.parent_id,
            name: "Final.md".into(),
        })
        .await
        .unwrap();
    let Outcome::Restored(restored) = restored.outcome else {
        panic!("synthetic restore failed")
    };
    assert_ne!(restored.content_rev.as_ref(), Some(&initial.content_rev));
    assert_eq!(fs::read(notes.join("Final.md")).unwrap(), A);
    let (current, _) = workspace.read(initial.node_id).await.unwrap();
    save(&mut workspace, stream, 3, &current, LAST).await;
    assert_eq!(
        workspace.server_info().await.unwrap().head_seq,
        ChangeSeq::new(4)
    );
    workspace.close().await.unwrap();
    let mut connection = db(&state.join("state.db")).await;
    let mut tx = connection.begin().await.unwrap();
    sqlx::query("INSERT INTO journal_baseline(node_id,seq,post_state) SELECT node_id,seq,post_state FROM node_changes WHERE seq='1' ON CONFLICT(node_id) DO UPDATE SET seq=excluded.seq,post_state=excluded.post_state").execute(&mut *tx).await.unwrap();
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
    connection.close().await.unwrap();
    let mut valid = Workspace::open(&notes, &state)
        .await
        .expect("valid deletion and restore must reopen");
    assert_eq!(
        valid.server_info().await.unwrap().journal_floor_seq,
        ChangeSeq::new(1)
    );
    valid.close().await.unwrap();
    Fixture {
        _directory: directory,
        notes,
        state,
        id: initial.node_id.to_string(),
    }
}

#[tokio::test]
async fn journal_retained_revision_aba_rejects_content_revision_retained_across_delete() {
    let fixture = lifecycle_fixture().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.content_rev', (SELECT json_extract(post_state,'$.content_rev') FROM journal_baseline WHERE node_id=?1)) WHERE seq='2' AND node_id=?1").await;
    fixture.assert_rejected().await;
}

#[tokio::test]
async fn journal_retained_revision_aba_rejects_content_revision_retained_across_restore() {
    let fixture = lifecycle_fixture().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.content_rev', (SELECT json_extract(post_state,'$.content_rev') FROM node_changes WHERE seq='2' AND node_id=?1)) WHERE seq='3' AND node_id=?1").await;
    fixture.assert_rejected().await;
}
