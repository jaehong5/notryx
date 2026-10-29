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

const NOW: i64 = 1_800_000_000;
const RETENTION_SECONDS: u64 = 30 * 24 * 60 * 60;
const LARGE_BUDGET: u64 = 64 * 1024 * 1024;
const SAVED_BYTES: &[u8] = b"\xef\xbb\xbf# budget fixture\r\n";

async fn retain_journal(workspace: &mut Workspace, budget_bytes: u64) -> Result<(), Error> {
    workspace
        .retain_journal(RETENTION_SECONDS, budget_bytes, NOW)
        .await
}

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
    fn collect(root: &Path, current: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !current.exists() {
            return;
        }
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &path, output);
            } else {
                output.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    collect(root, root, &mut output);
    output
}

#[derive(Debug, PartialEq, Eq)]
struct Protected {
    nodes: Vec<String>,
    receipts: Vec<String>,
    streams: Vec<String>,
    note: Option<Vec<u8>>,
    deleted_note_exists: bool,
    objects: BTreeMap<PathBuf, Vec<u8>>,
}

async fn protected(state: &Path, notes: &Path) -> Protected {
    let mut connection = connect(&state.join("state.db")).await;
    let mut tx = connection.begin().await.unwrap();
    let nodes: Vec<String> = sqlx::query_scalar("SELECT json_array(node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,deleted) FROM nodes ORDER BY node_id")
        .fetch_all(&mut *tx).await.unwrap();
    let receipts: Vec<String> = sqlx::query_scalar("SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts ORDER BY operation_id")
        .fetch_all(&mut *tx).await.unwrap();
    let streams: Vec<String> = sqlx::query_scalar("SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams ORDER BY stream_id")
        .fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    connection.close().await.unwrap();
    Protected {
        nodes,
        receipts,
        streams,
        note: match fs::read(notes.join("Keep.md")) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("synthetic note read failed: {error}"),
        },
        deleted_note_exists: notes.join("Deleted.md").exists(),
        objects: files_under(&state.join("objects")),
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
    workspace: Workspace,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&state).unwrap();
        let workspace = Workspace::initialize(&notes, &state).await.unwrap();
        Self {
            _directory: directory,
            notes,
            state,
            workspace,
        }
    }

    async fn make_recent_changes(&mut self) {
        let stream = self.workspace.register_stream().await.unwrap();
        fs::write(self.notes.join("Keep.md"), b"synthetic original\r\n").unwrap();
        let document = self.workspace.adopt_root_document("Keep.md").await.unwrap();
        let receipt = self
            .workspace
            .save(SaveContent {
                epoch: self.workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: Sha256::digest(SAVED_BYTES)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                bytes: SAVED_BYTES.to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(receipt.outcome, Outcome::Saved(_)));
        fs::write(self.notes.join("Deleted.md"), b"synthetic deleted\n").unwrap();
        let deleted = self
            .workspace
            .adopt_root_document("Deleted.md")
            .await
            .unwrap();
        let expected_node_etag = self
            .workspace
            .node_state(deleted.node_id)
            .await
            .unwrap()
            .node_etag;
        let receipt = self
            .workspace
            .delete_node(DeleteNode {
                epoch: self.workspace.epoch(),
                stream_id: stream,
                operation_seq: 2,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: deleted.node_id,
                expected_node_etag,
            })
            .await
            .unwrap();
        assert!(matches!(receipt.outcome, Outcome::Deleted(_)));
        assert_eq!(
            self.workspace.server_info().await.unwrap().head_seq,
            ChangeSeq::new(4)
        );
        let mut connection = connect(&self.state.join("state.db")).await;
        let changed = sqlx::query("UPDATE change_sets SET committed_at=?")
            .bind(NOW)
            .execute(&mut connection)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(changed, 4);
        let first_set_nodes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq='1'")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        assert_eq!(first_set_nodes, 2);
        connection.close().await.unwrap();
    }
}

