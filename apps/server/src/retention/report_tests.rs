use super::RetentionReport;
use crate::{Error, Outcome, SaveContent, Workspace};
use notryx_domain::ChangeSeq;
use sha2::{Digest, Sha256};
use std::fs;
use uuid::Uuid;

async fn retain_with_report(
    workspace: &mut Workspace,
    retention_seconds: u64,
    budget_bytes: u64,
    now_seconds: i64,
) -> Result<RetentionReport, Error> {
    workspace
        .retain_journal_report(retention_seconds, budget_bytes, now_seconds)
        .await
}

#[tokio::test]
async fn journal_retention_report_distinguishes_expired_and_budget_forced_changes() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("Synthetic.md"), b"synthetic before\n").unwrap();
    let document = workspace.adopt_root_document("Synthetic.md").await.unwrap();
    let epoch = workspace.epoch();
    let first = retain_with_report(&mut workspace, 10, 1_000_000, 1_000)
        .await
        .unwrap();
    assert_eq!(first.before_floor, 0);
    assert_eq!(first.after_floor, 0);
    assert_eq!(first.expired_count, 0);
    assert_eq!(first.budget_forced_count, 0);
    assert_eq!(first.budget_bytes, 1_000_000);
    assert!(!first.budget_exceeded);
    assert!(first.logical_bytes > 0);

    let bytes = b"synthetic after\n";
    let stream_id = workspace.register_stream().await.unwrap();
    let receipt = workspace
        .save(SaveContent {
            epoch,
            stream_id,
            operation_seq: 1,
            operation_id: Uuid::now_v7().to_string().parse().unwrap(),
            node_id: document.node_id,
            expected_content_rev: document.content_rev,
            content_hash: Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: bytes.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, Outcome::Saved(_)));
    let result = retain_with_report(&mut workspace, 10, 0, 1_012)
        .await
        .unwrap();
    assert_eq!(result.before_floor, 0);
    assert_eq!(result.after_floor, 2);
    assert_eq!(result.expired_count, 1);
    assert_eq!(result.budget_forced_count, 1);
    assert_eq!(result.budget_bytes, 0);
    assert!(result.budget_exceeded);
    assert!(result.logical_bytes > 0);
    let info = workspace.server_info().await.unwrap();
    assert_eq!(info.workspace_epoch, epoch);
    assert_eq!(info.head_seq, ChangeSeq::new(2));
    assert_eq!(info.journal_floor_seq, ChangeSeq::new(2));
    assert_eq!(fs::read(notes.join("Synthetic.md")).unwrap(), bytes);
    workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&notes, &state).await.unwrap();
    assert_eq!(
        reopened.server_info().await.unwrap().journal_floor_seq,
        ChangeSeq::new(2)
    );
    assert_eq!(fs::read(notes.join("Synthetic.md")).unwrap(), bytes);
    reopened.close().await.unwrap();
}
