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

const NOW: i64 = 1_000_000;
const RETENTION_SECONDS: u64 = 86_400;
const BUDGET_BYTES: u64 = 64 * 1024 * 1024;
const SAVED_BYTES: &[u8] = b"\xef\xbb\xbf# synthetic retained\r\n";

async fn retain_journal(
    workspace: &mut Workspace,
    retention_seconds: u64,
    budget_bytes: u64,
    now_seconds: i64,
) -> Result<(), Error> {
    workspace
        .retain_journal(retention_seconds, budget_bytes, now_seconds)
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
    fn collect(root: &Path, current: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !current.exists() {
            return;
        }
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    tables: BTreeMap<&'static str, Vec<String>>,
    note: Vec<u8>,
    deleted_note_exists: bool,
    objects: BTreeMap<PathBuf, Vec<u8>>,
}

async fn snapshot(state: &Path, notes: &Path) -> Snapshot {
    let mut connection = connect(&state.join("state.db")).await;
    let mut tx = connection.begin().await.unwrap();
    let queries = [
        (
            "workspace",
            "SELECT json_array(singleton,workspace_id,epoch,root_id,head_seq,journal_floor_seq) FROM workspaces",
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
        (
            "retired",
            "SELECT json_array(operation_id,stream_id,operation_seq) FROM retired_operations",
        ),
    ];
    let mut tables = BTreeMap::new();
    for (name, sql) in queries {
        let mut rows: Vec<String> = sqlx::query_scalar(sql).fetch_all(&mut *tx).await.unwrap();
        rows.sort();
        tables.insert(name, rows);
    }
    tx.commit().await.unwrap();
    connection.close().await.unwrap();
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
    workspace: Workspace,
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
        fs::write(notes.join("Keep.md"), b"synthetic original\r\n").unwrap();
        let document = workspace.adopt_root_document("Keep.md").await.unwrap();
        let receipt = workspace
            .save(SaveContent {
                epoch: workspace.epoch(),
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
        fs::write(notes.join("Deleted.md"), b"synthetic deleted\n").unwrap();
        let deleted = workspace.adopt_root_document("Deleted.md").await.unwrap();
        let expected_node_etag = workspace
            .node_state(deleted.node_id)
            .await
            .unwrap()
            .node_etag;
        let receipt = workspace
            .delete_node(DeleteNode {
                epoch: workspace.epoch(),
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
            workspace.server_info().await.unwrap().head_seq,
            ChangeSeq::new(4)
        );
        let mut connection = connect(&state.join("state.db")).await;
        sqlx::query(
            "UPDATE change_sets SET committed_at = CASE WHEN seq IN ('1','2') THEN 1 ELSE ? END",
        )
        .bind(NOW - 1)
        .execute(&mut connection)
        .await
        .unwrap();
        let counts: (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COUNT(DISTINCT seq) FROM node_changes WHERE seq='1'")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        assert_eq!(counts, (2, 1));
        connection.close().await.unwrap();
        Self {
            _directory: directory,
            notes,
            state,
            workspace,
        }
    }
}

#[tokio::test]
async fn journal_retention_rollback_positive_prunes_whole_prefix_and_survives_restart() {
    let mut fixture = Fixture::new().await;
    let before = snapshot(&fixture.state, &fixture.notes).await;
    assert_eq!(before.note, SAVED_BYTES);
    assert!(!before.deleted_note_exists);
    assert!(!before.objects.is_empty());
    let epoch = fixture.workspace.epoch();
    retain_journal(&mut fixture.workspace, RETENTION_SECONDS, BUDGET_BYTES, NOW)
        .await
        .unwrap();
    let info = fixture.workspace.server_info().await.unwrap();
    assert_eq!(info.workspace_epoch, epoch);
    assert_eq!(info.head_seq, ChangeSeq::new(4));
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(2));
    let after = snapshot(&fixture.state, &fixture.notes).await;
    for table in [
        "nodes",
        "receipts",
        "streams",
        "intents",
        "recovery_cases",
        "recovery_objects",
        "retired",
    ] {
        assert_eq!(before.tables[table], after.tables[table], "{table} changed");
    }
    assert_eq!(before.note, after.note);
    assert_eq!(before.deleted_note_exists, after.deleted_note_exists);
    assert_eq!(before.objects, after.objects);
    let mut connection = connect(&fixture.state.join("state.db")).await;
    let surviving: Vec<String> = sqlx::query_scalar("SELECT seq FROM change_sets ORDER BY seq")
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert_eq!(surviving, ["3", "4"]);
    let removed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM node_changes WHERE seq IN ('1','2')")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(removed, 0);
    let baseline: Vec<(String, String)> =
        sqlx::query_as("SELECT seq,post_state FROM journal_baseline ORDER BY seq")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(baseline.len(), 2);
    assert!(
        baseline
            .iter()
            .any(|(seq, post)| seq == "1" && post.contains("\"kind\":\"folder\""))
    );
    assert!(
        baseline
            .iter()
            .any(|(seq, post)| seq == "2" && post.contains("\"name\":\"Keep.md\""))
    );
    connection.close().await.unwrap();
    fixture.workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let reopened_info = reopened.server_info().await.unwrap();
    assert_eq!(reopened_info.workspace_epoch, epoch);
    assert_eq!(reopened_info.head_seq, ChangeSeq::new(4));
    assert_eq!(reopened_info.journal_floor_seq, ChangeSeq::new(2));
    assert_eq!(snapshot(&fixture.state, &fixture.notes).await, after);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn journal_retention_rollback_sqlite_failure_preserves_every_source_and_reopens() {
    for trigger in [
        "CREATE TRIGGER fail_journal_retention BEFORE DELETE ON node_changes BEGIN SELECT RAISE(FAIL,'synthetic retention failure'); END",
        "CREATE TRIGGER fail_journal_retention BEFORE UPDATE OF journal_floor_seq ON workspaces BEGIN SELECT RAISE(FAIL,'synthetic retention failure'); END",
    ] {
        let mut fixture = Fixture::new().await;
        let mut connection = connect(&fixture.state.join("state.db")).await;
        sqlx::raw_sql(trigger)
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let before = snapshot(&fixture.state, &fixture.notes).await;
        let epoch = fixture.workspace.epoch();
        assert_eq!(
            retain_journal(&mut fixture.workspace, RETENTION_SECONDS, BUDGET_BYTES, NOW).await,
            Err(Error::Storage)
        );
        assert_eq!(snapshot(&fixture.state, &fixture.notes).await, before);
        let mut connection = connect(&fixture.state.join("state.db")).await;
        sqlx::raw_sql("DROP TRIGGER fail_journal_retention")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        fixture.workspace.close().await.unwrap();
        let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
            .await
            .unwrap();
        let info = reopened.server_info().await.unwrap();
        assert_eq!(info.workspace_epoch, epoch);
        assert_eq!(info.head_seq, ChangeSeq::new(4));
        assert_eq!(info.journal_floor_seq, ChangeSeq::new(0));
        assert_eq!(snapshot(&fixture.state, &fixture.notes).await, before);
        reopened.close().await.unwrap();
    }
}
