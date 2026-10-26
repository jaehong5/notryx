#![cfg(target_os = "linux")]

use notryx_client_store::ClientStore;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn acknowledged_checkpoint_survives_sigkill() {
    if let Some(path) = std::env::var_os("NOTRYX_M0_CRASH_DB") {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut store = ClientStore::open(path).await.unwrap();
            let opened = store
                .open_document("document-a", b"original base", "base-revision")
                .await
                .unwrap();
            store
                .save_checkpoint(
                    "document-a",
                    &opened.edit_session_id,
                    3,
                    b"acknowledged draft",
                )
                .await
                .unwrap();
            println!("CHECKPOINT_COMMITTED");
            std::io::stdout().flush().unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            store.close().await.unwrap();
        });
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.db");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "acknowledged_checkpoint_survives_sigkill",
            "--nocapture",
        ])
        .env("NOTRYX_M0_CRASH_DB", &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("CHECKPOINT_COMMITTED") {
                let _ = sender.send(());
                break;
            }
        }
    });
    let acknowledged = receiver.recv_timeout(Duration::from_secs(15));
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    assert!(
        acknowledged.is_ok(),
        "child did not acknowledge a checkpoint: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut reopened = ClientStore::open(&path).await.unwrap();
        let draft = reopened
            .read_checkpoint("document-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.generation, 3);
        assert_eq!(draft.bytes, b"acknowledged draft");
        assert_eq!(draft.base_bytes, b"original base");
        assert_eq!(draft.base_revision, "base-revision");
        reopened.close().await.unwrap();
    });
}
