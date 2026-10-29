use super::{ForegroundSignal, recover_resync};
use crate::{
    Runtime, RuntimeState, registration_binding,
    send::TransmissionBlock,
    sync_bootstrap::{bootstrap_metadata, server_test_support, tests::setup},
    sync_catchup::{CatchUpError, catch_up},
};
use notryx_client_runtime::{Error as ClientError, Session};
use notryx_client_store::{ClientStore, RegistrationBinding};
use notryx_server::{
    Workspace,
    config::{Config, ServerConfig, WorkspaceConfig},
    http::RunningServer,
};
use std::sync::Arc;
use tokio::sync::Mutex;

const TEST_NOW: i64 = 1_800_000_000;
const RETENTION_SECONDS: u64 = 30 * 24 * 60 * 60;

async fn preserved_sources(
    runtime: &Runtime,
    binding: &RegistrationBinding,
    document: &str,
) -> (
    notryx_client_store::PersistedDraft,
    notryx_client_store::OutboxPage,
    notryx_client_store::OutboxOperation,
) {
    let mut state = runtime.0.lock().await;
    let draft = state
        .store
        .read_registered_checkpoint(binding, document)
        .await
        .unwrap()
        .unwrap();
    let outbox = state.store.list_outbox(binding, 0, 100).await.unwrap();
    assert_eq!(outbox.entries.len(), 1);
    let operation = state
        .store
        .read_outbox_operation(binding, &outbox.entries[0].operation_id)
        .await
        .unwrap()
        .unwrap();
    (draft, outbox, operation)
}

