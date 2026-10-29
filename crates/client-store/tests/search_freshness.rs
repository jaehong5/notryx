use notryx_client_store::{
    ClientStore, OutboxCommit, OutboxOperation, OutboxStatus, RegistrationBinding, SearchScope,
    SearchSource, StoreError, search_contains,
};
use notryx_domain::ChangeSeq;
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const DOCUMENT: &str = "019a1000-0000-7000-8000-000000000002";

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

async fn seeded() -> (
    tempfile::TempDir,
    ClientStore,
    RegistrationBinding,
    NodeState,
    NodeState,
) {
    let directory = tempfile::tempdir().unwrap();
    let binding = RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    };
    let info: ServerInfo = serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2", "workspace_id": binding.workspace_id,
        "workspace_epoch": binding.workspace_epoch, "root_id": ROOT,
        "head_seq": "1", "journal_floor_seq": "0", "ready": true,
        "max_document_bytes": 5242880, "max_attachment_bytes": 26214400
    }))
    .unwrap();
    let root: NodeState = serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder", "deleted": false,
        "content_rev": null, "content_etag": null, "meta_rev": "root-meta",
        "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"", "content_hash": null,
        "size": null, "children_generation": "1", "last_change_seq": "1"
    }))
    .unwrap();
    let node: NodeState = serde_json::from_value(json!({
        "node_id": DOCUMENT, "parent_id": ROOT, "name": "document.md", "kind": "file", "deleted": false,
        "content_rev": "content-1", "content_etag": "\"content-1\"", "meta_rev": "meta-1",
        "meta_etag": "\"meta-1\"", "node_etag": "\"node-1\"", "content_hash": digest(b"base"),
        "size": "4", "children_generation": "0", "last_change_seq": "1"
    })).unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&binding).await.unwrap();
    let session = store.begin_bootstrap(&binding, &info).await.unwrap();
    store
        .bootstrap_inventory(
            &binding,
            &session,
            &InventoryPage {
                workspace_epoch: info.workspace_epoch,
                nodes: vec![root.clone(), node.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&binding, &session, &info)
        .await
        .unwrap();
    store
        .bootstrap_publish(&binding, &session, &info)
        .await
        .unwrap();
    assert!(store.cache_content(&binding, &node, b"base").await.unwrap());
    (directory, store, binding, root, node)
}

fn page(binding: &RegistrationBinding, nodes: Vec<NodeState>) -> ChangesPage {
    serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch, "from_exclusive": "1", "through_seq": "2",
        "covered_through": "2", "has_more": false, "nodes": nodes
    }))
    .unwrap()
}

fn content_node(mut node: NodeState, revision: &str, bytes: &[u8], sequence: u64) -> NodeState {
    node.content_rev = Some(revision.to_owned().try_into().unwrap());
    node.content_etag = Some(format!("\"{revision}\""));
    node.node_etag = format!("\"node-{sequence}\"");
    node.content_hash = Some(digest(bytes));
    node.size = Some(bytes.len().to_string());
    node.last_change_seq = ChangeSeq::new(sequence);
    node
}

fn change_page(binding: &RegistrationBinding, from: u64, node: NodeState) -> ChangesPage {
    serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch, "from_exclusive": from.to_string(),
        "through_seq": (from + 1).to_string(), "covered_through": (from + 1).to_string(),
        "has_more": false, "nodes": [node]
    }))
    .unwrap()
}

fn committed(operation: &OutboxOperation, revision: &str) -> OutboxCommit {
    OutboxCommit {
        operation_id: operation.operation_id.clone(),
        operation_seq: operation.operation_seq,
        workspace_epoch: operation.binding.workspace_epoch.clone(),
        document_id: operation.document_id.clone(),
        content_revision: revision.into(),
        content_hash: operation.payload_hash.clone(),
    }
}

