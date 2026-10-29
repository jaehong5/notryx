// Proposed store API:
//   enum FrozenCreateChoice { Apply { parent_id: NodeId, name: String }, Dismiss }
//   ClientStore::resolve_frozen_create(&old_binding, &current_binding, old_operation_id, &choice)
//     -> Result<..., StoreError>
// The return value is deliberately not assumed; the observable contract is read
// from SQLite and the existing list_create_operations/next_pending_outbox APIs.
// Proposed decision table name: create_reconciliations (old_operation_id PRIMARY KEY).
use notryx_client_store::{
    ClientStore, FrozenCreateChoice, OutboxStatus, PendingOutboxOperation, RegistrationBinding,
    StoreError,
};
use notryx_protocol::CreateNodeRequest;
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const PARENT: &str = "019a1000-0000-7000-8000-000000000002";
const OLD_NODE: &str = "019a1000-0000-7000-8000-000000000003";
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
fn old_request() -> CreateNodeRequest {
    serde_json::from_value(
        json!({"node_id":OLD_NODE,"parent_id":ROOT,"name":"original name","kind":"folder"}),
    )
    .unwrap()
}
fn apply(name: &str) -> FrozenCreateChoice {
    FrozenCreateChoice::Apply {
        parent_id: PARENT.parse().unwrap(),
        name: name.into(),
    }
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
async fn seed_nodes(path: &Path, b: &RegistrationBinding) {
    let mut conn = db(path).await;
    let id:i64=sqlx::query_scalar("SELECT id FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?").bind(&b.server_origin).bind(&b.workspace_id).bind(&b.workspace_epoch).bind(&b.device_id).bind(&b.device_stream_id).fetch_one(&mut conn).await.unwrap();
    let root = json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"1","last_change_seq":"1"});
    let parent = json!({"node_id":PARENT,"parent_id":ROOT,"name":"target","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"parent-meta","meta_etag":"\"parent-meta\"","node_etag":"\"parent-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"});
    sqlx::query("INSERT INTO remote_index VALUES (?,?,'1')")
        .bind(id)
        .bind(ROOT)
        .execute(&mut conn)
        .await
        .unwrap();
    for (node, state) in [(ROOT, root), (PARENT, parent)] {
        sqlx::query("INSERT INTO remote_nodes VALUES (?,?,?,0)")
            .bind(id)
            .bind(node)
            .bind(state.to_string())
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn.close().await.unwrap();
}
async fn old_row(
    path: &Path,
    op: &str,
) -> (
    String,
    i64,
    String,
    String,
    Vec<u8>,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
) {
    let mut conn = db(path).await;
    let row=sqlx::query_as("SELECT r.workspace_id,o.operation_seq,r.device_id,r.device_stream_id,o.bytes,o.payload_hash,o.receipt_json,o.dependency,o.status,o.abandon_state,o.terminal_kind FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE o.operation_id=?").bind(op).fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    row
}
async fn counts(path: &Path, b: &RegistrationBinding) -> (i64, i64, Option<i64>) {
    let mut conn = db(path).await;
    let old_decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM create_reconciliations")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    let (items,next):(i64,Option<i64>)=sqlx::query_as("SELECT (SELECT COUNT(*) FROM client_outbox o WHERE o.registration_id=r.id),(SELECT s.next_seq FROM outbox_streams s WHERE s.registration_id=r.id) FROM client_registrations r WHERE r.device_id=? AND r.device_stream_id=?").bind(&b.device_id).bind(&b.device_stream_id).fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    (old_decisions, items, next)
}
async fn setup(
    path: &Path,
    sent: bool,
) -> (
    ClientStore,
    RegistrationBinding,
    RegistrationBinding,
    String,
) {
    let old = binding();
    let current = replacement(&old);
    let mut store = ClientStore::open(path).await.unwrap();
    store.activate_registration(&old).await.unwrap();
    seed_nodes(path, &old).await;
    let op = store.queue_create(&old, &old_request()).await.unwrap();
    let saved = old_row(path, &op.operation_id).await;
    assert_eq!(saved.7, None); // Normal create has no parent-create dependency.
    if sent {
        let PendingOutboxOperation::Create(item) =
            store.prepare_pending_outbox(&old).await.unwrap().unwrap()
        else {
            panic!("expected create")
        };
        assert_eq!(item.status, OutboxStatus::Sent);
    }
    store.activate_registration(&current).await.unwrap();
    seed_nodes(path, &current).await;
    (store, old, current, op.operation_id)
}

