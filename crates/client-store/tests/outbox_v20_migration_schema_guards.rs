#[path = "support/migration_fixtures.rs"]
#[allow(dead_code)]
mod migration_fixtures;
use migration_fixtures::{
    CONFLICT_OP, FOLLOWUP_OP, ROOT, V16_LOCAL, assert_integrity, connect, fixture_v19,
    frozen_binding_v16, hash, snapshot, v16_intents,
};
use notryx_client_store::ClientStore;
use sqlx::Connection;

#[tokio::test]
async fn v19_content_operation_column_without_foreign_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v19(&path, false, false, false).await;
    let before = snapshot(&path).await;
    let mut db = connect(&path).await;
    sqlx::query("ALTER TABLE content_conflict_copies ADD COLUMN content_operation_id TEXT")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(ClientStore::open(&path).await.is_err());
    assert_integrity(&path, 19).await;
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn v19_attempts_table_without_version_check_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v19(&path, true, false, false).await;
    let before = snapshot(&path).await;
    let mut db = connect(&path).await;
    sqlx::raw_sql("CREATE TABLE content_conflict_copy_attempts(old_create_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),operation_id TEXT NOT NULL REFERENCES content_conflict_intents(operation_id),registration_id INTEGER NOT NULL REFERENCES client_registrations(id),intent_version INTEGER NOT NULL,old_parent_id TEXT NOT NULL,old_name TEXT NOT NULL,old_node_id TEXT NOT NULL,new_parent_id TEXT NOT NULL,new_name TEXT NOT NULL,new_node_id TEXT NOT NULL,new_create_operation_id TEXT NOT NULL UNIQUE)").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(ClientStore::open(&path).await.is_err());
    assert_integrity(&path, 19).await;
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn v19_existing_global_content_unique_constraint_is_not_duplicated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v19(&path, true, false, false).await;
    ClientStore::open(&path)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    let mut db = connect(&path).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT x.name FROM pragma_index_list('content_conflict_copies') x JOIN pragma_index_info(x.name) y WHERE x.\"unique\"=1 GROUP BY x.name HAVING count(*)=1 AND max(y.name)='content_operation_id')").fetch_one(&mut db).await.unwrap();
    assert_eq!(count, 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn v19_partial_attempt_id_index_is_not_a_global_uniqueness_guarantee() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v19(&path, true, false, false).await;
    let before = snapshot(&path).await;
    let mut db = connect(&path).await;
    sqlx::raw_sql("CREATE TABLE content_conflict_copy_attempts(old_create_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),operation_id TEXT NOT NULL REFERENCES content_conflict_intents(operation_id),registration_id INTEGER NOT NULL REFERENCES client_registrations(id),intent_version INTEGER NOT NULL CHECK(intent_version>=0),old_parent_id TEXT NOT NULL,old_name TEXT NOT NULL,old_node_id TEXT NOT NULL,new_parent_id TEXT NOT NULL,new_name TEXT NOT NULL,new_node_id TEXT NOT NULL,new_create_operation_id TEXT NOT NULL); CREATE UNIQUE INDEX partial_attempt_id ON content_conflict_copy_attempts(new_create_operation_id) WHERE intent_version > 0;").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(ClientStore::open(&path).await.is_err());
    assert_integrity(&path, 19).await;
    assert_eq!(snapshot(&path).await, before);
}

#[tokio::test]
async fn v19_orphan_attempt_is_rejected_and_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v19(&path, true, true, false).await;
    let mut db = connect(&path).await;
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO content_conflict_copy_attempts VALUES('missing-create',?,5,0,?,'old.md','old-node',?,'new.md','new-node','new-create')").bind(CONFLICT_OP).bind(ROOT).bind(ROOT).execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(ClientStore::open(&path).await.is_err());
    let mut db = connect(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 19);
    let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(violations, 1);
    let orphan: String =
        sqlx::query_scalar("SELECT old_create_operation_id FROM content_conflict_copy_attempts")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(orphan, "missing-create");
    db.close().await.unwrap();
}

