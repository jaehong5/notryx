#![cfg(all(target_os = "linux", feature = "test-hooks"))]

use notryx_server_storage::{FileStore, StorageError, StorageStage};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn kill_at(notes: &std::path::Path, state: &std::path::Path, operation: &str, stage: StorageStage) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_storage-crash-probe"))
        .arg(notes)
        .arg(state)
        .arg(operation)
        .arg(format!("{stage:?}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line == "STORAGE_CUT_REACHED" {
                let _ = sender.send(());
                return;
            }
        }
    });
    let reached = receiver.recv_timeout(Duration::from_secs(20));
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    assert!(
        reached.is_ok(),
        "{stage:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());
}

#[test]
fn process_kill_at_file_and_directory_boundaries_preserves_recoverable_bytes() {
    for (operation, stage, expected) in [
        (
            "replace",
            StorageStage::AfterTemporaryFileSync,
            b"before".as_slice(),
        ),
        ("replace", StorageStage::AfterRename, b"proposed".as_slice()),
        (
            "replace",
            StorageStage::AfterParentDirectorySync,
            b"proposed".as_slice(),
        ),
        (
            "recovery",
            StorageStage::AfterRecoveryFileSync,
            b"before".as_slice(),
        ),
        (
            "recovery",
            StorageStage::AfterRecoveryDirectorySync,
            b"before".as_slice(),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&state).unwrap();
        let store = FileStore::init(
            &notes,
            &state,
            "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
        )
        .unwrap();
        std::fs::write(state.join("state.db"), b"state adapter fixture").unwrap();
        std::fs::write(notes.join("document.md"), b"before").unwrap();
        let before_hash = store.put_object(b"before").unwrap();
        let proposed_hash = store.put_object(b"proposed").unwrap();
        drop(store);
        kill_at(&notes, &state, operation, stage);
        let reopened = FileStore::open(&notes, &state).unwrap();
        let observed = reopened.observe("document.md").unwrap();
        assert_eq!(observed.bytes, expected, "{stage:?}");
        assert_eq!(reopened.read_object(&before_hash).unwrap(), b"before");
        assert_eq!(reopened.read_object(&proposed_hash).unwrap(), b"proposed");
        reopened
            .sync_observed("document.md", &observed.stamp)
            .unwrap();
    }
}

#[test]
fn process_kill_during_object_publication_never_exposes_partial_objects() {
    for stage in [
        StorageStage::AfterObjectFileSync,
        StorageStage::AfterObjectPublish,
        StorageStage::AfterObjectDirectorySync,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&state).unwrap();
        let store = FileStore::init(
            &notes,
            &state,
            "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
        )
        .unwrap();
        std::fs::write(state.join("state.db"), b"state adapter fixture").unwrap();
        drop(store);
        kill_at(&notes, &state, "object", stage);
        let reopened = FileStore::open(&notes, &state).unwrap();
        let proposed_hash: String = Sha256::digest(b"proposed")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let observed = reopened.read_object(&proposed_hash);
        if stage == StorageStage::AfterObjectFileSync {
            assert_eq!(observed, Err(StorageError::NotFound));
        } else {
            assert_eq!(observed.unwrap(), b"proposed");
        }
        assert_eq!(reopened.put_object(b"proposed").unwrap(), proposed_hash);
    }
}
