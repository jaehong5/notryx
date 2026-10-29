use notryx_server::Workspace;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy)]
enum ScanCase {
    Unchanged,
    SingleMissing,
    MultipleMissing,
}

async fn scan_log_case(case: ScanCase) {
    let dir = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let source = notes.join("private-doc-SENTINEL.md");
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(&source, b"private-payload-SENTINEL\n").unwrap();
    let document = workspace
        .adopt_root_document("private-doc-SENTINEL.md")
        .await
        .unwrap();
    let other = if matches!(case, ScanCase::MultipleMissing) {
        fs::write(
            notes.join("private-other-SENTINEL.md"),
            b"other-payload-SENTINEL\n",
        )
        .unwrap();
        Some(
            workspace
                .adopt_root_document("private-other-SENTINEL.md")
                .await
                .unwrap(),
        )
    } else {
        None
    };
    workspace.close().await.unwrap();
    let options = SqliteConnectOptions::new()
        .filename(state.join("state.db"))
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(1));
    let mut database = SqliteConnection::connect_with(&options).await.unwrap();
    let before_head: String =
        sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton = 1")
            .fetch_one(&mut database)
            .await
            .unwrap();
    let before_nodes: Vec<(String, i64, Option<String>, String, String, String)> = sqlx::query_as(
        "SELECT node_id, deleted, content_hash, content_rev, meta_rev, last_change_seq FROM nodes WHERE name IN (?, ?) ORDER BY name",
    )
    .bind("private-doc-SENTINEL.md")
    .bind("private-other-SENTINEL.md")
    .fetch_all(&mut database)
    .await
    .unwrap();
    database.close().await.unwrap();
    if !matches!(case, ScanCase::Unchanged) {
        fs::remove_file(&source).unwrap();
    }
    if matches!(case, ScanCase::MultipleMissing) {
        fs::remove_file(notes.join("private-other-SENTINEL.md")).unwrap();
    }
    let config = dir.path().join("server.toml");
    fs::write(
        &config,
        format!(
            "[server]\nlisten = '127.0.0.1:0'\nadmin_socket = '{}'\n[workspace]\nnotes_root = '{}'\nstate_root = '{}'\n",
            dir.path().join("admin.sock").display(),
            notes.display(),
            state.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let log_path = dir.path().join("stderr.log");
    let log_file = fs::File::create(&log_path).unwrap();
    let mut server = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_notryx-server"))
            .arg("serve")
            .arg("--config")
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()
            .unwrap(),
    );
    let terminal = if matches!(case, ScanCase::MultipleMissing) {
        "known_file_scan_incomplete"
    } else {
        "known_file_scan_complete"
    };
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let log = fs::read_to_string(&log_path).unwrap();
            if log.lines().any(|line| line == terminal) {
                break log;
            }
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited before scan log"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let output = match result {
        Ok(output) => output,
        Err(_) => {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            let events: Vec<_> = log
                .lines()
                .filter(|line| {
                    matches!(
                        *line,
                        "known_file_scan_started"
                            | "known_file_scan_complete"
                            | "known_file_scan_incomplete"
                            | "known_file_scan_stopped"
                    )
                })
                .collect();
            let options = SqliteConnectOptions::new()
                .filename(state.join("state.db"))
                .read_only(true)
                .create_if_missing(false)
                .busy_timeout(Duration::from_secs(1));
            let state = match SqliteConnection::connect_with(&options).await {
                Ok(mut connection) => {
                    let head = sqlx::query_scalar::<_, String>(
                        "SELECT head_seq FROM workspaces WHERE singleton = 1",
                    )
                    .fetch_optional(&mut connection)
                    .await
                    .ok()
                    .flatten();
                    let deleted =
                        sqlx::query_scalar::<_, i64>("SELECT deleted FROM nodes WHERE name = ?")
                            .bind("private-doc-SENTINEL.md")
                            .fetch_optional(&mut connection)
                            .await
                            .ok()
                            .flatten();
                    (head, deleted)
                }
                Err(_) => (None, None),
            };
            panic!(
                "scan log was not emitted: events={events:?}, head_seq={:?}, deleted={:?}",
                state.0, state.1
            );
        }
    };
    let events: Vec<_> = output
        .lines()
        .filter(|line| line.starts_with("known_file_scan_"))
        .collect();
    assert_eq!(events, ["known_file_scan_started", terminal]);
    assert!(!output.contains("private-doc-SENTINEL"));
    assert!(!output.contains("private-payload-SENTINEL"));
    assert!(!output.contains("private-other-SENTINEL"));
    assert!(!output.contains("other-payload-SENTINEL"));
    assert!(!output.contains(dir.path().to_str().unwrap()));

    let mut database = SqliteConnection::connect_with(&options).await.unwrap();
    let after_head: String =
        sqlx::query_scalar("SELECT head_seq FROM workspaces WHERE singleton = 1")
            .fetch_one(&mut database)
            .await
            .unwrap();
    let after_nodes: Vec<(String, i64, Option<String>, String, String, String)> = sqlx::query_as(
        "SELECT node_id, deleted, content_hash, content_rev, meta_rev, last_change_seq FROM nodes WHERE name IN (?, ?) ORDER BY name",
    )
    .bind("private-doc-SENTINEL.md")
    .bind("private-other-SENTINEL.md")
    .fetch_all(&mut database)
    .await
    .unwrap();
    match case {
        ScanCase::Unchanged | ScanCase::MultipleMissing => {
            assert_eq!(after_head, before_head);
            assert_eq!(after_nodes, before_nodes);
        }
        ScanCase::SingleMissing => {
            assert_eq!(
                after_head.parse::<u64>().unwrap(),
                before_head.parse::<u64>().unwrap() + 1
            );
            assert_eq!(after_nodes.len(), 1);
            assert_eq!(after_nodes[0].0, document.node_id.to_string());
            assert_eq!(after_nodes[0].1, 1);
            assert_eq!(after_nodes[0].2, before_nodes[0].2);
            assert_eq!(after_nodes[0].5, after_head);
        }
    }
    assert_eq!(
        fs::read(state.join("objects/sha256").join(&document.content_hash)).unwrap(),
        b"private-payload-SENTINEL\n"
    );
    if let Some(other) = other {
        assert_eq!(
            fs::read(state.join("objects/sha256").join(&other.content_hash)).unwrap(),
            b"other-payload-SENTINEL\n"
        );
    }
}

#[tokio::test]
async fn cli_logs_known_file_scan_start_and_completion_without_private_data() {
    scan_log_case(ScanCase::Unchanged).await;
}

#[tokio::test]
async fn cli_logs_known_file_scan_failure_without_private_data() {
    scan_log_case(ScanCase::MultipleMissing).await;
}

#[tokio::test]
async fn cli_logs_single_missing_file_as_completed_deletion_without_private_data() {
    scan_log_case(ScanCase::SingleMissing).await;
}
