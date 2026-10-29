use notryx_domain::ChangeSeq;
use notryx_server::{DeleteNode, Error, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const V9_SCHEMA: &str = include_str!("fixtures/server_schema_v9.sql");
const TABLES: &[&str] = &[
    "workspaces",
    "nodes",
    "change_sets",
    "node_changes",
    "operation_streams",
    "operation_receipts",
    "write_intents",
    "principals",
    "devices",
    "pairing_secrets",
    "retired_operations",
    "recovery_cases",
    "recovery_objects",
    "uploads",
];

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
    original_bytes: Vec<u8>,
}

async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

async fn columns(connection: &mut SqliteConnection, schema: &str, table: &str) -> Vec<String> {
    assert!(TABLES.contains(&table));
    assert!(matches!(schema, "main" | "legacy"));
    let query = format!("PRAGMA {schema}.table_info({table})");
    sqlx::query(sqlx::AssertSqlSafe(query))
        .fetch_all(connection)
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get::<String, _>("name").unwrap())
        .collect()
}

async fn copy_fixed_v9(source_path: &Path, legacy_path: &Path) {
    let mut legacy_db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(legacy_path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(V9_SCHEMA)
        .execute(&mut legacy_db)
        .await
        .unwrap();
    legacy_db.close().await.unwrap();
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(source_path)
            .foreign_keys(false),
    )
    .await
    .unwrap();
    sqlx::query("ATTACH DATABASE ? AS legacy")
        .bind(legacy_path.to_str().unwrap())
        .execute(&mut connection)
        .await
        .unwrap();
    for &table in TABLES {
        let list = columns(&mut connection, "legacy", table)
            .await
            .into_iter()
            .map(|name| {
                assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
                format!("\"{name}\"")
            })
            .collect::<Vec<_>>()
            .join(",");
        let query = format!("INSERT INTO legacy.{table} ({list}) SELECT {list} FROM main.{table}");
        sqlx::query(sqlx::AssertSqlSafe(query))
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::query("DETACH DATABASE legacy")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let mut legacy_db = db(legacy_path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA application_id")
            .fetch_one(&mut legacy_db)
            .await
            .unwrap(),
        1314148947
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut legacy_db)
            .await
            .unwrap(),
        9
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA quick_check")
            .fetch_one(&mut legacy_db)
            .await
            .unwrap(),
        "ok"
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut legacy_db)
            .await
            .unwrap()
            .is_empty()
    );
    legacy_db.close().await.unwrap();
}

async fn rows(connection: &mut SqliteConnection) -> BTreeMap<String, Vec<String>> {
    let mut result = BTreeMap::new();
    for &table in TABLES {
        // Fixed V9 columns make a V10 value unable to hide a changed legacy row.
        let fixed = v9_columns(table);
        let args = fixed.iter().map(|name| format!("CASE WHEN typeof(\"{name}\")='blob' THEN 'blob:' || hex(\"{name}\") ELSE \"{name}\" END")).collect::<Vec<_>>().join(",");
        let query = format!("SELECT json_array({args}) FROM {table}");
        let mut entries: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .fetch_all(&mut *connection)
            .await
            .unwrap();
        entries.sort();
        result.insert(table.to_owned(), entries);
    }
    result
}

fn v9_columns(table: &str) -> &'static [&'static str] {
    match table {
        "workspaces" => &["singleton", "workspace_id", "epoch", "root_id", "head_seq"],
        "nodes" => &[
            "node_id",
            "parent_id",
            "name",
            "normalized_name_key",
            "kind",
            "content_rev",
            "meta_rev",
            "content_hash",
            "size",
            "children_generation",
            "last_change_seq",
            "deleted",
        ],
        "change_sets" => &["seq", "operation_id"],
        "node_changes" => &["seq", "node_id", "post_state"],
        "operation_streams" => &["stream_id", "last_seq", "retired_through"],
        "operation_receipts" => &[
            "operation_id",
            "stream_id",
            "operation_seq",
            "fingerprint",
            "outcome",
            "pending",
            "node_etag",
        ],
        "write_intents" => &[
            "operation_id",
            "intent",
            "observed_hash",
            "recovery_conflict",
        ],
        "principals" => &["principal_id", "workspace_id", "workspace_role"],
        "devices" => &[
            "device_id",
            "principal_id",
            "stream_id",
            "token_id",
            "token_hash",
            "issued_at",
            "idle_expires_at",
            "absolute_expires_at",
            "revoked_at",
        ],
        "pairing_secrets" => &[
            "pairing_id",
            "principal_id",
            "workspace_id",
            "issued_role",
            "secret_hash",
            "issued_at",
            "expires_at",
            "consumed_at",
        ],
        "retired_operations" => &["operation_id", "stream_id", "operation_seq"],
        "recovery_cases" => &[
            "operation_id",
            "intent",
            "paths",
            "resolved",
            "resolved_head",
        ],
        "recovery_objects" => &["operation_id", "hash"],
        "uploads" => &[
            "upload_id",
            "device_id",
            "status",
            "expected_hash",
            "declared_size",
            "reserved_bytes",
            "started_at",
            "expires_at",
            "content_hash",
            "size",
            "pending_operation_id",
        ],
        _ => panic!("unknown V9 fixture table"),
    }
}