async fn two_commits_with_retired_first() -> (
    tempfile::TempDir,
    ClientStore,
    RegistrationBinding,
    NodeState,
    OutboxOperation,
    OutboxOperation,
) {
    let (directory, mut store, binding, _root, node) = seeded().await;
    let opened = store
        .open_registered_document(&binding, DOCUMENT, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            DOCUMENT,
            &opened.edit_session_id,
            1,
            b"first committed",
        )
        .await
        .unwrap();
    store.queue_checkpoint(&binding, DOCUMENT, 1).await.unwrap();
    let first = store.prepare_outbox(&binding).await.unwrap().unwrap();
    assert_eq!(first.operation_seq, 1);
    assert_eq!(first.expected_revision.as_deref(), Some("content-1"));
    store
        .apply_outbox_commit(&binding, &committed(&first, "content-2"))
        .await
        .unwrap();
    let node = content_node(node, "content-2", b"first committed", 2);
    store
        .apply_changes(
            &binding,
            &change_page(&binding, 1, node.clone()),
            ChangeSeq::new(2),
        )
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&binding, &node, b"first committed")
            .await
            .unwrap()
    );
    store
        .save_registered_checkpoint(
            &binding,
            DOCUMENT,
            &opened.edit_session_id,
            2,
            b"second committed",
        )
        .await
        .unwrap();
    store.queue_checkpoint(&binding, DOCUMENT, 2).await.unwrap();
    let second = store.prepare_outbox(&binding).await.unwrap().unwrap();
    assert_eq!(second.operation_seq, 2);
    assert_eq!(second.expected_revision.as_deref(), Some("content-2"));
    store
        .apply_outbox_commit(&binding, &committed(&second, "content-3"))
        .await
        .unwrap();
    let ack = serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": "2", "retired_through": "2"
    }))
    .unwrap();
    store.confirm_receipt_ack(&binding, &ack).await.unwrap();
    assert_eq!(store.receipt_ack_through(&binding).await.unwrap(), 2);
    assert_eq!(store.confirmed_receipt_through(&binding).await.unwrap(), 2);
    assert_eq!(
        store
            .read_outbox_operation(&binding, &first.operation_id)
            .await,
        Err(StoreError::PayloadRetired)
    );
    let entries = store.list_outbox(&binding, 0, 10).await.unwrap().entries;
    assert_eq!(entries.len(), 2);
    assert_eq!((entries[0].operation_seq, entries[1].operation_seq), (1, 2));
    assert!(entries[0].payload_retired);
    assert!(!entries[1].payload_retired);
    assert_eq!(entries[1].status, OutboxStatus::Committed);
    assert_eq!(
        store
            .read_outbox_operation(&binding, &second.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"second committed"
    );
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"second committed");
    assert_eq!(draft.base_bytes, b"second committed");
    assert_eq!(draft.base_revision, "content-3");
    let (cursor, nodes) = store.sync_index(&binding).await.unwrap().unwrap();
    assert_eq!(cursor, ChangeSeq::new(2));
    assert_eq!(
        nodes.iter().find(|current| current.node_id == node.node_id),
        Some(&node)
    );
    (directory, store, binding, node, first, second)
}

