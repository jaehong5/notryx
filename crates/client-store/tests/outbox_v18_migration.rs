use notryx_client_store::{ClientStore, RegistrationBinding, StoreError};
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

// This v13 fixture is assembled from fixed older schemas without opening ClientStore.
async fn fixture_v13(path: &Path, obstruct: bool) {
    fixture_v10(path, false).await;
    let mut db = connect(path).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql("CREATE TABLE create_reconciliations (old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id), decision TEXT NOT NULL CHECK(decision IN ('applied','dismissed')), new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id), CHECK((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))); PRAGMA user_version=11;").execute(&mut *tx).await.unwrap();
    sqlx::raw_sql(r#"
    PRAGMA defer_foreign_keys = ON;
    CREATE TABLE create_reconciliations_v12 (
        old_operation_id TEXT PRIMARY KEY NOT NULL,
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE,
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations_v12 SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations;
    DROP TABLE create_reconciliations;
    CREATE TABLE client_outbox_v12 (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore', 'create', 'delete')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox_v12(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder', 'node_id_conflict', 'node_precondition_failed')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind IN ('restore', 'create', 'delete') AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
    INSERT INTO client_outbox_v12
        (operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json)
    SELECT operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json
    FROM client_outbox;
    DROP TABLE client_outbox;
    ALTER TABLE client_outbox_v12 RENAME TO client_outbox;
    CREATE TABLE create_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations_v12;
    DROP TABLE create_reconciliations_v12;
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 12;
"#).execute(&mut *tx).await.unwrap();
    sqlx::raw_sql("CREATE TABLE delete_reconciliations (old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id), decision TEXT NOT NULL CHECK(decision IN ('applied','dismissed')), new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id), CHECK((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))); PRAGMA user_version=13;").execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO create_reconciliations VALUES ('019a1000-0000-7000-8000-000000000011','applied','019a1000-0000-7000-8000-000000000012')").execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES ('019a1000-0000-7000-8000-000000000031',4,3,'delete','019a1000-0000-7000-8000-000000000032','\"node-1\"',X'','sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','sent',0)").execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE outbox_streams SET next_seq=4 WHERE registration_id=4")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO delete_reconciliations VALUES ('019a1000-0000-7000-8000-000000000031','dismissed',NULL)").execute(&mut *tx).await.unwrap();
    if obstruct {
        sqlx::query("CREATE TABLE client_outbox_v14 (obstruction TEXT)")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
}

async fn decisions(path: &Path) -> Vec<String> {
    let mut db = connect(path).await;
    let rows = sqlx::query_scalar("SELECT 'create:'||quote(old_operation_id)||':'||decision||':'||quote(new_operation_id) FROM create_reconciliations UNION ALL SELECT 'delete:'||quote(old_operation_id)||':'||decision||':'||quote(new_operation_id) FROM delete_reconciliations ORDER BY 1").fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

// This v14 fixture is assembled from fixed older schemas without opening ClientStore.
async fn fixture_v14(path: &Path, obstruct: bool) {
    fixture_v13(path, false).await;
    let mut db = connect(path).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(r#"
    PRAGMA defer_foreign_keys = ON;
    CREATE TABLE create_reconciliations_v14 (
        old_operation_id TEXT PRIMARY KEY NOT NULL,
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE,
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations_v14 SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations;
    DROP TABLE create_reconciliations;
    CREATE TABLE delete_reconciliations_v14 (
        old_operation_id TEXT PRIMARY KEY NOT NULL,
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE,
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO delete_reconciliations_v14 SELECT old_operation_id, decision, new_operation_id FROM delete_reconciliations;
    DROP TABLE delete_reconciliations;
    CREATE TABLE client_outbox_v14 (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore', 'create', 'delete', 'move')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox_v14(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder', 'node_id_conflict', 'node_precondition_failed', 'expected_parent_mismatch')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind IN ('restore', 'create', 'delete', 'move') AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
    INSERT INTO client_outbox_v14
        (operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json)
    SELECT operation_id, registration_id, operation_seq, operation_kind, document_id, storage_id, generation,
         expected_revision, dependency, bytes, payload_hash, status, committed_revision,
         conflict_revision, conflict_etag, abandon_state, terminal_kind, payload_retired, payload_len, receipt_json
    FROM client_outbox;
    DROP TABLE client_outbox;
    ALTER TABLE client_outbox_v14 RENAME TO client_outbox;
    CREATE TABLE create_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO create_reconciliations SELECT old_operation_id, decision, new_operation_id FROM create_reconciliations_v14;
    DROP TABLE create_reconciliations_v14;
    CREATE TABLE delete_reconciliations (
        old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
        decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
        new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
        CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
    );
    INSERT INTO delete_reconciliations SELECT old_operation_id, decision, new_operation_id FROM delete_reconciliations_v14;
    DROP TABLE delete_reconciliations_v14;
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 14;
"#).execute(&mut *tx).await.unwrap();
    let bytes = serde_json::to_vec(
        &serde_json::json!({"expected_parent_id":ROOT,"parent_id":ROOT,"name":"renamed.md"}),
    )
    .unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,expected_revision,bytes,payload_hash,status,payload_len) VALUES ('019a1000-0000-7000-8000-000000000041',4,4,'move','019a1000-0000-7000-8000-000000000042','\"meta-1\"',?,?,'sent',?)")
        .bind(&bytes).bind(hash(&bytes)).bind(bytes.len() as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE outbox_streams SET next_seq=5 WHERE registration_id=4")
        .execute(&mut *tx)
        .await
        .unwrap();
    if obstruct {
        sqlx::query("CREATE TABLE move_reconciliations (obstruction TEXT)")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
}

// Assemble v15 from fixed legacy schemas without opening the current ClientStore.
async fn fixture_v15(path: &Path, obstruct: bool) {
    fixture_v14(path, false).await;
    let mut db = connect(path).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(r#"
        CREATE TABLE move_reconciliations (
            old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
            decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
            new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
            CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
        );
        INSERT INTO move_reconciliations VALUES ('019a1000-0000-7000-8000-000000000041','dismissed',NULL);
        PRAGMA user_version = 15;
    "#).execute(&mut *tx).await.unwrap();
    if obstruct {
        sqlx::query("CREATE TABLE content_conflict_intents (obstruction TEXT)")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
}

async fn move_decisions(path: &Path) -> Vec<(String, String, Option<String>)> {
    let mut db = connect(path).await;
    let rows = sqlx::query_as("SELECT old_operation_id, decision, new_operation_id FROM move_reconciliations ORDER BY old_operation_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

async fn assert_integrity(path: &Path, version: i64) {
    let mut db = connect(path).await;
    let actual: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(actual, version);
    let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(errors, 0);
    let check: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(check, "ok");
    db.close().await.unwrap();
}

const CONFLICT_NODE: &str = "019a1000-0000-7000-8000-000000000052";
const CONFLICT_OP: &str = "019a1000-0000-7000-8000-000000000071";
const FOLLOWUP_OP: &str = "019a1000-0000-7000-8000-000000000072";
const V16_BASE: &[u8] = b"base-v16\r\n";
const V16_SENT: &[u8] = b"sent-v16\n";
const V16_LOCAL: &[u8] = b"local-v16\n";
const V16_REMOTE: &[u8] = b"peer-v16\n";
const COMPLETED_NODE: &str = "019a1000-0000-7000-8000-000000000053";
const COMPLETED_OP: &str = "019a1000-0000-7000-8000-000000000073";

fn frozen_binding_v16() -> RegistrationBinding {
    let mut value = binding();
    value.device_id = "00000000-0000-4000-8000-000000000015".into();
    value.device_stream_id = "00000000-0000-4000-8000-000000000016".into();
    value
}

// Construct the previous schema independently of ClientStore migrations.
async fn fixture_v16(path: &Path, obstruct: bool) {
    fixture_v15(path, false).await;
    let mut db = connect(path).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(r#"
        CREATE TABLE content_conflict_intents (
            operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
            registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
            document_id TEXT NOT NULL,
            generation INTEGER NOT NULL CHECK(generation > 0),
            base_revision TEXT NOT NULL,
            base_bytes BLOB NOT NULL CHECK(length(base_bytes) <= 5242880),
            local_bytes BLOB NOT NULL CHECK(length(local_bytes) <= 5242880),
            remote_revision TEXT NOT NULL,
            remote_hash TEXT NOT NULL,
            remote_bytes BLOB NOT NULL CHECK(length(remote_bytes) <= 5242880),
            choice TEXT NOT NULL CHECK(choice IN ('keep_local_copy', 'choose_remote', 'save_merged')),
            merged_bytes BLOB CHECK(merged_bytes IS NULL OR length(merged_bytes) <= 5242880),
            CHECK((choice='save_merged' AND merged_bytes IS NOT NULL) OR (choice<>'save_merged' AND merged_bytes IS NULL))
        );
        PRAGMA user_version=16;
    "#).execute(&mut *tx).await.unwrap();
    let old = frozen_binding_v16();
    sqlx::query("INSERT INTO client_registrations VALUES(5,?,?,?,?,?,'frozen')")
        .bind(&old.server_origin)
        .bind(&old.workspace_id)
        .bind(&old.workspace_epoch)
        .bind(&old.device_id)
        .bind(&old.device_stream_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO registered_documents VALUES(5,?,'v16-conflict-storage')")
        .bind(CONFLICT_NODE)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO draft_checkpoints VALUES('v16-conflict-storage',2,1,?)")
        .bind(V16_LOCAL)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO draft_bases VALUES('v16-conflict-storage',?,'base-rev')")
        .bind(V16_BASE)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox_streams VALUES(5,3,1,1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,conflict_revision,conflict_etag,payload_len) VALUES (?,5,1,'content',?,'v16-conflict-storage',1,'base-rev',?,?,'conflict','peer-rev','\"peer-rev\"',?)")
        .bind(CONFLICT_OP).bind(CONFLICT_NODE).bind(V16_SENT).bind(hash(V16_SENT)).bind(V16_SENT.len() as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,dependency,bytes,payload_hash,status,payload_len) VALUES (?,5,2,'content',?,'v16-conflict-storage',2,?,?,?,'queued',?)")
        .bind(FOLLOWUP_OP).bind(CONFLICT_NODE).bind(CONFLICT_OP).bind(V16_LOCAL).bind(hash(V16_LOCAL)).bind(V16_LOCAL.len() as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO content_conflict_intents(operation_id,registration_id,document_id,generation,base_revision,base_bytes,local_bytes,remote_revision,remote_hash,remote_bytes,choice,merged_bytes) VALUES (?,5,?,2,'base-rev',?,?,'current-rev',?,?,'choose_remote',NULL)")
        .bind(CONFLICT_OP).bind(CONFLICT_NODE).bind(V16_BASE).bind(V16_LOCAL).bind(hash(V16_REMOTE)).bind(V16_REMOTE).execute(&mut *tx).await.unwrap();
    if obstruct {
        sqlx::query("ALTER TABLE content_conflict_intents ADD COLUMN completed_generation INTEGER")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE content_conflict_intents SET completed_generation=99 WHERE operation_id=?",
        )
        .bind(CONFLICT_OP)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
}

async fn v16_intents(path: &Path) -> Vec<String> {
    let mut db = connect(path).await;
    let rows = sqlx::query_scalar("SELECT quote(operation_id)||':'||quote(registration_id)||':'||quote(document_id)||':'||quote(generation)||':'||quote(base_revision)||':'||hex(base_bytes)||':'||hex(local_bytes)||':'||quote(remote_revision)||':'||quote(remote_hash)||':'||hex(remote_bytes)||':'||quote(choice)||':'||quote(merged_bytes) FROM content_conflict_intents ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    rows
}

// Construct v17 without invoking the migration under test.
async fn fixture_v17(path: &Path, obstruct: bool) {
    fixture_v16(path, false).await;
    let mut db = connect(path).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(
        "ALTER TABLE content_conflict_intents ADD COLUMN completed_generation INTEGER
             CHECK(completed_generation IS NULL OR (choice='choose_remote' AND completed_generation>generation));
         ALTER TABLE content_conflict_intents ADD COLUMN completed_through INTEGER
             CHECK((completed_generation IS NULL AND completed_through IS NULL) OR
                   (completed_generation IS NOT NULL AND completed_through IS NOT NULL AND completed_through>0));
         PRAGMA user_version=17;",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let mut completed_binding = binding();
    completed_binding.device_id = "00000000-0000-4000-8000-000000000017".into();
    completed_binding.device_stream_id = "00000000-0000-4000-8000-000000000018".into();
    sqlx::query("INSERT INTO client_registrations VALUES(6,?,?,?,?,?,'frozen')")
        .bind(&completed_binding.server_origin)
        .bind(&completed_binding.workspace_id)
        .bind(&completed_binding.workspace_epoch)
        .bind(&completed_binding.device_id)
        .bind(&completed_binding.device_stream_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO registered_documents VALUES(6,?,'v17-completed-storage')")
        .bind(COMPLETED_NODE)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO draft_checkpoints VALUES('v17-completed-storage',2,1,?)")
        .bind(V16_REMOTE)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO draft_bases VALUES('v17-completed-storage',?,'current-rev')")
        .bind(V16_REMOTE)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox_streams VALUES(6,2,1,1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO client_outbox (operation_id,registration_id,operation_seq,operation_kind,document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,conflict_revision,conflict_etag,payload_len) VALUES (?,6,1,'content',?,'v17-completed-storage',1,'base-rev',?,?,'conflict','peer-rev','\"peer-rev\"',?)")
        .bind(COMPLETED_OP).bind(COMPLETED_NODE).bind(V16_SENT).bind(hash(V16_SENT)).bind(V16_SENT.len() as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO content_conflict_intents(operation_id,registration_id,document_id,generation,base_revision,base_bytes,local_bytes,remote_revision,remote_hash,remote_bytes,choice,merged_bytes,completed_generation,completed_through) VALUES (?,6,?,1,'base-rev',?,?,'current-rev',?,?,'choose_remote',NULL,2,1)")
        .bind(COMPLETED_OP).bind(COMPLETED_NODE).bind(V16_BASE).bind(V16_SENT).bind(hash(V16_REMOTE)).bind(V16_REMOTE).execute(&mut *tx).await.unwrap();
    if obstruct {
        sqlx::raw_sql("CREATE TABLE content_conflict_intent_history (sentinel TEXT NOT NULL); INSERT INTO content_conflict_intent_history VALUES ('preserve-me');")
            .execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    db.close().await.unwrap();
}

async fn completed_markers(path: &Path) -> Vec<(String, Option<i64>, Option<i64>)> {
    let mut db = connect(path).await;
    let result = sqlx::query_as("SELECT operation_id,completed_generation,completed_through FROM content_conflict_intents ORDER BY operation_id")
        .fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    result
}

#[tokio::test]
async fn v17_to_v18_preserves_existing_data_and_creates_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v17(&path, false).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    let create_delete = decisions(&path).await;
    let moves = move_decisions(&path).await;
    assert_eq!(
        markers,
        vec![
            (CONFLICT_OP.to_owned(), None, None),
            (COMPLETED_OP.to_owned(), Some(2), Some(1))
        ]
    );

    let mut store = ClientStore::open(&path).await.unwrap();
    let preserved = store
        .read_content_conflict_intent(&frozen_binding_v16(), CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preserved.base_bytes, V16_BASE);
    assert_eq!(preserved.local_bytes, V16_LOCAL);
    assert_eq!(preserved.remote_bytes, V16_REMOTE);
    assert_eq!(preserved.remote_hash, hash(V16_REMOTE));
    assert_eq!(
        store
            .read_registered_checkpoint(&frozen_binding_v16(), CONFLICT_NODE)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        V16_LOCAL
    );
    assert_eq!(
        store
            .read_outbox_operation(&frozen_binding_v16(), CONFLICT_OP)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        V16_SENT
    );
    assert_eq!(
        store
            .read_outbox_operation(&frozen_binding_v16(), FOLLOWUP_OP)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        V16_LOCAL
    );
    store.close().await.unwrap();

    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, create_delete);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let history_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM content_conflict_intent_history")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(history_count, 0);
    db.close().await.unwrap();
    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn failed_v18_migration_rolls_back_version_and_preserves_obstruction_and_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v17(&path, true).await;
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let markers = completed_markers(&path).await;
    let create_delete = decisions(&path).await;
    let moves = move_decisions(&path).await;
    assert!(matches!(
        ClientStore::open(&path).await,
        Err(StoreError::Storage)
    ));
    assert_integrity(&path, 17).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, create_delete);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let sentinel: String =
        sqlx::query_scalar("SELECT sentinel FROM content_conflict_intent_history")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(sentinel, "preserve-me");
    sqlx::query("DROP TABLE content_conflict_intent_history")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();

    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    assert_eq!(completed_markers(&path).await, markers);
    assert_eq!(decisions(&path).await, create_delete);
    assert_eq!(move_decisions(&path).await, moves);
    let mut db = connect(&path).await;
    let history_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM content_conflict_intent_history")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(history_count, 0);
    db.close().await.unwrap();
}
