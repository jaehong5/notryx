// Proposed API: list_frozen_creates(&current_binding, after_operation_id: Option<&str>, limit: u16)
// -> Result<FrozenCreatePage { entries: Vec<CreateOutboxState>, next_after_id: Option<String> }, StoreError>.
// The returned operation.binding is the original frozen binding; receipt is None for queued/sent.
use notryx_client_store::{
    ClientStore, CreateOutboxOperation, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::CreateNodeRequest;
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use tempfile::TempDir;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const NODES: [&str; 6] = [
    "019a1000-0000-7000-8000-000000000002",
    "019a1000-0000-7000-8000-000000000003",
    "019a1000-0000-7000-8000-000000000004",
    "019a1000-0000-7000-8000-000000000005",
    "019a1000-0000-7000-8000-000000000006",
    "019a1000-0000-7000-8000-000000000007",
];
fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}
fn replacement(old: &RegistrationBinding) -> RegistrationBinding {
    RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..old.clone()
    }
}
fn request(node: &str) -> CreateNodeRequest {
    serde_json::from_value(
        json!({"node_id":node,"parent_id":ROOT,"name":"same name","kind":"folder"}),
    )
    .unwrap()
}
async fn db(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}
async fn seed_root(path: &Path, b: &RegistrationBinding) {
    let mut conn = db(path).await;
    let registration:i64=sqlx::query_scalar("SELECT id FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?")
        .bind(&b.server_origin).bind(&b.workspace_id).bind(&b.workspace_epoch).bind(&b.device_id).bind(&b.device_stream_id).fetch_one(&mut conn).await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"meta","meta_etag":"\"meta\"","node_etag":"\"node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    sqlx::query("INSERT INTO remote_index VALUES (?,?,'1')")
        .bind(registration)
        .bind(ROOT)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO remote_nodes VALUES (?,?,?,0)")
        .bind(registration)
        .bind(ROOT)
        .bind(root.to_string())
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
}
async fn snapshot(path: &Path) -> Vec<String> {
    let mut conn = db(path).await;
    let rows=sqlx::query_scalar("SELECT 'reg:'||quote(id)||':'||quote(server_origin)||':'||quote(workspace_id)||':'||quote(workspace_epoch)||':'||quote(device_id)||':'||quote(device_stream_id)||':'||quote(status) FROM client_registrations UNION ALL SELECT 'out:'||quote(operation_id)||':'||quote(registration_id)||':'||quote(operation_seq)||':'||quote(operation_kind)||':'||quote(document_id)||':'||quote(dependency)||':'||hex(bytes)||':'||quote(payload_hash)||':'||quote(status)||':'||quote(abandon_state)||':'||quote(terminal_kind)||':'||quote(receipt_json) FROM client_outbox UNION ALL SELECT 'stream:'||quote(registration_id)||':'||quote(next_seq)||':'||quote(ack_through)||':'||quote(confirmed_through) FROM outbox_streams UNION ALL SELECT 'decision:'||quote(old_operation_id)||':'||quote(decision)||':'||quote(new_operation_id) FROM create_reconciliations ORDER BY 1").fetch_all(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    rows
}
struct Rig {
    _dir: TempDir,
    path: std::path::PathBuf,
    store: ClientStore,
    old: RegistrationBinding,
    other_origin: RegistrationBinding,
    current: RegistrationBinding,
    eligible: Vec<CreateOutboxOperation>,
    excluded: Vec<String>,
    active: CreateOutboxOperation,
}
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let old = binding();
    let current = replacement(&old);
    let mut store = ClientStore::open(&path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    seed_root(&path, &old).await;
    let first = store.queue_create(&old, &request(NODES[0])).await.unwrap();
    let PendingOutboxOperation::Create(sent) =
        store.prepare_pending_outbox(&old).await.unwrap().unwrap()
    else {
        panic!("create must be first")
    };
    assert_eq!(first.operation_id, sent.operation_id);
    assert_eq!(sent.status, OutboxStatus::Sent);
    let queued = store.queue_create(&old, &request(NODES[1])).await.unwrap();
    let decided_binding = replacement(&old);
    store.activate_registration(&decided_binding).await.unwrap();
    seed_root(&path, &decided_binding).await;
    let decided = store
        .queue_create(&decided_binding, &request(NODES[2]))
        .await
        .unwrap();
    let other = binding();
    store.activate_registration(&other).await.unwrap();
    seed_root(&path, &other).await;
    let foreign = store
        .queue_create(&other, &request(NODES[3]))
        .await
        .unwrap();
    let mut other_origin = replacement(&old);
    other_origin.server_origin = "https://different.example".into();
    store.activate_registration(&other_origin).await.unwrap();
    seed_root(&path, &other_origin).await;
    let foreign_origin = store
        .queue_create(&other_origin, &request(NODES[4]))
        .await
        .unwrap();
    store.activate_registration(&current).await.unwrap();
    seed_root(&path, &current).await;
    let active = store
        .queue_create(&current, &request(NODES[5]))
        .await
        .unwrap();
    let mut conn = db(&path).await;
    sqlx::query("INSERT INTO create_reconciliations(old_operation_id,decision,new_operation_id) VALUES (?,'dismissed',NULL)").bind(&decided.operation_id).execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    Rig {
        _dir: dir,
        path,
        store,
        old,
        other_origin,
        current,
        eligible: vec![sent, queued, foreign_origin],
        excluded: vec![decided.operation_id, foreign.operation_id],
        active,
    }
}

#[tokio::test]
async fn frozen_list_filters_scopes_and_decisions_and_pages_without_mutation() {
    let mut rig = rig().await;
    let before = snapshot(&rig.path).await;
    assert!(matches!(
        rig.store.list_frozen_creates(&rig.old, None, 100).await,
        Err(StoreError::StaleRegistration)
    ));
    for limit in [0, 101] {
        assert!(matches!(
            rig.store
                .list_frozen_creates(&rig.current, None, limit)
                .await,
            Err(StoreError::InvalidRequest)
        ));
    }
    let mut expected = rig.eligible.clone();
    expected.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
    let mut after: Option<String> = None;
    let mut actual = Vec::new();
    for (index, expected_entry) in expected.iter().enumerate() {
        let page = rig
            .store
            .list_frozen_creates(&rig.current, after.as_deref(), 1)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        let entry = &page.entries[0];
        assert_eq!(&entry.operation, expected_entry);
        assert!(entry.frozen);
        assert!(entry.receipt.is_none());
        assert!(!rig.excluded.contains(&entry.operation.operation_id));
        if index + 1 < expected.len() {
            assert_eq!(
                page.next_after_id.as_deref(),
                Some(expected_entry.operation_id.as_str())
            );
        } else {
            assert_eq!(page.next_after_id, None);
        }
        actual.push(entry.operation.clone());
        after = page.next_after_id;
    }
    let from_other_origin = actual
        .iter()
        .find(|entry| entry.binding == rig.other_origin)
        .expect("same workspace from another origin must remain visible");
    assert_eq!(
        from_other_origin.binding.workspace_id,
        rig.current.workspace_id
    );
    assert_ne!(
        from_other_origin.binding.server_origin,
        rig.current.server_origin
    );
    assert_eq!(from_other_origin.request, request(NODES[4]));
    assert!(actual.iter().any(|entry| entry.binding == rig.old));
    assert_eq!(
        rig.store.next_pending_outbox(&rig.current).await.unwrap(),
        Some(PendingOutboxOperation::Create(rig.active.clone()))
    );
    assert_eq!(snapshot(&rig.path).await, before);
    rig.store.close().await.unwrap();
    let mut store = ClientStore::open(&rig.path).await.unwrap();
    let page = store
        .list_frozen_creates(&rig.current, None, 100)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 3);
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| entry.operation.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        store.next_pending_outbox(&rig.current).await.unwrap(),
        Some(PendingOutboxOperation::Create(rig.active))
    );
    assert_eq!(snapshot(&rig.path).await, before);
    store.close().await.unwrap();
}

