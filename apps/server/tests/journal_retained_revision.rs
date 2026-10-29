use notryx_domain::ChangeSeq;
use notryx_server::{Document, Error, MoveNode, Outcome, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[path = "support/journal_baseline_fixture.rs"]
mod journal_baseline_fixture;

const FIRST: &[u8] = b"synthetic initial\n";
const SECOND: &[u8] = b"synthetic second body\n";
const FINAL: &[u8] = b"synthetic final body after saves\n";

async fn connect(path: &Path) -> SqliteConnection {
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
    sequence: u64,
    document: &Document,
    bytes: &[u8],
) -> Document {
    let result = workspace
        .save(SaveContent {
            epoch: workspace.epoch(),
            stream_id: stream,
            operation_seq: sequence,
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
    let Outcome::Saved(saved) = result.outcome else {
        panic!("synthetic save failed")
    };
    saved
}

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
    document_id: String,
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
        fs::write(notes.join("Before.md"), FIRST).unwrap();
        let document = workspace.adopt_root_document("Before.md").await.unwrap();
        let document = save(&mut workspace, stream, 1, &document, SECOND).await;
        let document = save(&mut workspace, stream, 2, &document, FINAL).await;
        let moved = workspace
            .move_node(MoveNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 3,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_meta_rev: document.meta_rev.clone(),
                expected_parent_id: document.parent_id,
                parent_id: document.parent_id,
                name: "Middle.md".into(),
            })
            .await
            .unwrap();
        let Outcome::Moved(moved) = moved.outcome else {
            panic!("synthetic move failed")
        };
        let moved_again = workspace
            .move_node(MoveNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 4,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_meta_rev: moved.meta_rev,
                expected_parent_id: document.parent_id,
                parent_id: document.parent_id,
                name: "Final.md".into(),
            })
            .await
            .unwrap();
        assert!(matches!(moved_again.outcome, Outcome::Moved(_)));
        assert_eq!(
            workspace.server_info().await.unwrap().head_seq,
            ChangeSeq::new(5)
        );
        workspace.close().await.unwrap();

        let mut db = connect(&state.join("state.db")).await;
        let mut tx = db.begin().await.unwrap();
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
        db.close().await.unwrap();
        let mut valid = Workspace::open(&notes, &state)
            .await
            .expect("valid retained history must reopen");
        assert_eq!(
            valid.server_info().await.unwrap().journal_floor_seq,
            ChangeSeq::new(1)
        );
        valid.close().await.unwrap();
        assert_eq!(fs::read(notes.join("Final.md")).unwrap(), FINAL);
        Self {
            _directory: directory,
            notes,
            state,
            document_id: document.node_id.to_string(),
        }
    }

    async fn corrupt(&self, sql: &'static str) {
        let mut db = connect(&self.state.join("state.db")).await;
        let changed = sqlx::query(sql)
            .bind(&self.document_id)
            .execute(&mut db)
            .await
            .unwrap();
        assert_eq!(changed.rows_affected(), 1);
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&mut db)
                .await
                .unwrap()
                .is_empty()
        );
        db.close().await.unwrap();
    }

    async fn assert_rejected_without_modifying_files(&self) {
        let mut db = connect(&self.state.join("state.db")).await;
        let before: Vec<(String, String, String)> =
            sqlx::query_as("SELECT seq,node_id,post_state FROM node_changes ORDER BY seq,node_id")
                .fetch_all(&mut db)
                .await
                .unwrap();
        let receipts_before: Vec<String> = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
            .fetch_all(&mut db).await.unwrap();
        db.close().await.unwrap();
        assert!(matches!(
            Workspace::open(&self.notes, &self.state).await,
            Err(Error::InvalidFormat)
        ));
        let mut db = connect(&self.state.join("state.db")).await;
        let after: Vec<(String, String, String)> =
            sqlx::query_as("SELECT seq,node_id,post_state FROM node_changes ORDER BY seq,node_id")
                .fetch_all(&mut db)
                .await
                .unwrap();
        let receipts_after: Vec<String> = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
            .fetch_all(&mut db).await.unwrap();
        assert_eq!(after, before);
        assert_eq!(receipts_after, receipts_before);
        db.close().await.unwrap();
        assert_eq!(fs::read(self.notes.join("Final.md")).unwrap(), FINAL);
    }
}

#[tokio::test]
async fn journal_retained_revision_rejects_content_revision_with_changed_bytes() {
    let fixture = Fixture::new().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.content_rev', (SELECT json_extract(post_state,'$.content_rev') FROM journal_baseline WHERE node_id=?1)) WHERE seq='2' AND node_id=?1").await;
    fixture.assert_rejected_without_modifying_files().await;
}

#[tokio::test]
async fn journal_retained_revision_rejects_meta_revision_with_changed_name() {
    let fixture = Fixture::new().await;
    fixture.corrupt("UPDATE node_changes SET post_state=json_set(post_state,'$.meta_rev', (SELECT json_extract(post_state,'$.meta_rev') FROM node_changes WHERE seq='3' AND node_id=?1)) WHERE seq='4' AND node_id=?1").await;
    fixture.assert_rejected_without_modifying_files().await;
}