#[tokio::test]
async fn apply_creates_one_new_durable_operation_and_preserves_original() {
    for sent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, old, current, id) = setup(&path, sent).await;
        let before = old_row(&path, &id).await;
        store
            .resolve_frozen_create(&old, &current, &id, &apply("chosen name"))
            .await
            .unwrap();
        let PendingOutboxOperation::Create(new) =
            store.next_pending_outbox(&current).await.unwrap().unwrap()
        else {
            panic!("new create required")
        };
        assert_eq!(new.operation_seq, 1);
        assert_ne!(new.operation_id, id);
        assert_ne!(new.request.node_id.to_string(), OLD_NODE);
        assert_eq!(
            Uuid::parse_str(&new.operation_id)
                .unwrap()
                .get_version_num(),
            7
        );
        assert_eq!(
            Uuid::parse_str(&new.request.node_id.to_string())
                .unwrap()
                .get_version_num(),
            7
        );
        assert_eq!(new.request.parent_id.to_string(), PARENT);
        assert_eq!(new.request.name, "chosen name");
        assert_eq!(new.binding, current);
        assert_eq!(old_row(&path, &id).await, before);
        assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
        store
            .resolve_frozen_create(&old, &current, &id, &apply("chosen name"))
            .await
            .unwrap();
        assert_eq!(
            store.next_pending_outbox(&current).await.unwrap(),
            Some(PendingOutboxOperation::Create(new.clone()))
        );
        assert!(
            store
                .resolve_frozen_create(&old, &current, &id, &apply("different name"))
                .await
                .is_err()
        );
        assert!(
            store
                .resolve_frozen_create(&old, &current, &id, &FrozenCreateChoice::Dismiss)
                .await
                .is_err()
        );
        assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        store
            .resolve_frozen_create(&old, &current, &id, &apply("chosen name"))
            .await
            .unwrap();
        assert_eq!(
            store.next_pending_outbox(&current).await.unwrap(),
            Some(PendingOutboxOperation::Create(new))
        );
        assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
        assert_eq!(old_row(&path, &id).await, before);
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn dismiss_is_explicit_idempotent_and_keeps_old_payload_without_new_mutation() {
    for sent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, old, current, id) = setup(&path, sent).await;
        let before = old_row(&path, &id).await;
        store
            .resolve_frozen_create(&old, &current, &id, &FrozenCreateChoice::Dismiss)
            .await
            .unwrap();
        assert_eq!(counts(&path, &current).await, (1, 0, None));
        assert_eq!(old_row(&path, &id).await, before);
        assert!(store.next_pending_outbox(&current).await.unwrap().is_none());
        store
            .resolve_frozen_create(&old, &current, &id, &FrozenCreateChoice::Dismiss)
            .await
            .unwrap();
        assert!(
            store
                .resolve_frozen_create(&old, &current, &id, &apply("chosen name"))
                .await
                .is_err()
        );
        store.close().await.unwrap();
        let mut store = ClientStore::open(&path).await.unwrap();
        store
            .resolve_frozen_create(&old, &current, &id, &FrozenCreateChoice::Dismiss)
            .await
            .unwrap();
        assert_eq!(old_row(&path, &id).await, before);
        assert_eq!(counts(&path, &current).await, (1, 0, None));
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn failed_decision_insert_rolls_back_new_operation_and_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let (mut store, old, current, id) = setup(&path, true).await;
    let before = old_row(&path, &id).await;
    let mut conn = db(&path).await;
    sqlx::raw_sql("CREATE TRIGGER fail_decision BEFORE INSERT ON create_reconciliations BEGIN SELECT RAISE(ABORT,'injected'); END;").execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    assert!(matches!(
        store
            .resolve_frozen_create(&old, &current, &id, &apply("chosen name"))
            .await,
        Err(StoreError::Storage)
    ));
    assert_eq!(counts(&path, &current).await, (0, 0, None));
    assert_eq!(old_row(&path, &id).await, before);
    let mut conn = db(&path).await;
    sqlx::raw_sql("DROP TRIGGER fail_decision")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    store
        .resolve_frozen_create(&old, &current, &id, &apply("chosen name"))
        .await
        .unwrap();
    assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
    store.close().await.unwrap();
}