#[tokio::test]
async fn v19_repeated_failed_attempts_receipts_and_current_copy_survive_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    fixture_v19(&path, true, true, false).await;
    let binding = frozen_binding_v16();
    let mut db = connect(&path).await;
    let ids = [
        "019a1000-0000-7000-8000-000000000081",
        "019a1000-0000-7000-8000-000000000082",
        "019a1000-0000-7000-8000-000000000083",
    ];
    let nodes = [
        "019a1000-0000-7000-8000-000000000091",
        "019a1000-0000-7000-8000-000000000092",
        "019a1000-0000-7000-8000-000000000093",
    ];
    let names = ["first-copy.md", "second-copy.md", "third-copy.md"];
    let abandoned = serde_json::json!({"status":"abandoned","operation_id":FOLLOWUP_OP,"operation_seq":"2","workspace_epoch":binding.workspace_epoch}).to_string();
    sqlx::query("UPDATE client_outbox SET status='committed',terminal_kind='abandoned',abandon_state='sent',receipt_json=? WHERE operation_id=?").bind(abandoned).bind(FOLLOWUP_OP).execute(&mut db).await.unwrap();
    for index in 0..2 {
        let seq = index as i64 + 3;
        let payload = serde_json::to_vec(&serde_json::json!({"node_id":nodes[index],"parent_id":ROOT,"name":names[index],"kind":"file"})).unwrap();
        let receipt = serde_json::json!({"status":"destination_exists","operation_id":ids[index],"operation_seq":seq.to_string(),"workspace_epoch":binding.workspace_epoch}).to_string();
        sqlx::query("INSERT INTO client_outbox(operation_id,registration_id,operation_seq,operation_kind,document_id,bytes,payload_hash,status,terminal_kind,payload_len,receipt_json) VALUES(?,5,?,'create',?,?,?,'conflict','destination_exists',?,?)")
            .bind(ids[index]).bind(seq).bind(nodes[index]).bind(&payload).bind(hash(&payload)).bind(payload.len() as i64).bind(receipt).execute(&mut db).await.unwrap();
        sqlx::query("INSERT INTO content_conflict_copy_attempts VALUES(?,?,5,0,?,?,?,?,?,?,?)")
            .bind(ids[index])
            .bind(CONFLICT_OP)
            .bind(ROOT)
            .bind(names[index])
            .bind(nodes[index])
            .bind(ROOT)
            .bind(names[index + 1])
            .bind(nodes[index + 1])
            .bind(ids[index + 1])
            .execute(&mut db)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE outbox_streams SET next_seq=5,ack_through=4,confirmed_through=4 WHERE registration_id=5").execute(&mut db).await.unwrap();
    sqlx::query("UPDATE content_conflict_copies SET node_id=?,create_operation_id=?,name=? WHERE operation_id=?")
        .bind(nodes[2]).bind(ids[2]).bind(names[2]).bind(CONFLICT_OP).execute(&mut db).await.unwrap();
    let before_attempts: Vec<(String,String,String,String)> = sqlx::query_as("SELECT old_create_operation_id,old_name,new_create_operation_id,new_name FROM content_conflict_copy_attempts ORDER BY old_create_operation_id").fetch_all(&mut db).await.unwrap();
    db.close().await.unwrap();
    let before = snapshot(&path).await;
    let intents = v16_intents(&path).await;
    let mut store = ClientStore::open(&path).await.unwrap();
    let copy = store
        .read_content_conflict_copy(&binding, CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.node_id, nodes[2]);
    assert_eq!(copy.create_operation_id, ids[2]);
    assert_eq!(copy.name, names[2]);
    assert_eq!(copy.local_bytes, V16_LOCAL);
    let entries = store
        .list_create_operations(&binding, 0, 100)
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 2);
    for (entry, id) in entries.iter().zip(ids.iter()) {
        assert_eq!(&entry.operation.operation_id, id);
        assert!(matches!(
            entry.receipt,
            Some(notryx_protocol::OperationReceipt::DestinationExists { .. })
        ));
    }
    store.close().await.unwrap();
    assert_integrity(&path, 24).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents);
    let mut db = connect(&path).await;
    let after: Vec<(String,String,String,String)> = sqlx::query_as("SELECT old_create_operation_id,old_name,new_create_operation_id,new_name FROM content_conflict_copy_attempts ORDER BY old_create_operation_id").fetch_all(&mut db).await.unwrap();
    assert_eq!(after, before_attempts);
    db.close().await.unwrap();
}