#[tokio::test]
async fn committed_base_remains_searchable_after_earlier_payload_is_retired() {
    let (_directory, mut store, binding, node, first, second) =
        two_commits_with_retired_first().await;
    let cache = store
        .read_cached_content(&binding, node.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(cache.current);
    assert_eq!(cache.bytes, b"first committed");
    let page = store.search_candidates(&binding, None, 100).await.unwrap();
    assert_eq!((page.available_documents, page.total_documents), (1, 1));
    let candidate = page
        .candidates
        .iter()
        .find(|candidate| candidate.node_id == node.node_id)
        .unwrap();
    assert_eq!(candidate.source, SearchSource::Draft);
    assert_eq!(candidate.bytes(), b"second committed");
    assert!(search_contains(candidate.bytes(), "second"));
    assert!(!search_contains(candidate.bytes(), "first"));
    assert!(
        store
            .search_candidate_is_current(&binding, candidate)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .read_outbox_operation(&binding, &first.operation_id)
            .await,
        Err(StoreError::PayloadRetired)
    );
    assert_eq!(
        store
            .read_outbox_operation(&binding, &second.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"second committed"
    );
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (draft.bytes, draft.base_bytes, draft.base_revision.as_str()),
        (
            b"second committed".to_vec(),
            b"second committed".to_vec(),
            "content-3"
        )
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn later_server_metadata_and_cache_replace_an_older_committed_base_in_search() {
    let (_directory, mut store, binding, node, first, second) =
        two_commits_with_retired_first().await;
    let committed = content_node(node, "content-3", b"second committed", 3);
    store
        .apply_changes(
            &binding,
            &change_page(&binding, 2, committed.clone()),
            ChangeSeq::new(3),
        )
        .await
        .unwrap();
    let later = content_node(committed, "content-4", b"later server text", 4);
    store
        .apply_changes(
            &binding,
            &change_page(&binding, 3, later.clone()),
            ChangeSeq::new(4),
        )
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&binding, &later, b"later server text")
            .await
            .unwrap()
    );
    let cache = store
        .read_cached_content(&binding, later.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(cache.current);
    assert_eq!(cache.bytes, b"later server text");
    let page = store.search_candidates(&binding, None, 100).await.unwrap();
    assert_eq!((page.available_documents, page.total_documents), (1, 1));
    let candidate = page
        .candidates
        .iter()
        .find(|candidate| candidate.node_id == later.node_id)
        .unwrap();
    assert_eq!(candidate.source, SearchSource::Cache);
    assert_eq!(candidate.bytes(), b"later server text");
    assert!(search_contains(candidate.bytes(), "later server"));
    assert!(!search_contains(candidate.bytes(), "second committed"));
    assert!(
        store
            .search_candidate_is_current(&binding, candidate)
            .await
            .unwrap()
    );
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"second committed");
    assert_eq!(draft.base_bytes, b"second committed");
    assert_eq!(draft.base_revision, "content-3");
    assert_eq!(
        store
            .read_outbox_operation(&binding, &first.operation_id)
            .await,
        Err(StoreError::PayloadRetired)
    );
    assert_eq!(
        store
            .read_outbox_operation(&binding, &second.operation_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"second committed"
    );
    let (cursor, nodes) = store.sync_index(&binding).await.unwrap().unwrap();
    assert_eq!(cursor, ChangeSeq::new(4));
    assert_eq!(
        nodes
            .iter()
            .find(|current| current.node_id == later.node_id),
        Some(&later)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn unconfirmed_checkpoint_equal_to_base_remains_a_searchable_draft() {
    let (_directory, mut store, binding, _root, node) = seeded().await;
    let opened = store
        .open_registered_document(&binding, DOCUMENT, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, DOCUMENT, &opened.edit_session_id, 1, b"base")
        .await
        .unwrap();
    store
        .close_registered_document(&binding, DOCUMENT, &opened.edit_session_id)
        .await
        .unwrap();
    let later = content_node(node, "content-2", b"new server text", 2);
    store
        .apply_changes(
            &binding,
            &change_page(&binding, 1, later.clone()),
            ChangeSeq::new(2),
        )
        .await
        .unwrap();
    assert!(
        store
            .cache_content(&binding, &later, b"new server text")
            .await
            .unwrap()
    );
    assert!(
        store
            .list_outbox(&binding, 0, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let page = store.search_candidates(&binding, None, 100).await.unwrap();
    let candidate = page
        .candidates
        .iter()
        .find(|candidate| candidate.node_id == later.node_id)
        .unwrap();
    assert_eq!(candidate.source, SearchSource::Draft);
    assert_eq!(candidate.bytes(), b"base");
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 1);
    assert_eq!(draft.bytes, b"base");
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(draft.base_revision, "content-1");
}

#[tokio::test]
async fn acknowledging_a_matching_checkpoint_invalidates_previous_search_candidate() {
    let (_directory, mut store, binding, _root, _node) = seeded().await;
    let opened = store
        .open_registered_document(&binding, DOCUMENT, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&binding, DOCUMENT, &opened.edit_session_id, 1, b"base")
        .await
        .unwrap();
    store
        .close_registered_document(&binding, DOCUMENT, &opened.edit_session_id)
        .await
        .unwrap();
    store.queue_checkpoint(&binding, DOCUMENT, 1).await.unwrap();
    let operation = store.prepare_outbox(&binding).await.unwrap().unwrap();
    store
        .apply_outbox_commit(&binding, &committed(&operation, "content-1"))
        .await
        .unwrap();
    let before = store
        .search_candidates(&binding, None, 100)
        .await
        .unwrap()
        .candidates
        .remove(0);
    assert_eq!(before.source, SearchSource::Draft);
    let ack = serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": "1", "retired_through": "1"
    }))
    .unwrap();
    store.confirm_receipt_ack(&binding, &ack).await.unwrap();
    assert!(
        !store
            .search_candidate_is_current(&binding, &before)
            .await
            .unwrap()
    );
    let after = store
        .search_candidates(&binding, None, 100)
        .await
        .unwrap()
        .candidates
        .remove(0);
    assert_eq!(after.bytes(), b"base");
    assert!(
        store
            .search_candidate_is_current(&binding, &after)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn repeated_ack_without_changes_keeps_search_mutation_token() {
    let (_directory, mut store, binding, node, _first, _second) =
        two_commits_with_retired_first().await;
    let page = store.search_candidates(&binding, None, 100).await.unwrap();
    let candidate = page
        .candidates
        .iter()
        .find(|candidate| candidate.node_id == node.node_id)
        .unwrap();
    let token = store.search_mutation_token().await.unwrap();
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    let outbox = store.list_outbox(&binding, 0, 100).await.unwrap();
    let ack = serde_json::from_value(json!({
        "workspace_epoch": binding.workspace_epoch,
        "device_stream_id": binding.device_stream_id,
        "last_seq": "2", "retired_through": "2"
    }))
    .unwrap();
    store.confirm_receipt_ack(&binding, &ack).await.unwrap();
    assert_eq!(
        store.search_mutation_token().await.unwrap(),
        token,
        "an unchanged ACK must not invalidate search"
    );
    assert!(
        store
            .search_candidate_is_current(&binding, candidate)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .read_registered_checkpoint(&binding, DOCUMENT)
            .await
            .unwrap()
            .unwrap(),
        draft
    );
    assert_eq!(store.list_outbox(&binding, 0, 100).await.unwrap(), outbox);
}

#[tokio::test]
async fn candidate_is_invalidated_by_new_draft_generation_and_remote_delete() {
    let (_directory, mut store, binding, mut root, mut node) = seeded().await;
    let opened = store
        .open_registered_document(&binding, DOCUMENT, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            DOCUMENT,
            &opened.edit_session_id,
            1,
            "기기 초안".as_bytes(),
        )
        .await
        .unwrap();
    store
        .close_registered_document(&binding, DOCUMENT, &opened.edit_session_id)
        .await
        .unwrap();
    let operation = store.queue_checkpoint(&binding, DOCUMENT, 1).await.unwrap();
    let candidate = store
        .search_candidates(&binding, None, 100)
        .await
        .unwrap()
        .candidates
        .remove(0);
    assert_eq!(candidate.source, SearchSource::Draft);
    assert_eq!(candidate.scope, SearchScope::Workspace);
    assert!(
        store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .unwrap()
    );

    let reopened = store
        .open_registered_document(&binding, DOCUMENT, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &binding,
            DOCUMENT,
            &reopened.edit_session_id,
            2,
            "기기 초안".as_bytes(),
        )
        .await
        .unwrap();
    store
        .close_registered_document(&binding, DOCUMENT, &reopened.edit_session_id)
        .await
        .unwrap();
    assert!(
        !store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .unwrap()
    );

    let candidate = store
        .search_candidates(&binding, None, 100)
        .await
        .unwrap()
        .candidates
        .remove(0);
    root.children_generation = ChangeSeq::new(2);
    root.last_change_seq = ChangeSeq::new(2);
    node.deleted = true;
    node.last_change_seq = ChangeSeq::new(2);
    node.content_rev = Some("content-2".to_owned().try_into().unwrap());
    node.content_etag = Some("\"content-2\"".into());
    node.meta_rev = "meta-2".to_owned().try_into().unwrap();
    node.meta_etag = "\"meta-2\"".into();
    node.node_etag = "\"node-2\"".into();
    store
        .apply_changes(
            &binding,
            &page(&binding, vec![root, node]),
            ChangeSeq::new(2),
        )
        .await
        .unwrap();
    assert!(
        !store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .unwrap()
    );
    let recovery = store
        .search_candidates(&binding, None, 100)
        .await
        .unwrap()
        .candidates
        .remove(0);
    assert_eq!(recovery.scope, SearchScope::Recovery);
    assert!(
        store
            .search_candidate_is_current(&binding, &recovery)
            .await
            .unwrap()
    );
    let draft = store
        .read_registered_checkpoint(&binding, DOCUMENT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.generation, 2);
    assert_eq!(draft.bytes, "기기 초안".as_bytes());
    assert_eq!(draft.base_bytes, b"base");
    assert_eq!(
        store
            .read_outbox_operation(&binding, &operation.operation_id)
            .await
            .unwrap(),
        Some(operation)
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn cached_candidate_is_invalidated_by_new_remote_revision_and_registration() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let (directory, mut store, binding, _root, mut node) = seeded().await;
    let candidate = store
        .search_candidates(&binding, None, 100)
        .await
        .unwrap()
        .candidates
        .remove(0);
    assert_eq!(candidate.source, SearchSource::Cache);
    assert!(
        store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .unwrap()
    );
    let mut other = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE cached_content SET bytes=? WHERE node_id=?")
        .bind(b"bAse".as_slice())
        .bind(DOCUMENT)
        .execute(&mut other)
        .await
        .unwrap();
    assert!(
        !store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE cached_content SET bytes=? WHERE node_id=?")
        .bind(b"base".as_slice())
        .bind(DOCUMENT)
        .execute(&mut other)
        .await
        .unwrap();
    assert!(
        store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .unwrap()
    );
    other.close().await.unwrap();
    node.last_change_seq = ChangeSeq::new(2);
    node.content_rev = Some("content-2".to_owned().try_into().unwrap());
    node.content_etag = Some("\"content-2\"".into());
    node.node_etag = "\"node-2\"".into();
    node.content_hash = Some(digest(b"new"));
    node.size = Some("3".into());
    store
        .apply_changes(&binding, &page(&binding, vec![node]), ChangeSeq::new(2))
        .await
        .unwrap();
    assert!(
        !store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .unwrap()
    );

    let mut replacement = binding.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    assert!(
        store
            .search_candidate_is_current(&binding, &candidate)
            .await
            .is_err()
    );
    store.close().await.unwrap();
}
