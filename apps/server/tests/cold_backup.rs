use notryx_server::Workspace;
use sqlx::{
    Connection, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;

fn cli(arguments: &[&Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_notryx-server"))
        .args(arguments)
        .output()
        .expect("run synthetic backup command")
}

fn private_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

fn fixture_config(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let notes = root.join("notes");
    let state = root.join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let config = root.join("server.toml");
    fs::write(
        &config,
        format!(
            "[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n",
            root.join("admin.sock").display(), notes.display(), state.display()
        ),
    )
    .unwrap();
    (notes, state, config)
}

#[tokio::test]
async fn cold_backup_preserves_excluded_directories() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    workspace.close().await.unwrap();
    let excluded = notes.join(".obsidian");
    fs::create_dir(&excluded).unwrap();
    for index in 0..4 {
        fs::create_dir(excluded.join(format!("directory-{index:03}"))).unwrap();
    }
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        cli(&[Path::new("backup-verify"), &destination])
            .status
            .success()
    );
    for index in 0..4 {
        assert!(
            destination
                .join("notes/.obsidian")
                .join(format!("directory-{index:03}"))
                .is_dir()
        );
    }
}

#[tokio::test]
async fn cold_backup_preserves_source_and_verifies_complete_manifest() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let original = b"\xef\xbb\xbf# synthetic\r\n\r\nbody\r\n";
    fs::write(notes.join("Document.md"), original).unwrap();
    workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    fs::create_dir(notes.join(".obsidian")).unwrap();
    fs::write(notes.join(".obsidian").join("workspace.json"), b"{}\n").unwrap();
    let outside = root.path().join("outside.txt");
    fs::write(&outside, b"outside must stay unchanged\n").unwrap();
    symlink(&outside, notes.join(".obsidian").join("external-link")).unwrap();
    let original_db = fs::read(state.join("state.db")).unwrap();
    let original_config = fs::read(&config).unwrap();
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(destination.join("notes/Document.md")).unwrap(),
        original
    );
    assert_eq!(
        fs::read(destination.join("notes/.obsidian/workspace.json")).unwrap(),
        b"{}\n"
    );
    assert_eq!(
        fs::read_link(destination.join("notes/.obsidian/external-link")).unwrap(),
        outside
    );
    assert_eq!(
        fs::read(&outside).unwrap(),
        b"outside must stay unchanged\n"
    );
    assert_eq!(
        fs::read(destination.join("config/server.toml")).unwrap(),
        original_config
    );
    assert_eq!(fs::read(state.join("state.db")).unwrap(), original_db);
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), original);
    assert!(destination.join("manifest.json").is_file());
    assert!(destination.join("COMPLETE").is_file());
    let verified = cli(&[Path::new("backup-verify"), &destination]);
    assert!(
        verified.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    fs::write(
        destination.join("notes/Document.md"),
        b"synthetic tampering\n",
    )
    .unwrap();
    let damaged = cli(&[Path::new("backup-verify"), &destination]);
    assert!(!damaged.status.success());
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), original);
    assert_eq!(fs::read(state.join("state.db")).unwrap(), original_db);
}

#[tokio::test]
async fn cold_backup_rejects_unobserved_changes_without_touching_source() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("Document.md"), b"observed\n").unwrap();
    workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    fs::write(notes.join("Document.md"), b"unobserved\n").unwrap();
    let before = fs::read(state.join("state.db")).unwrap();
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(
        !output.status.success(),
        "an unobserved change cannot be a completed backup"
    );
    assert!(!destination.exists());
    assert_eq!(
        fs::read(notes.join("Document.md")).unwrap(),
        b"unobserved\n"
    );
    assert_eq!(fs::read(state.join("state.db")).unwrap(), before);
}

#[tokio::test]
async fn cold_backup_rejects_missing_referenced_object_without_touching_source() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("Document.md"), b"object\n").unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    fs::remove_file(state.join("objects/sha256").join(document.content_hash)).unwrap();
    let before = fs::read(state.join("state.db")).unwrap();
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(!output.status.success());
    assert!(!destination.exists());
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), b"object\n");
    assert_eq!(fs::read(state.join("state.db")).unwrap(), before);
}

#[tokio::test]
async fn cold_backup_rejects_semantically_corrupt_journal_without_touching_source() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("Document.md"), b"journal\n").unwrap();
    workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(state.join("state.db"))
            .foreign_keys(false),
    )
    .await
    .unwrap();
    sqlx::raw_sql("UPDATE change_sets SET seq='3' WHERE seq='1';
        UPDATE node_changes SET seq='3', post_state=json_set(post_state,'$.last_change_seq','3') WHERE seq='1';
        UPDATE nodes SET last_change_seq='3' WHERE last_change_seq='1';
        UPDATE workspaces SET head_seq='3' WHERE singleton=1;")
        .execute(&mut db).await.unwrap();
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    let foreign_keys = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut db)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    assert!(foreign_keys.is_empty());
    db.close().await.unwrap();
    let before = fs::read(state.join("state.db")).unwrap();
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(
        !output.status.success(),
        "a journal gap cannot be a completed backup"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("InvalidFormat"),
        "expected an invalid state error, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!destination.exists());
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), b"journal\n");
    assert_eq!(fs::read(state.join("state.db")).unwrap(), before);
}

