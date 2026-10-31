use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_protocol::{CONTRACT, PairRequest};
use notryx_server::{SaveContent, Workspace};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

const V9_SCHEMA: &str = include_str!("fixtures/server_schema_v9.sql");
const V9_TABLES: &[&str] = &[
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

fn cli(args: &[&Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_notryx-server"))
        .args(args)
        .output()
        .expect("run synthetic server binary")
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn files(base: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(base, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(base).unwrap().into(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(base, base, &mut result);
    result
}

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
    config: PathBuf,
    bytes: Vec<u8>,
    token: String,
    now: i64,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&state).unwrap();
        let config = directory.path().join("server.toml");
        fs::write(&config, format!("[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n", directory.path().join("admin.sock").display(), notes.display(), state.display())).unwrap();
        let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let owner = workspace.auth_status().await.unwrap().owner_principal_id;
        let grant = workspace.issue_pairing(owner, now).await.unwrap();
        let device = workspace
            .pair(
                PairRequest {
                    protocol: CONTRACT.into(),
                    workspace_id: grant.workspace_id,
                    pairing_secret: grant.pairing_secret,
                },
                now,
            )
            .await
            .unwrap();
        let stream: Uuid = device.device_stream_id.parse().unwrap();
        fs::write(notes.join("Synthetic.md"), b"first\r\n").unwrap();
        let document = workspace.adopt_root_document("Synthetic.md").await.unwrap();
        let bytes = b"\xef\xbb\xbf# preserved synthetic backup\r\n".to_vec();
        workspace
            .save(SaveContent {
                epoch: workspace.epoch(),
                stream_id: stream,
                operation_seq: 1,
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                node_id: document.node_id,
                expected_content_rev: document.content_rev,
                content_hash: hex(&bytes),
                bytes: bytes.clone(),
            })
            .await
            .unwrap();
        workspace
            .authenticate(&device.device_token, now + 1)
            .await
            .unwrap();
        workspace.close().await.unwrap();
        Self {
            _directory: directory,
            notes,
            state,
            config,
            bytes,
            token: device.device_token,
            now,
        }
    }

    fn backup(&self, target: &Path) {
        let result = cli(&[
            Path::new("backup"),
            Path::new("--config"),
            &self.config,
            target,
        ]);
        assert!(
            result.status.success(),
            "synthetic backup creation failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
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

async fn legacy_v9_backup(backup: &Path) {
    let state_db = backup.join("state/state.db");
    let before_mode = fs::metadata(&state_db).unwrap().permissions().mode() & 0o7777;
    let legacy_path = backup.join("state/legacy-fixture.db");
    let mut legacy = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&legacy_path)
            .create_if_missing(true)
            .foreign_keys(false),
    )
    .await
    .unwrap();
    sqlx::raw_sql(V9_SCHEMA).execute(&mut legacy).await.unwrap();
    sqlx::query("ATTACH DATABASE ? AS newer")
        .bind(state_db.to_str().unwrap())
        .execute(&mut legacy)
        .await
        .unwrap();
    for &table in V9_TABLES {
        let query = format!("PRAGMA main.table_info({table})");
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&mut legacy)
            .await
            .unwrap();
        let names = rows
            .iter()
            .map(|row| {
                let name: String = row.try_get("name").unwrap();
                assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
                format!("\"{name}\"")
            })
            .collect::<Vec<_>>()
            .join(",");
        let query = format!("INSERT INTO main.{table}({names}) SELECT {names} FROM newer.{table}");
        sqlx::query(sqlx::AssertSqlSafe(query))
            .execute(&mut legacy)
            .await
            .unwrap();
    }
    sqlx::query("DETACH DATABASE newer")
        .execute(&mut legacy)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut legacy)
            .await
            .unwrap(),
        9
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA quick_check")
            .fetch_one(&mut legacy)
            .await
            .unwrap(),
        "ok"
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut legacy)
            .await
            .unwrap()
            .is_empty()
    );
    legacy.close().await.unwrap();
    fs::set_permissions(&legacy_path, fs::Permissions::from_mode(before_mode)).unwrap();
    fs::rename(&legacy_path, &state_db).unwrap();
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir(target).unwrap();
    fs::set_permissions(target, fs::metadata(source).unwrap().permissions()).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if [
            "server.lock",
            ".mdw-server.lock",
            "state.db-wal",
            "state.db-shm",
        ]
        .iter()
        .any(|skip| name == *skip)
        {
            continue;
        }
        let destination = target.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            assert!(entry.file_type().unwrap().is_file());
            fs::copy(entry.path(), &destination).unwrap();
            fs::set_permissions(&destination, entry.metadata().unwrap().permissions()).unwrap();
        }
    }
}

