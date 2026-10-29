include!("frozen_content_inspection_tests.rs");

use crate::frozen_content_native::{FrozenContentInspection, finish_frozen_content_inspection};
use notryx_protocol::WorkspaceFrozenContentInspectRequest;

const FINISH_UNIMPLEMENTED: &str = "frozen_content_finish_unimplemented";

fn finish_request(rig: &Rig) -> WorkspaceFrozenContentInspectRequest {
    WorkspaceFrozenContentInspectRequest {
        renderer_session_id: "owner".into(),
        current_binding: crate::to_contract(rig.current.clone()),
        old_binding: crate::to_contract(rig.old.clone()),
        operation_id: rig.sent.operation_id.clone(),
    }
}

async fn finish_snapshot(rig: &Rig) -> FrozenContentInspection {
    inspect_frozen_content_for_renderer(
        &rig.runtime,
        "owner",
        &rig.current,
        &rig.old,
        &rig.sent.operation_id,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn frozen_content_finish_unchanged_snapshot_returns_same_bindings_and_generations() {
    let rig = rig().await;
    let before = finish_snapshot(&rig).await;
    let result = finish_frozen_content_inspection(&rig.runtime, finish_request(&rig), before)
        .await
        .unwrap();
    assert_eq!(
        result.current_binding,
        crate::to_contract(rig.current.clone())
    );
    assert_eq!(result.old_binding, crate::to_contract(rig.old.clone()));
    assert_eq!(result.operation_id, rig.sent.operation_id);
    assert_eq!(result.operation_generation, "1");
    assert_eq!(result.local_generation, "2");
    assert!(result.outcome_unknown);
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_finish_rejects_owner_and_registration_replacement() {
    for change in ["owner", "registration"] {
        let rig = rig().await;
        let before = finish_snapshot(&rig).await;
        let old_before = {
            let mut state = rig.runtime.0.lock().await;
            (
                state
                    .store
                    .read_registered_checkpoint(&rig.old, &rig.document)
                    .await
                    .unwrap()
                    .unwrap(),
                state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
            )
        };
        if change == "owner" {
            rig.runtime
                .0
                .lock()
                .await
                .gate
                .replace_owner("replacement-owner".into());
        } else {
            let config = Config {
                sync: Default::default(),
                server: ServerConfig {
                    listen: rig.server.local_addr(),
                    admin_socket: rig._directory.path().join("admin.sock"),
                },
                workspace: WorkspaceConfig {
                    notes_root: rig._directory.path().join("notes"),
                    state_root: rig._directory.path().join("server-state"),
                    uploads: Default::default(),
                },
            };
            let pairing = server_test_support::pairing_request(&config).await.0;
            let replacement = Arc::new(
                Session::pair(
                    &format!("http://{}", rig.server.local_addr()),
                    pairing.workspace_id,
                    &pairing.pairing_secret,
                )
                .await
                .unwrap(),
            );
            let binding = registration_binding(replacement.binding());
            let mut state = rig.runtime.0.lock().await;
            let pair = state.gate.begin_pair("owner").unwrap();
            state.store.activate_registration(&binding).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
            state.session = Some(replacement);
        }
        let result =
            finish_frozen_content_inspection(&rig.runtime, finish_request(&rig), before).await;
        assert!(matches!(result, Err(ref error) if error != FINISH_UNIMPLEMENTED));
        let mut state = rig.runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&rig.old, &rig.document)
                .await
                .unwrap()
                .unwrap(),
            old_before.0
        );
        assert_eq!(
            state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
            old_before.1
        );
        drop(state);
        assert_eq!(
            std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
            SENT
        );
        rig.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn frozen_content_finish_rejects_newer_recovery_checkpoint_without_losing_it() {
    let rig = rig().await;
    let before = finish_snapshot(&rig).await;
    let outbox = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .list_outbox(&rig.old, 0, 100)
        .await
        .unwrap();
    {
        let mut state = rig.runtime.0.lock().await;
        let opened = state
            .store
            .open_recovery_draft(&rig.old, &rig.document)
            .await
            .unwrap();
        state
            .store
            .save_recovery_checkpoint(
                &rig.old,
                &rig.document,
                &opened.edit_session_id,
                3,
                b"synthetic recovery after display\n",
            )
            .await
            .unwrap();
        state
            .store
            .close_recovery_draft(&rig.old, &rig.document, &opened.edit_session_id)
            .await
            .unwrap();
    }
    let result = finish_frozen_content_inspection(&rig.runtime, finish_request(&rig), before).await;
    assert!(matches!(result, Err(ref error) if error != FINISH_UNIMPLEMENTED));
    let mut state = rig.runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(&rig.old, &rig.document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 3);
    assert_eq!(draft.bytes, b"synthetic recovery after display\n");
    assert_eq!(draft.base_bytes, BASE);
    assert_eq!(
        state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        outbox
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md")).unwrap(),
        SENT
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_finish_rejects_changed_operation_payload_without_overwriting_it() {
    let rig = rig().await;
    let before = finish_snapshot(&rig).await;
    let draft_before = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .read_registered_checkpoint(&rig.old, &rig.document)
        .await
        .unwrap()
        .unwrap();
    let path = rig
        ._directory
        .path()
        .join("frozen-content-inspection.sqlite");
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let changed = b"synthetic corrupted sent payload";
    sqlx::query("UPDATE client_outbox SET bytes=? WHERE operation_id=?")
        .bind(changed.as_slice())
        .bind(&rig.sent.operation_id)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let result = finish_frozen_content_inspection(&rig.runtime, finish_request(&rig), before).await;
    assert!(matches!(result, Err(ref error) if error != FINISH_UNIMPLEMENTED));
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let after: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM client_outbox WHERE operation_id=?")
        .bind(&rig.sent.operation_id)
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(after.as_slice(), changed.as_slice());
    db.close().await.unwrap();
    assert_eq!(
        rig.runtime
            .0
            .lock()
            .await
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap(),
        draft_before
    );
    rig.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn frozen_content_finish_rejects_later_applied_remote_revision_without_mutation() {
    let rig = rig().await;
    let before = finish_snapshot(&rig).await;
    let old_before = {
        let mut state = rig.runtime.0.lock().await;
        (
            state
                .store
                .read_registered_checkpoint(&rig.old, &rig.document)
                .await
                .unwrap()
                .unwrap(),
            state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        )
    };
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: rig.server.local_addr(),
            admin_socket: rig._directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: rig._directory.path().join("notes"),
            state_root: rig._directory.path().join("server-state"),
            uploads: Default::default(),
        },
    };
    let pairing = server_test_support::pairing_request(&config).await.0;
    let peer = Session::pair(
        &format!("http://{}", rig.server.local_addr()),
        pairing.workspace_id,
        &pairing.pairing_secret,
    )
    .await
    .unwrap();
    let node = peer
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == rig.document)
        .unwrap();
    let new_remote = b"synthetic remote change after inspection\n";
    let receipt = peer
        .put_content(&ContentMutation {
            binding: peer.binding().clone(),
            operation_id: uuid::Uuid::now_v7().to_string().parse().unwrap(),
            operation_seq: 1,
            node_id: node.node_id,
            expected_revision: node.content_rev.unwrap(),
            bytes: new_remote.to_vec(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, OperationReceipt::Committed { .. }));
    let gate = rig.runtime.0.lock().await.gate.snapshot("owner").unwrap();
    crate::sync_catchup::catch_up_metadata(&rig.runtime, &gate, &rig.session)
        .await
        .unwrap();
    let index_before = rig
        .runtime
        .0
        .lock()
        .await
        .store
        .sync_index(&rig.current)
        .await
        .unwrap()
        .unwrap();
    let result = finish_frozen_content_inspection(&rig.runtime, finish_request(&rig), before).await;
    assert!(matches!(result, Err(ref error) if error != FINISH_UNIMPLEMENTED));
    let mut state = rig.runtime.0.lock().await;
    assert_eq!(
        state.store.sync_index(&rig.current).await.unwrap().unwrap(),
        index_before
    );
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&rig.old, &rig.document)
            .await
            .unwrap()
            .unwrap(),
        old_before.0
    );
    assert_eq!(
        state.store.list_outbox(&rig.old, 0, 100).await.unwrap(),
        old_before.1
    );
    assert!(
        state
            .store
            .list_outbox(&rig.current, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    drop(state);
    assert_eq!(
        std::fs::read(rig._directory.path().join("notes/move.md"))
            .unwrap()
            .as_slice(),
        new_remote.as_slice()
    );
    rig.server.shutdown().await.unwrap();
}