#[tokio::test]
async fn other_workspace_and_removed_parent_do_not_consume_the_decision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let (mut store, old, current, id) = setup(&path, true).await;
    let before = old_row(&path, &id).await;
    let mut unrelated = current.clone();
    unrelated.workspace_id = Uuid::new_v4().to_string();
    assert!(
        store
            .resolve_frozen_create(&old, &unrelated, &id, &apply("name"))
            .await
            .is_err()
    );
    let mut conn = db(&path).await;
    sqlx::query("UPDATE remote_nodes SET state=json_set(state,'$.deleted',json('true')) WHERE node_id=? AND registration_id=(SELECT id FROM client_registrations WHERE status='active')").bind(PARENT).execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    assert!(
        store
            .resolve_frozen_create(&old, &current, &id, &apply("name"))
            .await
            .is_err()
    );
    assert_eq!(counts(&path, &current).await, (0, 0, None));
    assert_eq!(old_row(&path, &id).await, before);
    store.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_existing_dependency_is_preserved_and_prevents_resolution() {
    for choice in [apply("name"), FrozenCreateChoice::Dismiss] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, old, current, id) = setup(&path, true).await;
        let before = old_row(&path, &id).await;
        let editor = store
            .open_registered_document(&current, "independent", b"base", "rev-a")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(
                &current,
                "independent",
                &editor.edit_session_id,
                1,
                b"draft",
            )
            .await
            .unwrap();
        let dependent = store
            .queue_checkpoint(&current, "independent", 1)
            .await
            .unwrap();
        let mut conn = db(&path).await;
        sqlx::query("UPDATE client_outbox SET dependency=? WHERE operation_id=?")
            .bind(&id)
            .bind(&dependent.operation_id)
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        assert!(
            store
                .resolve_frozen_create(&old, &current, &id, &choice)
                .await
                .is_err()
        );
        let mut conn = db(&path).await;
        let recorded: Option<String> =
            sqlx::query_scalar("SELECT dependency FROM client_outbox WHERE operation_id=?")
                .bind(&dependent.operation_id)
                .fetch_one(&mut conn)
                .await
                .unwrap();
        conn.close().await.unwrap();
        assert_eq!(recorded.as_deref(), Some(id.as_str()));
        assert_eq!(old_row(&path, &id).await, before);
        assert_eq!(counts(&path, &current).await, (0, 1, Some(2)));
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn active_different_workspace_cannot_apply_or_dismiss_original_workspace_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let (mut store, old, _same_workspace, id) = setup(&path, true).await;
    let before = old_row(&path, &id).await;
    let other = binding();
    assert_ne!(old.workspace_id, other.workspace_id);
    store.activate_registration(&other).await.unwrap();
    seed_nodes(&path, &other).await;
    assert!(
        store
            .resolve_frozen_create(&old, &other, &id, &apply("name"))
            .await
            .is_err()
    );
    assert!(
        store
            .resolve_frozen_create(&old, &other, &id, &FrozenCreateChoice::Dismiss)
            .await
            .is_err()
    );
    assert_eq!(counts(&path, &other).await, (0, 0, None));
    assert_eq!(old_row(&path, &id).await, before);
    store.close().await.unwrap();
}
// Append to crates/client-store/tests/frozen_create_decision.rs.
// Reuses setup, db, old_row, counts and apply; no network or non-synthetic data.
#[tokio::test]
async fn corrupted_frozen_create_receipt_never_consumes_a_decision() {
    for decision in [apply("chosen name"), FrozenCreateChoice::Dismiss] {
        for variant in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("store.db");
            let (mut store, old, current, id) = setup(&path, true).await;
            let good = json!({
                "status": "destination_exists", "operation_id": id,
                "operation_seq": "1", "workspace_epoch": old.workspace_epoch,
            });
            let mut wrong_id = good.clone();
            wrong_id["operation_id"] = json!("019a1000-0000-7000-8000-000000000099");
            let mut wrong_epoch = good.clone();
            wrong_epoch["workspace_epoch"] = json!(Uuid::new_v4().to_string());
            let (state, terminal, raw) = match variant {
                0 => ("conflict", Some("destination_exists"), None),
                1 => (
                    "conflict",
                    Some("destination_exists"),
                    Some(wrong_id.to_string()),
                ),
                2 => (
                    "conflict",
                    Some("destination_exists"),
                    Some(wrong_epoch.to_string()),
                ),
                3 => ("conflict", Some("node_id_conflict"), Some(good.to_string())),
                4 => ("sent", None, Some(good.to_string())),
                _ => unreachable!(),
            };
            let mut conn = db(&path).await;
            sqlx::query("UPDATE client_outbox SET status=?,terminal_kind=?,receipt_json=? WHERE operation_id=?")
                .bind(state).bind(terminal).bind(raw).bind(&id).execute(&mut conn).await.unwrap();
            conn.close().await.unwrap();
            let before = old_row(&path, &id).await;
            assert!(
                store
                    .resolve_frozen_create(&old, &current, &id, &decision)
                    .await
                    .is_err(),
                "invalid stored receipt accepted: decision={decision:?}, variant={variant}"
            );
            assert_eq!(old_row(&path, &id).await, before);
            assert_eq!(counts(&path, &current).await, (0, 0, None));
            store.close().await.unwrap();
        }
    }
}
// Append to crates/client-store/tests/frozen_create_decision.rs.
#[tokio::test]
async fn repeating_decision_rejects_corruption_of_the_chosen_new_operation() {
    use sha2::{Digest, Sha256};
    for variant in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, old, current, id) = setup(&path, true).await;
        let decision = apply("chosen name");
        store
            .resolve_frozen_create(&old, &current, &id, &decision)
            .await
            .unwrap();
        let PendingOutboxOperation::Create(new) =
            store.next_pending_outbox(&current).await.unwrap().unwrap()
        else {
            panic!("create")
        };
        let mut conn = db(&path).await;
        match variant {
            0 => {
                sqlx::query(
                    "UPDATE client_outbox SET payload_hash='sha256:invalid' WHERE operation_id=?",
                )
                .bind(&new.operation_id)
                .execute(&mut conn)
                .await
                .unwrap();
            }
            1 => {
                sqlx::query("UPDATE client_outbox SET document_id=? WHERE operation_id=?")
                    .bind(OLD_NODE)
                    .bind(&new.operation_id)
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            2 => {
                let raw: Vec<u8> =
                    sqlx::query_scalar("SELECT bytes FROM client_outbox WHERE operation_id=?")
                        .bind(&new.operation_id)
                        .fetch_one(&mut conn)
                        .await
                        .unwrap();
                let mut request: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                request["node_id"] = json!(OLD_NODE);
                let payload = serde_json::to_vec(&request).unwrap();
                let hash = format!(
                    "sha256:{}",
                    Sha256::digest(&payload)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                );
                sqlx::query("UPDATE client_outbox SET document_id=?,bytes=?,payload_hash=?,payload_len=? WHERE operation_id=?")
                    .bind(OLD_NODE).bind(&payload).bind(hash).bind(payload.len() as i64).bind(&new.operation_id).execute(&mut conn).await.unwrap();
            }
            3 => {
                sqlx::query(
                    "UPDATE client_outbox SET payload_len=payload_len+1 WHERE operation_id=?",
                )
                .bind(&new.operation_id)
                .execute(&mut conn)
                .await
                .unwrap();
            }
            _ => unreachable!(),
        }
        conn.close().await.unwrap();
        let old_before = old_row(&path, &id).await;
        let new_before = old_row(&path, &new.operation_id).await;
        assert!(
            store
                .resolve_frozen_create(&old, &current, &id, &decision)
                .await
                .is_err(),
            "corrupt new create accepted on same decision: variant={variant}"
        );
        assert_eq!(old_row(&path, &id).await, old_before);
        assert_eq!(old_row(&path, &new.operation_id).await, new_before);
        assert_eq!(counts(&path, &current).await, (1, 1, Some(2)));
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn implicit_parent_reference_blocks_both_choices_without_mutating_children() {
    use sha2::{Digest, Sha256};
    for choice in [apply("chosen name"), FrozenCreateChoice::Dismiss] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let (mut store, old, current, id) = setup(&path, true).await;
        let original = old_row(&path, &id).await;
        let child_op = "019a1000-0000-7000-8000-000000000070";
        let child_node = "019a1000-0000-7000-8000-000000000071";
        let bytes = serde_json::to_vec(
            &json!({"node_id":child_node,"parent_id":OLD_NODE,"name":"child","kind":"folder"}),
        )
        .unwrap();
        let digest = Sha256::digest(&bytes);
        let hash = format!(
            "sha256:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let mut conn = db(&path).await;
        let mut tx = conn.begin().await.unwrap();
        let old_reg: i64 =
            sqlx::query_scalar("SELECT registration_id FROM client_outbox WHERE operation_id=?")
                .bind(&id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        let next: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(old_reg)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,bytes,payload_hash,status,payload_len,dependency) VALUES (?,?,?,'create',?,?,?,'queued',?,NULL)")
            .bind(child_op).bind(old_reg).bind(next).bind(child_node).bind(&bytes).bind(&hash).bind(bytes.len() as i64).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE outbox_streams SET next_seq=? WHERE registration_id=?")
            .bind(next + 1)
            .bind(old_reg)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        conn.close().await.unwrap();
        let before_child = old_row(&path, child_op).await;
        assert_eq!(before_child.7, None);
        let before_current = counts(&path, &current).await;
        assert!(matches!(
            store
                .resolve_frozen_create(&old, &current, &id, &choice)
                .await,
            Err(StoreError::ReconciliationRequired)
        ));
        assert_eq!(old_row(&path, &id).await, original);
        assert_eq!(old_row(&path, child_op).await, before_child);
        assert_eq!(counts(&path, &current).await, before_current);
        let mut conn = db(&path).await;
        let persisted_next: i64 =
            sqlx::query_scalar("SELECT next_seq FROM outbox_streams WHERE registration_id=?")
                .bind(old_reg)
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(persisted_next, next + 1);
        conn.close().await.unwrap();
        store.close().await.unwrap();
    }
}
