use super::*;
use notryx_server_storage::StorageStage;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::process::ExitStatusExt,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    time::Duration,
};

#[test]
#[ignore = "invoked only by the SIGKILL parent test"]
fn crash_worker() {
    let directory = std::path::PathBuf::from(std::env::var_os("NOTRYX_TEST_WORKSPACE").unwrap());
    let target = std::env::var("NOTRYX_TEST_CUT").unwrap();
    let recovery = std::env::var("NOTRYX_TEST_RECOVER").is_ok();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut workspace = Workspace::load(&directory.join("notes"), &directory.join("state"))
            .await
            .unwrap();
        if !recovery {
            workspace.recover().await.unwrap();
        }
        workspace.hook = Some(Box::new(move |phase| {
            if format!("{phase:?}") == target {
                println!("NOTRYX_CUT");
                std::io::stdout().flush().unwrap();
                loop {
                    std::thread::park();
                }
            }
            Ok(())
        }));
        if recovery {
            workspace.recover().await.unwrap();
        } else {
            let request =
                serde_json::from_slice(&fs::read(directory.join("request.json")).unwrap()).unwrap();
            workspace.save(request).await.unwrap();
        }
        panic!("requested crash boundary was not reached");
    });
}

fn kill_at(directory: &Path, target: Phase, recovery: bool) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tests::crash::crash_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("NOTRYX_TEST_WORKSPACE", directory)
        .env("NOTRYX_TEST_CUT", format!("{target:?}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if recovery {
        command.env("NOTRYX_TEST_RECOVER", "1");
    } else {
        command.env_remove("NOTRYX_TEST_RECOVER");
    }
    let child = command.spawn().unwrap();
    let (reached, status) = wait_for_cut_and_kill(child, Duration::from_secs(20));
    assert!(
        reached.is_ok(),
        "child did not reach {target:?}: {reached:?}, {status}"
    );
    assert_eq!(status.signal(), Some(9));
}

fn wait_for_cut_and_kill(
    mut child: Child,
    timeout: Duration,
) -> (Result<(), mpsc::RecvTimeoutError>, ExitStatus) {
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.trim() == "NOTRYX_CUT" {
                let _ = sender.send(());
                return;
            }
        }
    });
    let reached = receiver.recv_timeout(timeout);
    let _ = child.kill();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    (reached, status)
}

#[test]
#[ignore = "invoked only by the timeout parent test"]
fn silent_crash_worker() {
    loop {
        std::thread::park();
    }
}

#[test]
fn missing_cut_times_out_and_reaps_child() {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::crash::silent_crash_worker",
            "--ignored",
            "--nocapture",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let (reached, status) = wait_for_cut_and_kill(child, Duration::from_millis(100));
    assert_eq!(reached, Err(mpsc::RecvTimeoutError::Timeout));
    assert_eq!(status.signal(), Some(9));
}

#[tokio::test]
async fn sigkill_at_save_boundaries_replays_exactly_one_terminal_result() {
    for (phase, applied) in [
        (Phase::AfterAdmission, false),
        (Phase::Storage(StorageStage::AfterTemporaryFileSync), false),
        (Phase::Storage(StorageStage::AfterRename), true),
        (Phase::Storage(StorageStage::AfterParentDirectorySync), true),
        (Phase::BeforeFinish, true),
        (Phase::AfterFinish, true),
    ] {
        let rig = Rig::new().await;
        let request = rig.command(b"crash-safe bytes\n");
        fs::write(
            rig.dir.path().join("request.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        rig.workspace.close().await.unwrap();
        kill_at(rig.dir.path(), phase, false);
        let mut recovered =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(recovered.ready(), "{phase:?}");
        let receipt = recovered.save(request.clone()).await.unwrap();
        if applied {
            assert!(matches!(receipt.outcome, Outcome::Saved(_)), "{phase:?}");
            assert_eq!(
                recovered.read(rig.initial.node_id).await.unwrap().1,
                b"crash-safe bytes\n"
            );
        } else {
            assert_eq!(receipt.outcome, Outcome::NotApplied, "{phase:?}");
            assert_eq!(
                recovered.read(rig.initial.node_id).await.unwrap().0,
                rig.initial
            );
        }
        assert_eq!(recovered.save(request).await.unwrap(), receipt);
        let effects: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM change_sets WHERE operation_id IS NOT NULL")
                .fetch_one(&mut recovered.state.connection)
                .await
                .unwrap();
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM write_intents")
            .fetch_one(&mut recovered.state.connection)
            .await
            .unwrap();
        assert_eq!(effects, i64::from(applied));
        assert_eq!(pending, 0);
        recovered.close().await.unwrap();
    }
}

#[tokio::test]
async fn sigkill_during_recovery_can_repeat_fsync_and_finalize() {
    for (cut, expected_applied) in [
        (StorageStage::AfterRecoveryFileSync, false),
        (StorageStage::AfterRecoveryDirectorySync, true),
    ] {
        let mut rig = Rig::new().await;
        let interrupted = if expected_applied {
            Phase::BeforeFinish
        } else {
            Phase::AfterAdmission
        };
        rig.workspace.hook = Some(Box::new(move |phase| {
            if phase == interrupted {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }));
        let request = rig.command(b"recovery bytes\n");
        assert_eq!(
            rig.workspace.save(request.clone()).await,
            Err(Error::Storage)
        );
        rig.workspace.close().await.unwrap();
        kill_at(rig.dir.path(), Phase::Storage(cut), true);
        let mut recovered =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert!(recovered.ready());
        let receipt = recovered.save(request).await.unwrap();
        assert_eq!(
            matches!(receipt.outcome, Outcome::Saved(_)),
            expected_applied
        );
        recovered.close().await.unwrap();
    }
}