fn manifest_entries(base: &Path) -> Vec<Value> {
    fn walk(base: &Path, path: &Path, entries: &mut Vec<Value>) {
        let metadata = fs::metadata(path).unwrap();
        let relative = path.strip_prefix(base).unwrap();
        let encoded = STANDARD.encode(relative.to_str().unwrap());
        if metadata.is_dir() {
            entries.push(serde_json::json!({"path":encoded, "kind":"directory", "mode":metadata.permissions().mode() & 0o7777, "size":0, "sha256":null}));
            for entry in fs::read_dir(path).unwrap() {
                walk(base, &entry.unwrap().path(), entries);
            }
        } else {
            let bytes = fs::read(path).unwrap();
            entries.push(serde_json::json!({"path":encoded, "kind":"file", "mode":metadata.permissions().mode() & 0o7777, "size":bytes.len(), "sha256":hex(&bytes)}));
        }
    }
    let mut entries = Vec::new();
    for name in ["notes", "state", "config"] {
        walk(base, &base.join(name), &mut entries);
    }
    entries.sort_by_key(|entry| entry["path"].as_str().unwrap().to_owned());
    entries
}

async fn create_v9_fixture_backup(fixture: &Fixture, target: &Path) {
    let mut connection = connect(&fixture.state.join("state.db")).await;
    let checkpoint: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(checkpoint.0, 0);
    connection.close().await.unwrap();
    fs::create_dir(target).unwrap();
    copy_tree(&fixture.notes, &target.join("notes"));
    copy_tree(&fixture.state, &target.join("state"));
    fs::create_dir(target.join("config")).unwrap();
    fs::set_permissions(target.join("config"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy(&fixture.config, target.join("config/server.toml")).unwrap();
    fs::set_permissions(
        target.join("config/server.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    legacy_v9_backup(target).await;
    let mut db = connect(&target.join("state/state.db")).await;
    let row =
        sqlx::query("SELECT workspace_id,epoch,root_id,head_seq FROM workspaces WHERE singleton=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
    let workspace_id: String = row.try_get("workspace_id").unwrap();
    let epoch: String = row.try_get("epoch").unwrap();
    let root_id: String = row.try_get("root_id").unwrap();
    let head_seq: String = row.try_get("head_seq").unwrap();
    db.close().await.unwrap();
    let manifest = serde_json::json!({
        "format":1,"workspace_id":workspace_id,"epoch":epoch,"root_id":root_id,"head_seq":head_seq,
        "schema_version":9,"app_version":"synthetic-v9",
        "observation":"external_writer_quiescence_and_final_reconciliation_unverified",
        "deployment":"not_included",
        "filesystem_metadata":"mode_only_uid_gid_acl_xattr_hardlinks_and_timestamps_not_preserved",
        "entries":manifest_entries(target)
    });
    let bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(target.join("manifest.json"), &bytes).unwrap();
    fs::write(target.join("COMPLETE"), hex(&bytes)).unwrap();
}

fn resign_manifest(backup: &Path, schema: i64) {
    let manifest_path = backup.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = schema.into();
    let encoded = STANDARD.encode("state/state.db");
    let db = fs::read(backup.join("state/state.db")).unwrap();
    let entries = manifest["entries"].as_array_mut().unwrap();
    let mut matches = entries.iter_mut().filter(|entry| entry["path"] == encoded);
    let entry = matches.next().unwrap();
    entry["size"] = (db.len() as u64).into();
    entry["sha256"] = hex(&db).into();
    assert!(matches.next().is_none());
    let encoded_manifest = serde_json::to_vec(&manifest).unwrap();
    fs::write(&manifest_path, &encoded_manifest).unwrap();
    fs::write(backup.join("COMPLETE"), hex(&encoded_manifest)).unwrap();
}

async fn metadata(path: &Path) -> (String, String, String, i64) {
    let mut db = connect(path).await;
    let row = sqlx::query("SELECT workspace_id,epoch,head_seq FROM workspaces")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let version = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let result = (
        row.try_get("workspace_id").unwrap(),
        row.try_get("epoch").unwrap(),
        row.try_get("head_seq").unwrap(),
        version,
    );
    db.close().await.unwrap();
    result
}

async fn preserved_rows(path: &Path) -> BTreeMap<String, Vec<String>> {
    let mut db = connect(path).await;
    let mut result = BTreeMap::new();
    for (name, query) in [
        (
            "nodes",
            "SELECT json_array(node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq,deleted) FROM nodes",
        ),
        (
            "change_sets",
            "SELECT json_array(seq,operation_id) FROM change_sets",
        ),
        (
            "node_changes",
            "SELECT json_array(seq,node_id,post_state) FROM node_changes",
        ),
        (
            "operation_streams",
            "SELECT json_array(stream_id,last_seq,retired_through) FROM operation_streams",
        ),
        (
            "operation_receipts",
            "SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending,node_etag) FROM operation_receipts",
        ),
        (
            "write_intents",
            "SELECT json_array(operation_id,intent,observed_hash,recovery_conflict) FROM write_intents",
        ),
        (
            "retired_operations",
            "SELECT json_array(operation_id,stream_id,operation_seq) FROM retired_operations",
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
        let mut entries: Vec<String> = sqlx::query_scalar(query).fetch_all(&mut db).await.unwrap();
        entries.sort();
        result.insert(name.into(), entries);
    }
    db.close().await.unwrap();
    result
}

async fn retention_rows(path: &Path) -> (String, Vec<String>, Vec<String>) {
    let mut db = connect(path).await;
    let floor = sqlx::query_scalar("SELECT journal_floor_seq FROM workspaces")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let mut baseline: Vec<String> =
        sqlx::query_scalar("SELECT json_array(node_id,seq,post_state) FROM journal_baseline")
            .fetch_all(&mut db)
            .await
            .unwrap();
    let mut times: Vec<String> =
        sqlx::query_scalar("SELECT json_array(seq,committed_at) FROM change_sets")
            .fetch_all(&mut db)
            .await
            .unwrap();
    baseline.sort();
    times.sort();
    db.close().await.unwrap();
    (floor, baseline, times)
}

async fn assert_token_revoked(fixture: &Fixture, destination: &Path) {
    let mut workspace = Workspace::open(&destination.join("notes"), &destination.join("state"))
        .await
        .unwrap();
    assert!(
        workspace
            .authenticate(&fixture.token, fixture.now + 1)
            .await
            .is_err()
    );
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn journal_v10_backup_current_schema_verifies_and_restores() {
    let fixture = Fixture::new().await;
    let backup = fixture._directory.path().join("backup-v10");
    fixture.backup(&backup);
    let manifest: Value =
        serde_json::from_slice(&fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 10);
    let before = files(&backup);
    assert!(cli(&[Path::new("backup-verify"), &backup]).status.success());
    let old = metadata(&backup.join("state/state.db")).await;
    let rows_before = preserved_rows(&backup.join("state/state.db")).await;
    let retention_before = retention_rows(&backup.join("state/state.db")).await;
    assert_eq!(retention_before.0, "0");
    assert_eq!(retention_before.1.len(), 1);
    let objects_before = files(&backup.join("state/objects"));
    assert_eq!(old.3, 10);
    let destination = fixture._directory.path().join("restore-v10");
    let result = cli(&[Path::new("restore"), &backup, &destination]);
    assert!(
        result.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let restored = metadata(&destination.join("state/state.db")).await;
    assert_eq!((&restored.0, &restored.2, restored.3), (&old.0, &old.2, 10));
    assert_ne!(restored.1, old.1);
    assert_eq!(
        preserved_rows(&destination.join("state/state.db")).await,
        rows_before
    );
    assert_eq!(files(&destination.join("state/objects")), objects_before);
    assert_eq!(
        retention_rows(&destination.join("state/state.db")).await,
        retention_before
    );
    assert_eq!(
        fs::read(destination.join("notes/Synthetic.md")).unwrap(),
        fixture.bytes
    );
    assert_eq!(
        fs::read(fixture.notes.join("Synthetic.md")).unwrap(),
        fixture.bytes
    );
    assert_eq!(files(&backup), before);
    assert_token_revoked(&fixture, &destination).await;
}

#[tokio::test]
async fn journal_v10_backup_preserves_retained_baseline_and_remaining_journal() {
    let fixture = Fixture::new().await;
    let retention_seconds = 30 * 24 * 60 * 60;
    let budget_bytes = 64 * 1024 * 1024;
    let mut workspace = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    workspace
        .retain_journal(retention_seconds, budget_bytes, fixture.now)
        .await
        .unwrap();
    fs::write(fixture.notes.join("After.md"), b"remaining journal\r\n").unwrap();
    workspace.adopt_root_document("After.md").await.unwrap();
    workspace
        .retain_journal(
            retention_seconds,
            budget_bytes,
            fixture.now + retention_seconds as i64 + 2,
        )
        .await
        .unwrap();
    let info = workspace.server_info().await.unwrap();
    assert_eq!(info.journal_floor_seq.get(), 2);
    assert_eq!(info.head_seq.get(), 3);
    workspace.close().await.unwrap();

    let source_db = fixture.state.join("state.db");
    let old = metadata(&source_db).await;
    let rows_before = preserved_rows(&source_db).await;
    let retention_before = retention_rows(&source_db).await;
    assert_eq!(retention_before.0, "2");
    assert_eq!(retention_before.1.len(), 2);
    assert_eq!(retention_before.2.len(), 1);
    let remaining: Value = serde_json::from_str(&retention_before.2[0]).unwrap();
    assert_eq!(remaining[0], "3");
    let objects_before = files(&fixture.state.join("objects"));
    let notes_before = files(&fixture.notes);
    let state_before = files(&fixture.state);
    let backup = fixture._directory.path().join("backup-retained");
    fixture.backup(&backup);
    let backup_before = files(&backup);
    assert!(cli(&[Path::new("backup-verify"), &backup]).status.success());
    assert_eq!(metadata(&backup.join("state/state.db")).await, old);

    let destination = fixture._directory.path().join("restore-retained");
    let result = cli(&[Path::new("restore"), &backup, &destination]);
    assert!(
        result.status.success(),
        "retained journal restore failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let restored = metadata(&destination.join("state/state.db")).await;
    assert_eq!((&restored.0, &restored.2, restored.3), (&old.0, &old.2, 10));
    assert_ne!(restored.1, old.1);
    for root in [&backup, &destination] {
        assert_eq!(
            preserved_rows(&root.join("state/state.db")).await,
            rows_before
        );
        assert_eq!(
            retention_rows(&root.join("state/state.db")).await,
            retention_before
        );
        assert_eq!(files(&root.join("state/objects")), objects_before);
        assert_eq!(
            fs::read(root.join("notes/Synthetic.md")).unwrap(),
            fixture.bytes
        );
        assert_eq!(
            fs::read(root.join("notes/After.md")).unwrap(),
            b"remaining journal\r\n"
        );
    }
    assert_eq!(files(&fixture.notes), notes_before);
    assert_eq!(files(&fixture.state), state_before);
    assert_eq!(files(&backup), backup_before);
    assert_token_revoked(&fixture, &destination).await;
}

#[tokio::test]
async fn journal_v10_backup_legacy_v9_verifies_restores_and_migrates_only_copy() {
    let fixture = Fixture::new().await;
    let backup = fixture._directory.path().join("backup-v9");
    create_v9_fixture_backup(&fixture, &backup).await;
    let before = files(&backup);
    assert!(cli(&[Path::new("backup-verify"), &backup]).status.success());
    let old = metadata(&backup.join("state/state.db")).await;
    let rows_before = preserved_rows(&backup.join("state/state.db")).await;
    let objects_before = files(&backup.join("state/objects"));
    assert_eq!(old.3, 9);
    let destination = fixture._directory.path().join("restore-v9");
    let result = cli(&[Path::new("restore"), &backup, &destination]);
    assert!(
        result.status.success(),
        "legacy restore failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let restored = metadata(&destination.join("state/state.db")).await;
    assert_eq!((&restored.0, &restored.2, restored.3), (&old.0, &old.2, 10));
    assert_ne!(restored.1, old.1);
    assert_eq!(
        preserved_rows(&destination.join("state/state.db")).await,
        rows_before
    );
    assert_eq!(files(&destination.join("state/objects")), objects_before);
    let retained = retention_rows(&destination.join("state/state.db")).await;
    assert_eq!(retained.0, "0");
    assert_eq!(retained.1.len(), 1);
    let baseline: Value = serde_json::from_str(&retained.1[0]).unwrap();
    let mut db = connect(&destination.join("state/state.db")).await;
    let root_id: String = sqlx::query_scalar("SELECT root_id FROM workspaces")
        .fetch_one(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let root: Value = serde_json::from_str(baseline[2].as_str().unwrap()).unwrap();
    assert_eq!(baseline[0], root_id);
    assert_eq!(baseline[1], "0");
    assert_eq!(root["node_id"], root_id);
    assert_eq!(root["last_change_seq"], "0");
    assert_eq!(root["kind"], "folder");
    assert_eq!(root["parent_id"], Value::Null);
    assert_ne!(root["deleted"], true);
    for entry in &retained.2 {
        let item: Value = serde_json::from_str(entry).unwrap();
        assert!(item[1].is_null());
    }
    assert_eq!(
        fs::read(destination.join("notes/Synthetic.md")).unwrap(),
        fixture.bytes
    );
    assert_eq!(
        fs::read(fixture.notes.join("Synthetic.md")).unwrap(),
        fixture.bytes
    );
    assert_eq!(files(&backup), before);
    assert_token_revoked(&fixture, &destination).await;
}

#[tokio::test]
async fn journal_v10_backup_rejects_inconsistent_legacy_state_without_publishing_destination() {
    let fixture = Fixture::new().await;
    let backup = fixture._directory.path().join("damaged-v9");
    create_v9_fixture_backup(&fixture, &backup).await;
    let mut db = connect(&backup.join("state/state.db")).await;
    sqlx::query("UPDATE nodes SET content_hash=? WHERE name='Synthetic.md'")
        .bind("0".repeat(64))
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    resign_manifest(&backup, 9);
    let before = files(&backup);
    assert!(cli(&[Path::new("backup-verify"), &backup]).status.success());
    let destination = fixture._directory.path().join("restore-damaged");
    assert!(
        !cli(&[Path::new("restore"), &backup, &destination])
            .status
            .success()
    );
    assert!(!destination.exists());
    assert_eq!(files(&backup), before);
    assert_eq!(
        fs::read(fixture.notes.join("Synthetic.md")).unwrap(),
        fixture.bytes
    );
}