#[tokio::test]
async fn corrupt_frozen_row_is_rejected_without_mutation_or_false_status() {
    for variant in 0..4 {
        let mut rig = rig().await;
        let target = &rig.eligible[0].operation_id;
        let mut conn = db(&rig.path).await;
        match variant {
            0 => {
                sqlx::query(
                    "UPDATE client_outbox SET payload_hash='sha256:wrong' WHERE operation_id=?",
                )
                .bind(target)
                .execute(&mut conn)
                .await
                .unwrap();
            }
            1 => {
                sqlx::query("UPDATE client_outbox SET receipt_json='{}' WHERE operation_id=?")
                    .bind(target)
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            2 => {
                sqlx::query("UPDATE client_outbox SET document_id=? WHERE operation_id=?")
                    .bind(NODES[5])
                    .bind(target)
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            _ => {
                sqlx::query(
                    "UPDATE client_registrations SET workspace_epoch='invalid' WHERE device_id=?",
                )
                .bind(&rig.old.device_id)
                .execute(&mut conn)
                .await
                .unwrap();
            }
        }
        conn.close().await.unwrap();
        let before = snapshot(&rig.path).await;
        assert!(matches!(
            rig.store.list_frozen_creates(&rig.current, None, 100).await,
            Err(StoreError::UnsupportedFormat)
        ));
        assert_eq!(
            rig.store.next_pending_outbox(&rig.current).await.unwrap(),
            Some(PendingOutboxOperation::Create(rig.active))
        );
        assert_eq!(snapshot(&rig.path).await, before);
        rig.store.close().await.unwrap();
    }
}