#[tokio::test]
async fn real_retention_floor_rebootstraps_two_independent_client_stores_without_losing_drafts() {
    let (directory, server, runtime_a, session_a, document) = setup().await;
    let address = server.local_addr();
    let config = Config {
        sync: Default::default(),
        server: ServerConfig {
            listen: address,
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("server-state"),
            uploads: Default::default(),
        },
    };
    let pairing = server_test_support::pairing_request(&config).await.0;
    let session_b = Arc::new(
        Session::pair(
            &format!("http://{address}"),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap(),
    );
    let binding_a = registration_binding(session_a.binding());
    let binding_b = registration_binding(session_b.binding());
    assert_ne!(binding_a.device_stream_id, binding_b.device_stream_id);
    let node = session_b
        .inventory(None)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.node_id.to_string() == document)
        .unwrap();
    let mut store_b = ClientStore::open(directory.path().join("retention-client-b.sqlite"))
        .await
        .unwrap();
    store_b.activate_registration(&binding_b).await.unwrap();
    let opened_b = store_b
        .open_registered_document(
            &binding_b,
            &document,
            b"server source\n",
            node.content_rev.as_ref().unwrap().as_str(),
        )
        .await
        .unwrap();
    store_b
        .save_registered_checkpoint(
            &binding_b,
            &document,
            &opened_b.edit_session_id,
            1,
            b"synthetic second client draft\n",
        )
        .await
        .unwrap();
    store_b
        .close_registered_document(&binding_b, &document, &opened_b.edit_session_id)
        .await
        .unwrap();
    store_b
        .queue_checkpoint(&binding_b, &document, 1)
        .await
        .unwrap();
    let mut state_b = RuntimeState::new(store_b, "owner-b".into());
    state_b.session = Some(session_b.clone());
    let runtime_b = Runtime(Arc::new(Mutex::new(state_b)));

    let gate_a = runtime_a.0.lock().await.gate.snapshot("owner").unwrap();
    let gate_b = runtime_b.0.lock().await.gate.snapshot("owner-b").unwrap();
    let cursor_a = bootstrap_metadata(&runtime_a, &gate_a, &session_a)
        .await
        .unwrap();
    let cursor_b = bootstrap_metadata(&runtime_b, &gate_b, &session_b)
        .await
        .unwrap();
    assert_eq!(cursor_a, cursor_b);
    let original_info = session_a.server_info().await.unwrap();
    let before_a = preserved_sources(&runtime_a, &binding_a, &document).await;
    let before_b = preserved_sources(&runtime_b, &binding_b, &document).await;
    assert_ne!(before_a.0.bytes, before_b.0.bytes);

    server.shutdown().await.unwrap();
    let mut workspace = Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
        .await
        .unwrap();
    std::fs::write(
        config.workspace.notes_root.join("retention-synthetic.md"),
        b"# synthetic\n",
    )
    .unwrap();
    workspace
        .adopt_root_document("retention-synthetic.md")
        .await
        .unwrap();
    workspace
        .retain_journal(RETENTION_SECONDS, 0, TEST_NOW)
        .await
        .unwrap();
    let advanced = workspace.server_info().await.unwrap();
    assert_eq!(advanced.workspace_epoch, original_info.workspace_epoch);
    assert!(advanced.journal_floor_seq > cursor_a);
    assert!(advanced.head_seq > cursor_a);
    workspace.close().await.unwrap();
    let restarted = RunningServer::start(config.clone()).await.unwrap();
    assert_eq!(restarted.local_addr(), address);

    for (runtime, session, binding, owner, cursor, before) in [
        (
            &runtime_a, &session_a, &binding_a, "owner", cursor_a, &before_a,
        ),
        (
            &runtime_b, &session_b, &binding_b, "owner-b", cursor_b, &before_b,
        ),
    ] {
        let info = session.server_info().await.unwrap();
        assert_eq!(info.workspace_epoch, original_info.workspace_epoch);
        assert_eq!(info.journal_floor_seq, advanced.journal_floor_seq);
        assert!(matches!(
            session.changes(cursor, Some(info.head_seq)).await,
            Err(ClientError::ResyncRequired)
        ));
        let gate = runtime.0.lock().await.gate.snapshot(owner).unwrap();
        assert!(matches!(
            catch_up(runtime, Some(&gate), session).await,
            Err(CatchUpError::ResyncRequired)
        ));
        assert!(
            runtime
                .0
                .lock()
                .await
                .transmission_gate
                .has_block(binding, TransmissionBlock::ResyncRequired)
        );
        assert!(runtime.0.lock().await.transmission_gate.blocked(binding));
        let blocked = preserved_sources(runtime, binding, &document).await;
        assert_eq!(&blocked.0, &before.0);
        assert_eq!(&blocked.1, &before.1);
        assert!(blocked.2 == before.2);
        let (recovered, events, _) = recover_resync(runtime, session, &ForegroundSignal::new())
            .await
            .unwrap();
        drop(events);
        assert_eq!(recovered, info.head_seq);
        let mut state = runtime.0.lock().await;
        assert!(
            !state
                .transmission_gate
                .has_block(binding, TransmissionBlock::ResyncRequired)
        );
        let (index_cursor, mut nodes) = state.store.sync_index(binding).await.unwrap().unwrap();
        assert_eq!(index_cursor, info.head_seq);
        assert!(
            nodes
                .iter()
                .any(|node| node.name == "retention-synthetic.md" && !node.deleted)
        );
        drop(state);
        let mut inventory = session.inventory(None).await.unwrap().nodes;
        nodes.sort_by_key(|node| node.node_id.to_string());
        inventory.sort_by_key(|node| node.node_id.to_string());
        assert_eq!(nodes, inventory);
    }
    let after_a = preserved_sources(&runtime_a, &binding_a, &document).await;
    let after_b = preserved_sources(&runtime_b, &binding_b, &document).await;
    assert_eq!(before_a.0, after_a.0);
    assert_eq!(before_a.1, after_a.1);
    assert!(before_a.2 == after_a.2);
    assert_eq!(before_b.0, after_b.0);
    assert_eq!(before_b.1, after_b.1);
    assert!(before_b.2 == after_b.2);
    assert_eq!(
        std::fs::read(config.workspace.notes_root.join("example.md")).unwrap(),
        b"server source\n"
    );
    restarted.shutdown().await.unwrap();
}