fn object_files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(base: &Path, path: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(base, &path, output);
            } else {
                output.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    if path.exists() {
        visit(path, path, &mut output);
    }
    output
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
        fs::write(notes.join("Keep.md"), b"original\r\n").unwrap();
        let document = workspace.adopt_root_document("Keep.md").await.unwrap();
        let original_bytes = b"\xef\xbb\xbf# preserved synthetic\r\n".to_vec();
        workspace
            .save(SaveContent {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: Sha256::digest(&original_bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                bytes: original_bytes.clone(),
            })
            .await
            .unwrap();
        fs::write(notes.join("Deleted.md"), b"deleted evidence\n").unwrap();
        let deleted = workspace.adopt_root_document("Deleted.md").await.unwrap();
        let etag = workspace
            .node_state(deleted.node_id)
            .await
            .unwrap()
            .node_etag;
        workspace
            .delete_node(DeleteNode {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 2,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: deleted.node_id,
                expected_node_etag: etag,
            })
            .await
            .unwrap();
        workspace.close().await.unwrap();
        let source = state.join("state.db");
        let mut checkpoint = db(&source).await;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut checkpoint)
            .await
            .unwrap();
        checkpoint.close().await.unwrap();
        let legacy = directory.path().join("v9.db");
        copy_fixed_v9(&source, &legacy).await;
        fs::rename(&source, directory.path().join("generated.db")).unwrap();
        fs::rename(&legacy, &source).unwrap();
        Self {
            _directory: directory,
            notes,
            state,
            original_bytes,
        }
    }
}

#[tokio::test]
async fn journal_v10_migration_preserves_v9_history_receipts_objects_and_unknown_dates() {
    let fixture = Fixture::new().await;
    let mut connection = db(&fixture.state.join("state.db")).await;
    let before = rows(&mut connection).await;
    let workspace_id: String = sqlx::query_scalar("SELECT workspace_id FROM workspaces")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let objects_before = object_files(&fixture.state.join("objects"));
    assert!(!objects_before.is_empty());
    connection.close().await.unwrap();
    assert!(matches!(
        Workspace::open(&fixture.notes, &fixture.state).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let mut connection = db(&fixture.state.join("state.db")).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        10
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT journal_floor_seq FROM workspaces")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        "0"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM change_sets WHERE committed_at IS NOT NULL"
        )
        .fetch_one(&mut connection)
        .await
        .unwrap(),
        0
    );
    let root: String = sqlx::query_scalar("SELECT root_id FROM workspaces")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let baseline: Vec<(String, String, String)> =
        sqlx::query_as("SELECT node_id,seq,post_state FROM journal_baseline")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(baseline.len(), 1);
    assert_eq!((&baseline[0].0, baseline[0].1.as_str()), (&root, "0"));
    let root_state: serde_json::Value = serde_json::from_str(&baseline[0].2).unwrap();
    assert_eq!(root_state["node_id"], root);
    assert_eq!(root_state["last_change_seq"], "0");
    assert_eq!(root_state["children_generation"], "0");
    assert_eq!(rows(&mut connection).await, before);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .is_empty()
    );
    connection.close().await.unwrap();
    assert_eq!(
        fs::read(fixture.notes.join("Keep.md")).unwrap(),
        fixture.original_bytes
    );
    assert!(!fixture.notes.join("Deleted.md").exists());
    assert_eq!(object_files(&fixture.state.join("objects")), objects_before);
    let mut workspace = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert_eq!(
        workspace.server_info().await.unwrap().journal_floor_seq,
        ChangeSeq::new(0)
    );
    assert!(
        !workspace
            .changes(ChangeSeq::new(0), None)
            .await
            .unwrap()
            .nodes
            .is_empty()
    );
    workspace.close().await.unwrap();
    let snapshot = fixture._directory.path().join("backup-state.db");
    let mut checkpoint = db(&fixture.state.join("state.db")).await;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut checkpoint)
        .await
        .unwrap();
    checkpoint.close().await.unwrap();
    fs::copy(fixture.state.join("state.db"), &snapshot).unwrap();
    notryx_server::validate_backup_state_copy(&snapshot, workspace_id.parse().unwrap())
        .await
        .unwrap();
    Workspace::migrate(&fixture.notes, &fixture.state)
        .await
        .unwrap();
}

#[tokio::test]
async fn journal_v10_new_workspace_keeps_commit_time_unknown_until_observed() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("New.md"), b"synthetic\n").unwrap();
    workspace.adopt_root_document("New.md").await.unwrap();
    workspace.close().await.unwrap();
    let mut connection = db(&state.join("state.db")).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        10
    );
    let observed: Option<i64> =
        sqlx::query_scalar("SELECT committed_at FROM change_sets WHERE seq='1'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(observed, None);
    connection.close().await.unwrap();
}

#[tokio::test]
async fn journal_v10_migration_collision_rolls_back_and_preserves_v9_data() {
    let fixture = Fixture::new().await;
    let path = fixture.state.join("state.db");
    let mut connection = db(&path).await;
    sqlx::raw_sql("CREATE TABLE journal_baseline(sentinel TEXT NOT NULL); INSERT INTO journal_baseline VALUES('preserved synthetic obstruction');")
        .execute(&mut connection).await.unwrap();
    let before = rows(&mut connection).await;
    let objects_before = object_files(&fixture.state.join("objects"));
    connection.close().await.unwrap();
    assert!(
        Workspace::migrate(&fixture.notes, &fixture.state)
            .await
            .is_err()
    );
    let mut connection = db(&path).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        9
    );
    assert!(
        !columns(&mut connection, "main", "workspaces")
            .await
            .contains(&"journal_floor_seq".to_string())
    );
    assert!(
        !columns(&mut connection, "main", "change_sets")
            .await
            .contains(&"committed_at".to_string())
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sentinel FROM journal_baseline")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        "preserved synthetic obstruction"
    );
    assert_eq!(rows(&mut connection).await, before);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .is_empty()
    );
    connection.close().await.unwrap();
    assert_eq!(
        fs::read(fixture.notes.join("Keep.md")).unwrap(),
        fixture.original_bytes
    );
    assert_eq!(object_files(&fixture.state.join("objects")), objects_before);
}