#[tokio::test]
async fn backup_verify_rejects_incomplete_staging_even_if_a_marker_is_present() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("Document.md"), b"synthetic\n").unwrap();
    workspace.adopt_root_document("Document.md").await.unwrap();
    workspace.close().await.unwrap();
    let completed = root.path().join("completed");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &completed,
    ]);
    assert!(output.status.success());
    let incomplete = root.path().join(".notryx-backup-synthetic.incomplete");
    fs::rename(&completed, &incomplete).unwrap();
    assert!(incomplete.join("COMPLETE").is_file());
    let verified = cli(&[Path::new("backup-verify"), &incomplete]);
    assert!(!verified.status.success());
    assert_eq!(fs::read(notes.join("Document.md")).unwrap(), b"synthetic\n");
}

#[tokio::test]
async fn cold_backup_rejects_active_workspace_and_existing_or_nested_destinations() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(!output.status.success());
    assert!(!destination.exists());
    workspace.close().await.unwrap();
    let existing = root.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), b"do not overwrite").unwrap();
    for target in [&existing, &notes.join("nested-backup")] {
        let output = cli(&[Path::new("backup"), Path::new("--config"), &config, target]);
        assert!(!output.status.success());
    }
    assert_eq!(
        fs::read(existing.join("keep")).unwrap(),
        b"do not overwrite"
    );
    assert!(!notes.join("nested-backup").exists());
}

#[tokio::test]
async fn cold_backup_rejects_nonprivate_output_parent_without_creating_files() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    workspace.close().await.unwrap();
    let public_parent = root.path().join("public-output");
    fs::create_dir(&public_parent).unwrap();
    fs::set_permissions(&public_parent, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(public_parent.join("keep"), b"unchanged\n").unwrap();
    let destination = public_parent.join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(!output.status.success());
    assert!(!destination.exists());
    assert_eq!(
        fs::read(public_parent.join("keep")).unwrap(),
        b"unchanged\n"
    );
    assert_eq!(fs::read_dir(&public_parent).unwrap().count(), 1);
}

#[tokio::test]
async fn cold_backup_rejects_parent_with_extended_acl_even_when_mode_is_private() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    workspace.close().await.unwrap();
    let parent = root.path().join("output");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let fd = fs::File::open(&parent).unwrap();
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, permission, id) in [
        (1u16, 7u16, u32::MAX),
        (2, 7, 65_534),
        (4, 0, u32::MAX),
        (16, 0, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&permission.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    rustix::fs::fsetxattr(
        &fd,
        "system.posix_acl_access",
        &acl,
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
    assert_eq!(
        fs::metadata(&parent).unwrap().permissions().mode() & 0o077,
        0
    );
    let destination = parent.join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("backup_parent_acl_unsupported"));
    assert!(!destination.exists());
}

#[tokio::test]
async fn cold_backup_rejects_special_files_without_touching_the_source() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    workspace.close().await.unwrap();
    let socket = notes.join("external.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let before = fs::read(state.join("state.db")).unwrap();
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("backup_unsupported_file_type"));
    assert!(!destination.exists());
    assert!(
        fs::symlink_metadata(socket)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(fs::read(state.join("state.db")).unwrap(), before);
}

#[tokio::test]
async fn cold_backup_preserves_and_validates_committed_uncheckpointed_wal() {
    let root = private_tempdir();
    let (notes, state, config) = fixture_config(root.path());
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    let workspace_id = workspace.workspace_id().to_string();
    workspace.close().await.unwrap();
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(state.join("state.db"))
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let principal = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO principals(principal_id,workspace_id,workspace_role) VALUES(?,?,'reader')",
    )
    .bind(&principal)
    .bind(&workspace_id)
    .execute(&mut db)
    .await
    .unwrap();
    let original_db = fs::read(state.join("state.db")).unwrap();
    let original_wal = fs::read(state.join("state.db-wal")).unwrap();
    assert!(
        original_wal.len() > 32,
        "the committed change must remain in the WAL"
    );
    let destination = root.path().join("backup");
    let output = cli(&[
        Path::new("backup"),
        Path::new("--config"),
        &config,
        &destination,
    ]);
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(destination.join("state/state.db")).unwrap(),
        original_db
    );
    assert_eq!(
        fs::read(destination.join("state/state.db-wal")).unwrap(),
        original_wal
    );
    assert_eq!(fs::read(state.join("state.db")).unwrap(), original_db);
    assert_eq!(fs::read(state.join("state.db-wal")).unwrap(), original_wal);
    let verify = cli(&[Path::new("backup-verify"), &destination]);
    assert!(verify.status.success());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM principals WHERE principal_id=?")
        .bind(principal)
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(count, 1);
    db.close().await.unwrap();
}
