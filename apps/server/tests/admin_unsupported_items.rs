use notryx_protocol::admin::{AdminRequest, AdminResponse, AdminResult, UnsupportedItemReason};
use notryx_server::Workspace;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[tokio::test]
async fn admin_unsupported_items_lists_actual_entries_without_private_targets() {
    let dir = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    let outside = dir.path().join("outside");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    fs::create_dir(&outside).unwrap();
    let workspace = Workspace::initialize(&notes, &state).await.unwrap();
    workspace.close().await.unwrap();
    fs::write(
        outside.join("target-secret.md"),
        b"private-payload-SENTINEL\n",
    )
    .unwrap();
    symlink(&outside, notes.join("Linked folder")).unwrap();
    fs::hard_link(outside.join("target-secret.md"), notes.join("Hard link")).unwrap();
    let notes_fd = fs::File::open(&notes).unwrap();
    rustix::fs::mkfifoat(
        &notes_fd,
        "Named pipe",
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    let invalid = OsStr::from_bytes(b"invalid-\xff");
    fs::write(notes.join(invalid), b"invalid name bytes\n").unwrap();
    fs::write(
        notes.join("invalid\\name.txt"),
        b"invalid compatible bytes\n",
    )
    .unwrap();
    fs::write(notes.join("Safe.md"), b"safe markdown\n").unwrap();
    symlink(&outside, notes.join(".git")).unwrap();
    let config = dir.path().join("server.toml");
    let socket = dir.path().join("admin.sock");
    fs::write(&config, format!(
        "[server]\nlisten = '127.0.0.1:0'\nadmin_socket = '{}'\n[workspace]\nnotes_root = '{}'\nstate_root = '{}'\n",
        socket.display(), notes.display(), state.display()
    )).unwrap();
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
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let log = fs::read_to_string(&log_path).unwrap();
            if log.lines().any(|line| line == "known_file_scan_incomplete") && socket.exists() {
                break;
            }
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited before scan"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let request = AdminRequest::UnsupportedItems {
        snapshot_id: None,
        offset: 0,
        limit: 20,
    };
    let response = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let response = notryx_server::admin::request(&socket, &request)
                .await
                .unwrap();
            if matches!(
                &response,
                AdminResponse::Ok {
                    result: AdminResult::UnsupportedItems {
                        snapshot_id: Some(_),
                        coverage_complete: true,
                        stale: false,
                        ..
                    }
                }
            ) {
                break response;
            }
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited before diagnostic snapshot"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("complete diagnostic snapshot was not published");
    let AdminResponse::Ok {
        result:
            AdminResult::UnsupportedItems {
                snapshot_id,
                previous_snapshot_id,
                entries,
                coverage_complete,
                stale,
                truncated,
                next_offset,
            },
    } = response
    else {
        panic!("expected unsupported-item diagnostics");
    };
    let cli = Command::new(env!("CARGO_BIN_EXE_notryx-server"))
        .arg("admin")
        .arg("--config")
        .arg(&config)
        .arg("unsupported-items")
        .output()
        .unwrap();
    let cli_output = String::from_utf8(cli.stdout).unwrap();
    let log = fs::read_to_string(&log_path).unwrap();
    let outside_bytes = fs::read(outside.join("target-secret.md")).unwrap();
    let safe_bytes = fs::read(notes.join("Safe.md")).unwrap();

    assert!(snapshot_id.is_some());
    assert!(previous_snapshot_id.is_none());
    assert!(coverage_complete && !stale && !truncated);
    assert_eq!(next_offset, None);
    for (name, reason) in [
        (b"Linked folder".as_slice(), UnsupportedItemReason::Symlink),
        (b"Hard link".as_slice(), UnsupportedItemReason::HardLink),
        (b"Named pipe".as_slice(), UnsupportedItemReason::SpecialFile),
        (
            b"invalid-\xff".as_slice(),
            UnsupportedItemReason::InvalidName,
        ),
        (
            b"invalid\\name.txt".as_slice(),
            UnsupportedItemReason::InvalidName,
        ),
    ] {
        assert!(
            entries.iter().any(|entry| entry.parent_path.is_empty()
                && entry.name_hex == hex(name)
                && entry.reason == reason),
            "missing unsupported entry {}",
            hex(name)
        );
    }
    assert_eq!(entries.len(), 5);
    assert!(
        entries
            .iter()
            .find(|entry| entry.name_hex == hex(b"invalid-\xff"))
            .unwrap()
            .name_utf8
            .is_none()
    );
    assert!(cli.status.success());
    assert!(cli_output.contains("Linked folder"));
    assert!(!cli_output.contains(dir.path().to_str().unwrap()));
    assert!(!cli_output.contains("target-secret.md"));
    assert!(!cli_output.contains("private-payload-SENTINEL"));
    assert!(!log.contains("Linked folder"));
    assert!(!log.contains(dir.path().to_str().unwrap()));
    assert!(!log.contains("private-payload-SENTINEL"));
    assert_eq!(outside_bytes, b"private-payload-SENTINEL\n");
    assert_eq!(safe_bytes, b"safe markdown\n");
}
