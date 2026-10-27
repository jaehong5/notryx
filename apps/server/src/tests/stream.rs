use super::*;
use crate::{AbandonOperation, AckOperations};

async fn seed_terminals(rig: &mut Rig, count: u64) -> Vec<notryx_domain::OperationId> {
    let mut ids = Vec::new();
    let mut tx = rig.workspace.state.connection.begin().await.unwrap();
    for sequence in 1..=count {
        let operation = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO operation_receipts(operation_id,stream_id,operation_seq,fingerprint,outcome,pending) VALUES(?,?,?,?,'\"Abandoned\"',0)")
            .bind(&operation)
            .bind(rig.stream.to_string())
            .bind(sequence.to_string())
            .bind(digest(operation.as_bytes()))
            .execute(&mut *tx)
            .await
            .unwrap();
        ids.push(operation.parse().unwrap());
    }
    sqlx::query("UPDATE operation_streams SET last_seq=? WHERE stream_id=?")
        .bind(count.to_string())
        .bind(rig.stream.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    ids
}

fn ack(rig: &Rig, through: u64) -> AckOperations {
    AckOperations {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        ack_through: through,
    }
}

fn abandon(rig: &Rig, sequence: u64, operation_id: notryx_domain::OperationId) -> AbandonOperation {
    AbandonOperation {
        epoch: rig.workspace.epoch(),
        stream_id: rig.stream,
        operation_seq: sequence,
        operation_id,
    }
}

#[tokio::test]
async fn ack_retires_only_a_contiguous_terminal_prefix_and_is_idempotent() {
    let mut rig = Rig::new().await;
    let ids = seed_terminals(&mut rig, 110).await;
    for through in [0, 9, 10, 99, 100] {
        let state = rig
            .workspace
            .ack_operations(ack(&rig, through))
            .await
            .unwrap();
        assert_eq!(state.last_seq, 110);
        assert_eq!(state.retired_through, through);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(remaining, (110 - through) as i64);
    }
    let stale = rig.workspace.ack_operations(ack(&rig, 9)).await.unwrap();
    assert_eq!(stale.retired_through, 100);
    assert_eq!(
        rig.workspace.ack_operations(ack(&rig, 111)).await,
        Err(Error::InvalidAck)
    );
    assert_eq!(
        rig.workspace.state.operation(rig.stream, ids[99]).await,
        Err(Error::OperationRetired)
    );
    assert_eq!(
        rig.workspace
            .state
            .operation(rig.stream, ids[100])
            .await
            .unwrap()
            .operation_seq,
        101
    );
    rig.workspace.ack_operations(ack(&rig, 110)).await.unwrap();
    let mut replay = rig.command(b"never repeat\n");
    replay.operation_id = ids[0];
    assert_eq!(
        rig.workspace.save(replay.clone()).await,
        Err(Error::OperationRetired)
    );
    replay.operation_seq = 111;
    assert_eq!(
        rig.workspace.save(replay).await,
        Err(Error::IdempotencyMismatch)
    );
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    let epoch = rig.workspace.epoch();
    rig.workspace.close().await.unwrap();
    let mut reopened =
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
            .await
            .unwrap();
    let state = reopened
        .ack_operations(AckOperations {
            epoch,
            stream_id: rig.stream,
            ack_through: 110,
        })
        .await
        .unwrap();
    assert_eq!(state.retired_through, 110);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn ack_failures_roll_back_watermark_identifiers_and_receipt_removal() {
    for trigger in [
        "CREATE TRIGGER fail_ack BEFORE UPDATE OF retired_through ON operation_streams BEGIN SELECT RAISE(ABORT,'synthetic watermark failure'); END;",
        "CREATE TRIGGER fail_ack BEFORE INSERT ON retired_operations BEGIN SELECT RAISE(ABORT,'synthetic retired identity failure'); END;",
        "CREATE TRIGGER fail_ack BEFORE DELETE ON operation_receipts BEGIN SELECT RAISE(ABORT,'synthetic receipt removal failure'); END;",
    ] {
        let mut rig = Rig::new().await;
        let ids = seed_terminals(&mut rig, 3).await;
        sqlx::raw_sql(trigger)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!(
            rig.workspace.ack_operations(ack(&rig, 3)).await,
            Err(Error::Storage)
        );
        sqlx::raw_sql("DROP TRIGGER fail_ack")
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        let watermark: String =
            sqlx::query_scalar("SELECT retired_through FROM operation_streams WHERE stream_id=?")
                .bind(rig.stream.to_string())
                .fetch_one(&mut rig.workspace.state.connection)
                .await
                .unwrap();
        assert_eq!(watermark, "0");
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        let retired: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retired_operations")
            .fetch_one(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        assert_eq!((receipts, retired), (3, 0));
        rig.workspace.close().await.unwrap();
        let mut reopened =
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state"))
                .await
                .unwrap();
        assert_eq!(
            reopened
                .state
                .operation(rig.stream, ids[0])
                .await
                .unwrap()
                .outcome,
            Outcome::Abandoned
        );
        assert_eq!(
            reopened
                .ack_operations(AckOperations {
                    epoch: reopened.epoch(),
                    stream_id: rig.stream,
                    ack_through: 3
                })
                .await
                .unwrap()
                .retired_through,
            3
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn pending_work_cannot_be_acknowledged_or_abandoned() {
    let mut rig = Rig::new().await;
    let request = rig.command(b"pending original payload\n");
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.workspace.save(request.clone()).await,
        Err(Error::Storage)
    );
    rig.workspace.hook = None;
    assert_eq!(
        rig.workspace.ack_operations(ack(&rig, 1)).await,
        Err(Error::OperationPending)
    );
    assert_eq!(
        rig.workspace
            .ack_operations(ack(&rig, 0))
            .await
            .unwrap()
            .retired_through,
        0
    );
    assert_eq!(
        rig.workspace
            .abandon_operation(abandon(&rig, 1, request.operation_id))
            .await,
        Err(Error::OperationPending)
    );
    assert_eq!(
        rig.workspace
            .state
            .operation(rig.stream, request.operation_id)
            .await
            .unwrap()
            .outcome,
        Outcome::Pending
    );
    assert_eq!(rig.workspace.state.pending().await.unwrap().len(), 1);
    assert!(!rig.workspace.ready());
}

#[tokio::test]
async fn abandon_consumes_only_the_next_identity_and_never_changes_documents() {
    let mut rig = Rig::new().await;
    let operation_id = Uuid::now_v7().to_string().parse().unwrap();
    assert_eq!(
        rig.workspace
            .abandon_operation(abandon(&rig, 0, operation_id))
            .await,
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        rig.workspace
            .abandon_operation(abandon(&rig, 2, operation_id))
            .await,
        Err(Error::OperationGap)
    );
    let request = abandon(&rig, 1, operation_id);
    let record = rig
        .workspace
        .abandon_operation(request.clone())
        .await
        .unwrap();
    assert_eq!(record.outcome, Outcome::Abandoned);
    assert_eq!(record.operation_seq, 1);
    rig.workspace.ready = false;
    assert_eq!(
        rig.workspace.abandon_operation(request).await.unwrap(),
        record
    );
    let next_id = Uuid::now_v7().to_string().parse().unwrap();
    assert_eq!(
        rig.workspace
            .abandon_operation(abandon(&rig, 2, next_id))
            .await,
        Err(Error::RecoveryRequired)
    );
    rig.workspace.ready = true;
    let mut late_payload = rig.command(b"must never execute abandoned work\n");
    late_payload.operation_id = operation_id;
    assert_eq!(
        rig.workspace.save(late_payload).await,
        Err(Error::IdempotencyMismatch)
    );
    assert_eq!(
        rig.workspace.state.head_seq().await.unwrap(),
        rig.initial.last_change_seq
    );
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
    rig.workspace.ack_operations(ack(&rig, 1)).await.unwrap();
    assert_eq!(
        rig.workspace
            .abandon_operation(abandon(&rig, 1, operation_id))
            .await,
        Err(Error::OperationRetired)
    );
}

#[tokio::test]
async fn abandon_returns_the_original_terminal_result_without_rewriting_it() {
    let mut rig = Rig::new().await;
    let request = rig.command(b"saved before abandon\n");
    let receipt = rig.workspace.save(request.clone()).await.unwrap();
    let abandoned = rig
        .workspace
        .abandon_operation(abandon(&rig, 1, request.operation_id))
        .await
        .unwrap();
    assert_eq!(abandoned.outcome, receipt.outcome);
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().1,
        b"saved before abandon\n"
    );
    let different_id = Uuid::now_v7().to_string().parse().unwrap();
    assert_eq!(
        rig.workspace
            .abandon_operation(abandon(&rig, 1, different_id))
            .await,
        Err(Error::IdempotencyMismatch)
    );
}

#[tokio::test]
async fn startup_rejects_corrupt_retired_and_live_ranges_without_repairing_them() {
    for corruption in [
        "UPDATE operation_streams SET retired_through='4'",
        "UPDATE operation_streams SET retired_through='01'",
        "DELETE FROM retired_operations WHERE operation_seq='1'",
        "DELETE FROM operation_receipts WHERE operation_seq='3'",
        "UPDATE operation_receipts SET operation_id=(SELECT operation_id FROM retired_operations WHERE operation_seq='1')",
        "UPDATE retired_operations SET operation_id='not-an-operation-id' WHERE operation_seq='1'",
    ] {
        let mut rig = Rig::new().await;
        seed_terminals(&mut rig, 3).await;
        rig.workspace.ack_operations(ack(&rig, 2)).await.unwrap();
        sqlx::raw_sql(corruption)
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        rig.workspace.close().await.unwrap();
        assert!(matches!(
            Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await,
            Err(Error::InvalidFormat)
        ));
    }
}

#[tokio::test]
async fn ack_compares_sequences_above_signed_integer_range_without_rounding() {
    let mut rig = Rig::new().await;
    let through = i64::MAX as u64 + 1;
    // The prior prefix is abstracted here; this fixture tests arithmetic, not startup integrity.
    sqlx::query("UPDATE operation_streams SET retired_through=?,last_seq=? WHERE stream_id=?")
        .bind((through - 1).to_string())
        .bind((through + 1).to_string())
        .bind(rig.stream.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for sequence in [through, through + 1] {
        let id = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO operation_receipts(operation_id,stream_id,operation_seq,fingerprint,outcome,pending) VALUES(?,?,?,?,'\"Abandoned\"',0)")
            .bind(&id)
            .bind(rig.stream.to_string())
            .bind(sequence.to_string())
            .bind(digest(id.as_bytes()))
            .execute(&mut rig.workspace.state.connection)
            .await
            .unwrap();
        ids.push(id.parse().unwrap());
    }
    let state = rig
        .workspace
        .ack_operations(ack(&rig, through))
        .await
        .unwrap();
    assert_eq!(state.retired_through, through);
    assert_eq!(
        rig.workspace.state.operation(rig.stream, ids[0]).await,
        Err(Error::OperationRetired)
    );
    assert_eq!(
        rig.workspace
            .state
            .operation(rig.stream, ids[1])
            .await
            .unwrap()
            .operation_seq,
        through + 1
    );
}

#[tokio::test]
async fn ack_releases_receipt_capacity_without_rewinding_the_admission_sequence() {
    let mut rig = Rig::new().await;
    seed_terminals(&mut rig, 10_000).await;
    let next_id = Uuid::now_v7().to_string().parse().unwrap();
    let cancellation = abandon(&rig, 10_001, next_id);
    assert_eq!(
        rig.workspace.abandon_operation(cancellation.clone()).await,
        Err(Error::ReceiptLimit)
    );
    let mut save = rig.command(b"must wait for capacity\n");
    save.operation_seq = 10_001;
    assert_eq!(rig.workspace.save(save).await, Err(Error::ReceiptLimit));
    let state = rig.workspace.ack_operations(ack(&rig, 1)).await.unwrap();
    assert_eq!((state.retired_through, state.last_seq), (1, 10_000));
    let accepted = rig.workspace.abandon_operation(cancellation).await.unwrap();
    assert_eq!(accepted.operation_seq, 10_001);
    assert_eq!(accepted.outcome, Outcome::Abandoned);
    let state = rig.workspace.ack_operations(ack(&rig, 0)).await.unwrap();
    assert_eq!((state.retired_through, state.last_seq), (1, 10_001));
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_receipts")
        .fetch_one(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    assert_eq!(active, 10_000);
    assert_eq!(
        rig.workspace.read(rig.initial.node_id).await.unwrap().0,
        rig.initial
    );
}

#[tokio::test]
async fn startup_rejects_a_pending_operation_followed_by_a_later_receipt() {
    let mut rig = Rig::new().await;
    let request = rig.command(b"pending before a corrupt later receipt\n");
    rig.workspace.hook = Some(Box::new(|phase| {
        if phase == Phase::AfterAdmission {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }));
    assert_eq!(rig.workspace.save(request).await, Err(Error::Storage));
    let later = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO operation_receipts(operation_id,stream_id,operation_seq,fingerprint,outcome,pending) VALUES(?,?,'2',?,'\"Abandoned\"',0)")
        .bind(&later)
        .bind(rig.stream.to_string())
        .bind(digest(later.as_bytes()))
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    sqlx::query("UPDATE operation_streams SET last_seq='2' WHERE stream_id=?")
        .bind(rig.stream.to_string())
        .execute(&mut rig.workspace.state.connection)
        .await
        .unwrap();
    let before = fs::read(rig.dir.path().join("notes/Document.md")).unwrap();
    rig.workspace.close().await.unwrap();
    assert!(matches!(
        Workspace::open(&rig.dir.path().join("notes"), &rig.dir.path().join("state")).await,
        Err(Error::InvalidFormat)
    ));
    assert_eq!(
        fs::read(rig.dir.path().join("notes/Document.md")).unwrap(),
        before
    );
}