#[tokio::test]
async fn journal_budget_pressure_prunes_recent_complete_changes_and_preserves_sources() {
    let mut fixture = Fixture::new().await;
    fixture.make_recent_changes().await;
    let epoch = fixture.workspace.epoch();
    let before = protected(&fixture.state, &fixture.notes).await;
    assert_eq!(before.note.as_deref(), Some(SAVED_BYTES));
    assert!(!before.deleted_note_exists);
    assert_eq!(before.receipts.len(), 2);
    assert!(!before.objects.is_empty());

    retain_journal(&mut fixture.workspace, LARGE_BUDGET)
        .await
        .unwrap();
    let before_pressure = fixture.workspace.server_info().await.unwrap();
    assert_eq!(before_pressure.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(before_pressure.head_seq, ChangeSeq::new(4));
    assert_eq!(protected(&fixture.state, &fixture.notes).await, before);

    retain_journal(&mut fixture.workspace, 0).await.unwrap();
    let after_pressure = fixture.workspace.server_info().await.unwrap();
    assert_eq!(after_pressure.journal_floor_seq, ChangeSeq::new(4));
    assert_eq!(after_pressure.head_seq, ChangeSeq::new(4));
    assert_eq!(after_pressure.workspace_epoch, epoch);
    assert_eq!(protected(&fixture.state, &fixture.notes).await, before);
    let mut connection = connect(&fixture.state.join("state.db")).await;
    let change_sets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM change_sets")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let node_changes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM node_changes")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!((change_sets, node_changes), (0, 0));
    let baseline: Vec<String> =
        sqlx::query_scalar("SELECT post_state FROM journal_baseline ORDER BY node_id")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(baseline.len(), 3);
    let posts: Vec<serde_json::Value> = baseline
        .iter()
        .map(|post| serde_json::from_str(post).unwrap())
        .collect();
    assert!(
        posts
            .iter()
            .any(|post| post["kind"] == "folder" && post["parent_id"].is_null())
    );
    assert!(
        posts
            .iter()
            .any(|post| post["name"] == "Keep.md" && post["deleted"] == false)
    );
    assert!(
        posts
            .iter()
            .any(|post| post["name"] == "Deleted.md" && post["deleted"] == true)
    );
    connection.close().await.unwrap();
    fixture.workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let info = reopened.server_info().await.unwrap();
    assert_eq!(info.workspace_epoch, epoch);
    assert_eq!(info.head_seq, ChangeSeq::new(4));
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(4));
    assert_eq!(protected(&fixture.state, &fixture.notes).await, before);
    let page = reopened.changes(ChangeSeq::new(4), None).await.unwrap();
    assert_eq!(page.covered_through, ChangeSeq::new(4));
    assert!(page.nodes.is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn journal_budget_below_baseline_does_not_discard_root_or_repeat_changes() {
    let mut fixture = Fixture::new().await;
    let epoch = fixture.workspace.epoch();
    let before = protected(&fixture.state, &fixture.notes).await;
    let mut connection = connect(&fixture.state.join("state.db")).await;
    let baseline_before: Vec<String> = sqlx::query_scalar(
        "SELECT json_array(node_id,seq,post_state) FROM journal_baseline ORDER BY node_id",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(baseline_before.len(), 1);
    assert!(!baseline_before[0].is_empty());
    connection.close().await.unwrap();

    retain_journal(&mut fixture.workspace, 0).await.unwrap();
    retain_journal(&mut fixture.workspace, 0).await.unwrap();
    let info = fixture.workspace.server_info().await.unwrap();
    assert_eq!(info.workspace_epoch, epoch);
    assert_eq!(info.head_seq, ChangeSeq::new(0));
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(protected(&fixture.state, &fixture.notes).await, before);
    let mut connection = connect(&fixture.state.join("state.db")).await;
    let baseline_after: Vec<String> = sqlx::query_scalar(
        "SELECT json_array(node_id,seq,post_state) FROM journal_baseline ORDER BY node_id",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(baseline_after, baseline_before);
    connection.close().await.unwrap();
    fixture.workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let info = reopened.server_info().await.unwrap();
    assert_eq!(info.workspace_epoch, epoch);
    assert_eq!(info.head_seq, ChangeSeq::new(0));
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
    assert_eq!(protected(&fixture.state, &fixture.notes).await, before);
    reopened.close().await.unwrap();
}
