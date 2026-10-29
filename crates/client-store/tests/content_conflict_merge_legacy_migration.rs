include!("outbox_v20_migration.rs");

use notryx_client_store::{ContentConflictChoice, ContentConflictIntentRequest};
use notryx_protocol::{InventoryPage, NodeState, ServerInfo};
use serde_json::json;

const LEGACY_MERGED: &[u8] = b"legacy merge bytes\n";
const LEGACY_HISTORY: &[u8] = b"older merge bytes\n";

async fn v20_legacy_merge_fixture(path: &Path) {
    fixture_v20_for_v21(path, false).await;
    let mut db = connect(path).await;
    sqlx::query("DELETE FROM content_conflict_copies WHERE operation_id=?")
        .bind(CONFLICT_OP)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE content_conflict_intents SET choice='save_merged',merged_bytes=?,intent_version=1 WHERE operation_id=?")
        .bind(LEGACY_MERGED).bind(CONFLICT_OP).execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO content_conflict_intent_history(operation_id,registration_id,history_seq,document_id,generation,base_revision,base_bytes,local_bytes,remote_revision,remote_hash,remote_bytes,choice,merged_bytes) SELECT operation_id,registration_id,1,document_id,generation,base_revision,base_bytes,local_bytes,remote_revision,remote_hash,remote_bytes,'save_merged',? FROM content_conflict_intents WHERE operation_id=?")
        .bind(LEGACY_HISTORY).bind(CONFLICT_OP).execute(&mut db).await.unwrap();
    sqlx::query("UPDATE client_registrations SET status='frozen' WHERE status='active'")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE client_registrations SET status='active' WHERE id=5")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE client_outbox SET status='committed',abandon_state='sent',terminal_kind='abandoned' WHERE operation_id=?")
        .bind(FOLLOWUP_OP).execute(&mut db).await.unwrap();
    sqlx::query(
        "UPDATE outbox_streams SET ack_through=2,confirmed_through=2 WHERE registration_id=5",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
}

