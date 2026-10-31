use notryx_client_store::{
    ClientStore, OutboxStatus, PendingOutboxOperation, RegistrationBinding, StoreError,
};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const RESTORE_ID: &str = "019a1000-0000-7000-8000-000000000003";
const RESTORE_OP: &str = "019a1000-0000-7000-8000-000000000004";
const RESTORE: &[u8] = br#"{"expected_meta_rev":"meta-1","parent_id":"019a1000-0000-7000-8000-000000000001","name":"restored.md"}"#;

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: "00000000-0000-4000-8000-000000000001".into(),
        workspace_epoch: "00000000-0000-4000-8000-000000000002".into(),
        device_id: "00000000-0000-4000-8000-000000000003".into(),
        device_stream_id: "00000000-0000-4000-8000-000000000004".into(),
    }
}
fn hash(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

// Independent v9 schema fixture; do not construct it by opening the current ClientStore.
async fn v9_fixture(path: &Path) {
    let mut db = connect(path).await;
    sqlx::raw_sql(include_str!("fixtures/client_store_v7.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql(r#"
        PRAGMA defer_foreign_keys=ON;
        CREATE TABLE client_outbox_v8 (
            operation_id TEXT PRIMARY KEY NOT NULL, registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            operation_seq INTEGER NOT NULL CHECK(operation_seq>0), operation_kind TEXT NOT NULL DEFAULT 'content' CHECK(operation_kind IN ('content','restore')),
            document_id TEXT NOT NULL, storage_id TEXT REFERENCES registered_documents(storage_id), generation INTEGER,
            expected_revision TEXT, dependency TEXT REFERENCES client_outbox_v8(operation_id),
            bytes BLOB NOT NULL CHECK(length(bytes)<=5242880), payload_hash TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('queued','sent','committed','conflict')),
            committed_revision TEXT, conflict_revision TEXT, conflict_etag TEXT,
            abandon_state TEXT NOT NULL DEFAULT 'none' CHECK(abandon_state IN ('none','requested','sent')),
            terminal_kind TEXT CHECK(terminal_kind IN ('abandoned','node_deleted','recovery_conflict','not_found','not_applied','not_deleted','history_expired','metadata_precondition_failed','destination_exists','parent_not_folder')),
            payload_retired INTEGER NOT NULL DEFAULT 0 CHECK(payload_retired IN (0,1)),
            payload_len INTEGER NOT NULL DEFAULT 0 CHECK(payload_len>=0 AND payload_len<=5242880),
            receipt_json TEXT CHECK(receipt_json IS NULL OR length(receipt_json)<=65536),
            CHECK((operation_kind='content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation>0) OR (operation_kind='restore' AND storage_id IS NULL AND generation IS NULL)),
            UNIQUE(registration_id,operation_seq)
        );
        DROP TABLE client_outbox;
        ALTER TABLE client_outbox_v8 RENAME TO client_outbox;
        CREATE INDEX outbox_document_recent ON client_outbox(registration_id,document_id,operation_seq DESC);
        CREATE INDEX outbox_document_status ON client_outbox(registration_id,document_id,status,terminal_kind);
        CREATE INDEX outbox_dependencies ON client_outbox(registration_id,dependency,status);
        CREATE TABLE credential_pending (singleton INTEGER PRIMARY KEY CHECK(singleton=1), item_id TEXT NOT NULL UNIQUE, server_origin TEXT NOT NULL, workspace_id TEXT NOT NULL, workspace_epoch TEXT NOT NULL, device_id TEXT NOT NULL, device_stream_id TEXT NOT NULL, previous_registration_id INTEGER REFERENCES client_registrations(id));
        CREATE TABLE credential_links (registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id), item_id TEXT UNIQUE);
        PRAGMA user_version=9;
    "#).execute(&mut db).await.unwrap();
    let b = binding();
    sqlx::query("INSERT INTO client_registrations VALUES (1,?,?,?,?,?,'active')")
        .bind(&b.server_origin)
        .bind(&b.workspace_id)
        .bind(&b.workspace_epoch)
        .bind(&b.device_id)
        .bind(&b.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO credential_links VALUES (1,'00000000-0000-4000-8000-000000000005'); INSERT INTO registered_documents VALUES (1,'doc','storage'); INSERT INTO draft_checkpoints VALUES ('storage',3,1,X'6e65776572206472616674'); INSERT INTO draft_bases VALUES ('storage',X'62617365','rev-a'); INSERT INTO outbox_streams VALUES (1,5,1,1);").execute(&mut db).await.unwrap();
    for (id, seq, bytes, status, revision, dependency) in [
        ("op-1", 1, &b"base"[..], "committed", Some("rev-a"), None),
        ("op-2", 2, &b"in flight"[..], "sent", Some("rev-a"), None),
        ("op-4", 4, &b"newer draft"[..], "queued", None, Some("op-2")),
    ] {
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,document_id,storage_id,generation,expected_revision,dependency,bytes,payload_hash,status,committed_revision,payload_len) VALUES (?,1,?,'doc','storage',?,?,?,?,?,?,?,?)")
            .bind(id).bind(seq).bind(seq).bind(revision).bind(dependency).bind(bytes).bind(hash(bytes)).bind(status)
            .bind(if status=="committed" {Some("rev-a")} else {None}).bind(bytes.len() as i64).execute(&mut db).await.unwrap();
    }
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,bytes,payload_hash,status,payload_len) VALUES (?,1,3,'restore',?,?,?,'queued',?)")
        .bind(RESTORE_OP).bind(RESTORE_ID).bind(RESTORE).bind(hash(RESTORE)).bind(RESTORE.len() as i64).execute(&mut db).await.unwrap();
    let mut frozen = b.clone();
    frozen.device_id = "00000000-0000-4000-8000-000000000008".into();
    frozen.device_stream_id = "00000000-0000-4000-8000-000000000009".into();
    sqlx::query("INSERT INTO client_registrations VALUES (2,?,?,?,?,?,'frozen')")
        .bind(&frozen.server_origin)
        .bind(&frozen.workspace_id)
        .bind(&frozen.workspace_epoch)
        .bind(&frozen.device_id)
        .bind(&frozen.device_stream_id)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox_streams VALUES (2,2,1,0)")
        .execute(&mut db)
        .await
        .unwrap();
    let receipt = serde_json::json!({"status":"history_expired","operation_id":"019a1000-0000-7000-8000-000000000005","operation_seq":"1","workspace_epoch":frozen.workspace_epoch}).to_string();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,bytes,payload_hash,status,terminal_kind,payload_len,receipt_json) VALUES ('019a1000-0000-7000-8000-000000000005',2,1,'restore',?,?,?,'conflict','history_expired',?,?)").bind(RESTORE_ID).bind(RESTORE).bind(hash(RESTORE)).bind(RESTORE.len() as i64).bind(receipt).execute(&mut db).await.unwrap();
    let root = serde_json::json!({"node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,"content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"","node_etag":"\"root-node\"","content_hash":null,"size":null,"children_generation":"0","last_change_seq":"0"});
    sqlx::query("INSERT INTO remote_index VALUES (1,?,'0')")
        .bind(ROOT)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO remote_nodes VALUES (1,?,?,1)")
        .bind(ROOT)
        .bind(root.to_string())
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

async fn snapshot(path: &Path) -> Vec<String> {
    let mut db = connect(path).await;
    let sql = "SELECT 'reg:'||quote(id)||':'||quote(server_origin)||':'||quote(workspace_id)||':'||quote(workspace_epoch)||':'||quote(device_id)||':'||quote(device_stream_id)||':'||quote(status) FROM client_registrations UNION ALL SELECT 'cred:'||quote(registration_id)||':'||quote(item_id) FROM credential_links UNION ALL SELECT 'doc:'||quote(registration_id)||':'||quote(document_id)||':'||quote(storage_id) FROM registered_documents UNION ALL SELECT 'draft:'||quote(document_id)||':'||quote(generation)||':'||quote(format_version)||':'||hex(bytes) FROM draft_checkpoints UNION ALL SELECT 'base:'||quote(document_id)||':'||hex(bytes)||':'||quote(revision) FROM draft_bases UNION ALL SELECT 'stream:'||quote(registration_id)||':'||quote(next_seq)||':'||quote(ack_through)||':'||quote(confirmed_through) FROM outbox_streams UNION ALL SELECT 'outbox:'||quote(operation_id)||':'||quote(registration_id)||':'||quote(operation_seq)||':'||quote(operation_kind)||':'||quote(document_id)||':'||quote(storage_id)||':'||quote(generation)||':'||quote(expected_revision)||':'||quote(dependency)||':'||hex(bytes)||':'||quote(payload_hash)||':'||quote(status)||':'||quote(committed_revision)||':'||quote(conflict_revision)||':'||quote(conflict_etag)||':'||quote(abandon_state)||':'||quote(terminal_kind)||':'||quote(payload_retired)||':'||quote(payload_len)||':'||quote(receipt_json) FROM client_outbox UNION ALL SELECT 'remote:'||quote(registration_id)||':'||quote(node_id)||':'||quote(state)||':'||quote(content_ready) FROM remote_nodes ORDER BY 1";
    let rows = sqlx::query_scalar(sql).fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

// Convert the immutable v9 fixture to an explicitly defined v10 schema without opening ClientStore.
async fn fixture_v10(path: &Path, obstruct: bool) {
    v9_fixture(path).await;
    let mut db = connect(path).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(r#"
        PRAGMA defer_foreign_keys=ON;
        CREATE TABLE client_outbox_v10 (
            operation_id TEXT PRIMARY KEY NOT NULL, registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            operation_seq INTEGER NOT NULL CHECK(operation_seq>0), operation_kind TEXT NOT NULL DEFAULT 'content' CHECK(operation_kind IN ('content','restore','create')),
            document_id TEXT NOT NULL, storage_id TEXT REFERENCES registered_documents(storage_id), generation INTEGER,
            expected_revision TEXT, dependency TEXT REFERENCES client_outbox_v10(operation_id),
            bytes BLOB NOT NULL CHECK(length(bytes)<=5242880), payload_hash TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('queued','sent','committed','conflict')),
            committed_revision TEXT, conflict_revision TEXT, conflict_etag TEXT,
            abandon_state TEXT NOT NULL DEFAULT 'none' CHECK(abandon_state IN ('none','requested','sent')),
            terminal_kind TEXT CHECK(terminal_kind IN ('abandoned','node_deleted','recovery_conflict','not_found','not_applied','not_deleted','history_expired','metadata_precondition_failed','destination_exists','parent_not_folder','node_id_conflict')),
            payload_retired INTEGER NOT NULL DEFAULT 0 CHECK(payload_retired IN (0,1)),
            payload_len INTEGER NOT NULL DEFAULT 0 CHECK(payload_len>=0 AND payload_len<=5242880),
            receipt_json TEXT CHECK(receipt_json IS NULL OR length(receipt_json)<=65536),
            CHECK((operation_kind='content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation>0) OR (operation_kind IN ('restore','create') AND storage_id IS NULL AND generation IS NULL)),
            UNIQUE(registration_id,operation_seq)
        );
        INSERT INTO client_outbox_v10(operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,dependency,bytes,payload_hash,status,committed_revision,conflict_revision,conflict_etag,abandon_state,terminal_kind,payload_retired,payload_len,receipt_json)
        SELECT operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,dependency,bytes,payload_hash,status,committed_revision,conflict_revision,conflict_etag,abandon_state,terminal_kind,payload_retired,payload_len,receipt_json FROM client_outbox;
        DROP TABLE client_outbox;
        ALTER TABLE client_outbox_v10 RENAME TO client_outbox;
        CREATE INDEX outbox_document_recent ON client_outbox(registration_id,document_id,operation_seq DESC);
        CREATE INDEX outbox_document_status ON client_outbox(registration_id,document_id,status,terminal_kind);
        CREATE INDEX outbox_dependencies ON client_outbox(registration_id,dependency,status);
        PRAGMA user_version=10;
    "#).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let b = binding();
    for (registration, device, stream) in [
        (
            3,
            "00000000-0000-4000-8000-000000000011",
            "00000000-0000-4000-8000-000000000012",
        ),
        (
            4,
            "00000000-0000-4000-8000-000000000013",
            "00000000-0000-4000-8000-000000000014",
        ),
    ] {
        sqlx::query("INSERT INTO client_registrations VALUES (?,?,?,?,?,?,'frozen')")
            .bind(registration)
            .bind(&b.server_origin)
            .bind(&b.workspace_id)
            .bind(&b.workspace_epoch)
            .bind(device)
            .bind(stream)
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO outbox_streams VALUES (?,3,0,0)")
            .bind(registration)
            .execute(&mut db)
            .await
            .unwrap();
    }
    for (registration, seq, op, node, kind, status) in [
        (
            3,
            1,
            "019a1000-0000-7000-8000-000000000011",
            "019a1000-0000-7000-8000-000000000021",
            "create",
            "sent",
        ),
        (
            3,
            2,
            "019a1000-0000-7000-8000-000000000012",
            "019a1000-0000-7000-8000-000000000022",
            "create",
            "queued",
        ),
        (
            4,
            1,
            "019a1000-0000-7000-8000-000000000013",
            "019a1000-0000-7000-8000-000000000023",
            "restore",
            "sent",
        ),
        (
            4,
            2,
            "019a1000-0000-7000-8000-000000000014",
            "019a1000-0000-7000-8000-000000000024",
            "restore",
            "queued",
        ),
    ] {
        let bytes = if kind == "create" {
            serde_json::to_vec(&serde_json::json!({"node_id":node,"parent_id":ROOT,"name":format!("folder-{seq}"),"kind":"folder"})).unwrap()
        } else {
            RESTORE.to_vec()
        };
        sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,bytes,payload_hash,status,payload_len) VALUES (?,?,?,?,?,?,?,?,?)")
            .bind(op).bind(registration).bind(seq).bind(kind).bind(node).bind(&bytes).bind(hash(&bytes)).bind(status).bind(bytes.len() as i64).execute(&mut db).await.unwrap();
    }
    if obstruct {
        sqlx::raw_sql("CREATE TABLE create_reconciliations (obstruction TEXT)")
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn v10_to_v11_preserves_all_existing_payloads_receipts_streams_and_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v10(&path, false).await;
    let before = snapshot(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let draft = store
        .read_registered_checkpoint(&binding(), "doc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"newer draft");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(store.receipt_ack_through(&binding()).await.unwrap(), 1);
    let PendingOutboxOperation::Content(first) = store
        .next_pending_outbox(&binding())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("sent content must remain first")
    };
    assert_eq!(
        (
            first.operation_id.as_str(),
            first.operation_seq,
            first.status
        ),
        ("op-2", 2, OutboxStatus::Sent)
    );
    store.close().await.unwrap();
    assert_eq!(snapshot(&path).await, before);
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 24);
    let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(errors, 0);
    let decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM create_reconciliations")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(decisions, 0);
    let types:Vec<(String,String,i64)>=sqlx::query_as("SELECT operation_kind,status,COUNT(*) FROM client_outbox WHERE status IN ('queued','sent') GROUP BY operation_kind,status ORDER BY operation_kind,status").fetch_all(&mut db).await.unwrap();
    assert_eq!(
        types,
        vec![
            ("content".into(), "queued".into(), 1),
            ("content".into(), "sent".into(), 1),
            ("create".into(), "queued".into(), 1),
            ("create".into(), "sent".into(), 1),
            ("restore".into(), "queued".into(), 2),
            ("restore".into(), "sent".into(), 1)
        ]
    );
    db.close().await.unwrap();
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn failed_v11_table_creation_rolls_back_version_and_existing_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v10(&path, true).await;
    let before = snapshot(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(snapshot(&path).await, before);
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 10);
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('create_reconciliations') ORDER BY cid",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(columns, vec!["obstruction".to_string()]);
    let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(errors, 0);
    db.close().await.unwrap();
    let mut recovery = notryx_client_store::RecoveryStore::open(&path)
        .await
        .unwrap();
    let drafts = recovery.drafts(None).await.unwrap();
    assert_eq!(drafts.items.len(), 1);
    assert_eq!(
        recovery
            .draft(&drafts.items[0].storage_id)
            .await
            .unwrap()
            .bytes,
        b"newer draft"
    );
    assert_eq!(recovery.operations(None).await.unwrap().items.len(), 9);
}

async fn fixture_v11(path: &Path, obstruct: bool) {
    fixture_v10(path, false).await;
    let mut db = connect(path).await;
    sqlx::raw_sql("CREATE TABLE create_reconciliations (old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id), decision TEXT NOT NULL CHECK(decision IN ('applied','dismissed')), new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id), CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))); INSERT INTO create_reconciliations VALUES ('019a1000-0000-7000-8000-000000000011','applied','019a1000-0000-7000-8000-000000000012'); PRAGMA user_version=11;").execute(&mut db).await.unwrap();
    if obstruct {
        sqlx::query("CREATE TABLE client_outbox_v12 (obstruction TEXT)")
            .execute(&mut db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn v11_to_v12_preserves_existing_decisions_receipts_and_drafts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v11(&path, false).await;
    let before = snapshot(&path).await;
    let store = ClientStore::open(&path).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(snapshot(&path).await, before);
    let mut db = connect(&path).await;
    let decision: (String, String, String) = sqlx::query_as(
        "SELECT old_operation_id,decision,new_operation_id FROM create_reconciliations",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        decision,
        (
            "019a1000-0000-7000-8000-000000000011".into(),
            "applied".into(),
            "019a1000-0000-7000-8000-000000000012".into()
        )
    );
    let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(errors, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn failed_v12_creation_keeps_v11_decisions_receipts_drafts_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v11(&path, true).await;
    let before = snapshot(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_eq!(snapshot(&path).await, before);
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 11);
    let decision: (String, String, String) = sqlx::query_as(
        "SELECT old_operation_id,decision,new_operation_id FROM create_reconciliations",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        decision,
        (
            "019a1000-0000-7000-8000-000000000011".into(),
            "applied".into(),
            "019a1000-0000-7000-8000-000000000012".into()
        )
    );
    let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(errors, 0);
    db.close().await.unwrap();
}