async fn bootstrap_legacy_remote(store: &mut ClientStore, binding: &RegistrationBinding) {
    let info: ServerInfo = serde_json::from_value(json!({
        "protocol":"mdw-sync/1.2", "workspace_id":binding.workspace_id,
        "workspace_epoch":binding.workspace_epoch, "root_id":ROOT,
        "head_seq":"2", "journal_floor_seq":"0", "ready":true,
        "max_document_bytes":5242880, "max_attachment_bytes":26214400
    }))
    .unwrap();
    let root: NodeState = serde_json::from_value(json!({
        "node_id":ROOT,"parent_id":null,"name":"","kind":"folder","deleted":false,
        "content_rev":null,"content_etag":null,"meta_rev":"root-meta","meta_etag":"\"root-meta\"",
        "node_etag":"\"root-node\"","content_hash":null,"size":null,
        "children_generation":"1","last_change_seq":"2"
    }))
    .unwrap();
    let file: NodeState = serde_json::from_value(json!({
        "node_id":CONFLICT_NODE,"parent_id":ROOT,"name":"document.md","kind":"file","deleted":false,
        "content_rev":"current-rev","content_etag":"\"current-rev\"","meta_rev":"file-meta",
        "meta_etag":"\"file-meta\"","node_etag":"\"file-node\"",
        "content_hash":hash(V16_REMOTE),"size":V16_REMOTE.len().to_string(),
        "children_generation":"0","last_change_seq":"2"
    }))
    .unwrap();
    let bootstrap = store.begin_bootstrap(binding, &info).await.unwrap();
    store
        .bootstrap_inventory(
            binding,
            &bootstrap,
            &InventoryPage {
                workspace_epoch: info.workspace_epoch,
                nodes: vec![root, file.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(binding, &bootstrap, &info)
        .await
        .unwrap();
    store
        .bootstrap_publish(binding, &bootstrap, &info)
        .await
        .unwrap();
    assert!(
        store
            .cache_content(binding, &file, V16_REMOTE)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn v20_legacy_merge_and_history_remain_readable_without_claiming_prior_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    v20_legacy_merge_fixture(&path).await;
    let before = snapshot(&path).await;
    let intents_before = v16_intents(&path).await;
    let binding = frozen_binding_v16();
    let mut store = ClientStore::open(&path).await.unwrap();
    let current = store
        .read_content_conflict_intent(&binding, CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.choice,
        ContentConflictChoice::SaveMerged {
            bytes: LEGACY_MERGED.to_vec()
        }
    );
    assert_eq!(current.merged_hash, None);
    let history = store
        .read_content_conflict_intent_history(&binding, CONFLICT_OP, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        history.choice,
        ContentConflictChoice::SaveMerged {
            bytes: LEGACY_HISTORY.to_vec()
        }
    );
    assert_eq!(history.merged_hash, None);
    assert_eq!(
        store
            .read_registered_checkpoint(&binding, CONFLICT_NODE)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        V16_LOCAL
    );
    store.close().await.unwrap();
    assert_integrity(&path, 23).await;
    assert_eq!(snapshot(&path).await, before);
    assert_eq!(v16_intents(&path).await, intents_before);
}

#[tokio::test]
async fn v20_legacy_merge_requires_reconfirmation_before_queue_and_preserves_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    v20_legacy_merge_fixture(&path).await;
    let binding = frozen_binding_v16();
    let mut store = ClientStore::open(&path).await.unwrap();
    bootstrap_legacy_remote(&mut store, &binding).await;
    let old = store
        .read_content_conflict_intent(&binding, CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.merged_hash, None);
    let before = snapshot(&path).await;
    assert!(
        store
            .queue_content_conflict_merge(&binding, CONFLICT_OP, old.version)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&path).await, before);
    let request = ContentConflictIntentRequest {
        generation: old.generation,
        remote_revision: old.remote_revision.clone(),
        remote_hash: old.remote_hash.clone(),
        remote_bytes: old.remote_bytes.clone(),
        choice: ContentConflictChoice::SaveMerged {
            bytes: LEGACY_MERGED.to_vec(),
        },
    };
    let confirmed = store
        .replace_pending_content_conflict_intent(
            &binding,
            CONFLICT_NODE,
            CONFLICT_OP,
            &old,
            &request,
        )
        .await
        .unwrap();
    assert_eq!(confirmed.version, old.version + 1);
    assert_eq!(
        confirmed.merged_hash.as_deref(),
        Some(hash(LEGACY_MERGED).as_str())
    );
    assert_eq!(confirmed.choice, old.choice);
    assert_eq!(
        store
            .read_content_conflict_intent_history(&binding, CONFLICT_OP, old.version)
            .await
            .unwrap(),
        Some(old.clone())
    );
    assert_eq!(
        store
            .replace_pending_content_conflict_intent(
                &binding,
                CONFLICT_NODE,
                CONFLICT_OP,
                &old,
                &request
            )
            .await
            .unwrap(),
        confirmed
    );
    let queued = store
        .queue_content_conflict_merge(&binding, CONFLICT_OP, confirmed.version)
        .await
        .unwrap();
    assert_eq!(queued.bytes, LEGACY_MERGED);
    assert_eq!(queued.expected_revision.as_deref(), Some("current-rev"));
    let original = store
        .read_outbox_operation(&binding, CONFLICT_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.bytes, V16_SENT);
    assert_eq!(
        store
            .read_content_conflict_intent_history(&binding, CONFLICT_OP, 0)
            .await
            .unwrap()
            .unwrap()
            .choice,
        ContentConflictChoice::SaveMerged {
            bytes: LEGACY_HISTORY.to_vec()
        }
    );
}

#[tokio::test]
async fn v20_legacy_merge_does_not_authorize_followup_abandon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.sqlite");
    v20_legacy_merge_fixture(&path).await;
    let mut db = connect(&path).await;
    sqlx::query("UPDATE client_outbox SET status='queued',abandon_state='none',terminal_kind=NULL WHERE operation_id=?")
        .bind(FOLLOWUP_OP).execute(&mut db).await.unwrap();
    sqlx::query(
        "UPDATE outbox_streams SET ack_through=1,confirmed_through=1 WHERE registration_id=5",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    let binding = frozen_binding_v16();
    let mut store = ClientStore::open(&path).await.unwrap();
    bootstrap_legacy_remote(&mut store, &binding).await;
    let before = snapshot(&path).await;
    assert!(
        store
            .request_content_conflict_followup_abandon(&binding, CONFLICT_OP)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&path).await, before);
    let operation = store
        .read_outbox_operation(&binding, FOLLOWUP_OP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.status, notryx_client_store::OutboxStatus::Queued);
    assert_eq!(
        operation.abandon_state,
        notryx_client_store::OutboxAbandonState::None
    );
    assert_eq!(operation.bytes, V16_LOCAL);
}
