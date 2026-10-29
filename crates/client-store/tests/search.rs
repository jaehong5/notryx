use notryx_client_store::{
    ClientStore, RegistrationBinding, SearchScope, SearchSource, search_contains,
    search_contains_cancellable,
};
use notryx_protocol::{ChangesPage, InventoryPage, NodeState, ServerInfo};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const DRAFT: &str = "019a1000-0000-7000-8000-000000000002";
const CACHED: &str = "019a1000-0000-7000-8000-000000000003";
const MISSING: &str = "019a1000-0000-7000-8000-000000000004";
const ATTACHMENT: &str = "019a1000-0000-7000-8000-000000000005";

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn file(id: &str, name: &str, bytes: &[u8]) -> NodeState {
    let digest = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    serde_json::from_value(json!({
        "node_id": id, "parent_id": ROOT, "name": name, "kind": "file",
        "deleted": false, "content_rev": "content-1", "content_etag": "\"content-1\"",
        "meta_rev": "meta-1", "meta_etag": "\"meta-1\"", "node_etag": "\"node-1\"",
        "content_hash": format!("sha256:{digest}"), "size": bytes.len().to_string(),
        "children_generation": "0", "last_change_seq": "1"
    }))
    .unwrap()
}

#[tokio::test]
async fn local_search_uses_draft_before_cache_and_reports_partial_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let info: ServerInfo = serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2", "workspace_id": registration.workspace_id,
        "workspace_epoch": registration.workspace_epoch, "root_id": ROOT,
        "head_seq": "1", "journal_floor_seq": "0", "ready": true,
        "max_document_bytes": 5242880, "max_attachment_bytes": 26214400
    }))
    .unwrap();
    let root: NodeState = serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder",
        "deleted": false, "content_rev": null, "content_etag": null,
        "meta_rev": "root-meta", "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"",
        "content_hash": null, "size": null, "children_generation": "1", "last_change_seq": "1"
    }))
    .unwrap();
    let cached_draft = "서버의 가나다".as_bytes();
    let cached_other = "가나다 100%_기록".as_bytes();
    let draft = "기기 초안 가나".as_bytes();
    let draft_node = file(DRAFT, "draft.md", cached_draft);
    let cached_node = file(CACHED, "cached.md", cached_other);
    let missing_node = file(MISSING, "missing.md", "미캐시 본문".as_bytes());
    let attachment_node = file(ATTACHMENT, "attachment.txt", "가나다".as_bytes());

    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store.begin_bootstrap(&registration, &info).await.unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &InventoryPage {
                workspace_epoch: info.workspace_epoch,
                nodes: vec![
                    root,
                    draft_node.clone(),
                    cached_node.clone(),
                    missing_node,
                    attachment_node.clone(),
                ],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info)
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info)
        .await
        .unwrap();
    for (node, bytes) in [
        (&draft_node, cached_draft),
        (&cached_node, cached_other),
        (&attachment_node, "가나다".as_bytes()),
    ] {
        assert!(
            store
                .cache_content(&registration, node, bytes)
                .await
                .unwrap()
        );
    }
    let opened = store
        .open_registered_document(&registration, DRAFT, cached_draft, "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(&registration, DRAFT, &opened.edit_session_id, 1, draft)
        .await
        .unwrap();
    store
        .close_registered_document(&registration, DRAFT, &opened.edit_session_id)
        .await
        .unwrap();
    let operation = store
        .queue_checkpoint(&registration, DRAFT, 1)
        .await
        .unwrap();

    let result = store
        .search_candidates(&registration, None, 100)
        .await
        .unwrap();
    let matches: BTreeMap<_, _> = result
        .candidates
        .iter()
        .filter(|item| search_contains(item.bytes(), "가나"))
        .map(|item| (item.node_id.to_string(), item.source))
        .collect();
    assert_eq!(matches.get(DRAFT), Some(&SearchSource::Draft));
    assert_eq!(matches.get(CACHED), Some(&SearchSource::Cache));
    assert_eq!(matches.len(), 2);
    assert_eq!(result.available_documents, 2);
    assert_eq!(result.total_documents, 3);
    assert!(result.available_documents < result.total_documents);
    assert!(result.next_after_id.is_none());

    let mut after = None;
    let mut all = BTreeMap::new();
    let mut total = 0;
    let mut available = 0;
    for _ in 0..10 {
        let page = store
            .search_candidates(&registration, after, 1)
            .await
            .unwrap();
        total += page.total_documents;
        available += page.available_documents;
        for candidate in page.candidates {
            assert!(
                all.insert(candidate.node_id.to_string(), candidate.source)
                    .is_none()
            );
        }
        after = page.next_after_id;
        if after.is_none() {
            break;
        }
    }
    assert!(after.is_none());
    assert_eq!((total, available), (3, 2));
    assert_eq!(all.get(DRAFT), Some(&SearchSource::Draft));
    assert_eq!(all.get(CACHED), Some(&SearchSource::Cache));
    assert!(
        store
            .search_candidates(&registration, None, 0)
            .await
            .is_err()
    );

    for query in ["가나다", "%_"] {
        let matches: Vec<_> = result
            .candidates
            .iter()
            .filter(|item| search_contains(item.bytes(), query))
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].node_id.to_string(), CACHED);
    }

    let saved = store
        .read_registered_checkpoint(&registration, DRAFT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bytes, draft);
    assert_eq!(saved.base_bytes, cached_draft);
    assert_eq!(saved.base_revision, "content-1");
    assert_eq!(
        store
            .read_outbox_operation(&registration, &operation.operation_id)
            .await
            .unwrap(),
        Some(operation)
    );
    assert_eq!(
        store
            .read_cached_content(&registration, draft_node.node_id)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        cached_draft
    );

    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let mut other = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE cached_content SET content_rev=? WHERE node_id=?")
        .bind("older-revision")
        .bind(CACHED)
        .execute(&mut other)
        .await
        .unwrap();
    let page = store
        .search_candidates(&registration, None, 100)
        .await
        .unwrap();
    assert_eq!((page.total_documents, page.available_documents), (3, 1));
    assert!(
        page.candidates
            .iter()
            .all(|candidate| candidate.node_id.to_string() != CACHED)
    );
    let mut tampered = cached_other.to_vec();
    *tampered.last_mut().unwrap() ^= 1;
    sqlx::query("UPDATE cached_content SET content_rev=?,bytes=? WHERE node_id=?")
        .bind("content-1")
        .bind(tampered)
        .bind(CACHED)
        .execute(&mut other)
        .await
        .unwrap();
    let page = store
        .search_candidates(&registration, None, 100)
        .await
        .unwrap();
    assert_eq!((page.total_documents, page.available_documents), (3, 1));
    assert!(
        page.candidates
            .iter()
            .all(|candidate| candidate.node_id.to_string() != CACHED)
    );
    other.close().await.unwrap();
    let replacement = binding();
    store.activate_registration(&replacement).await.unwrap();
    assert!(
        store
            .search_candidates(&registration, None, 100)
            .await
            .is_err()
    );
    store.close().await.unwrap();
}

#[test]
fn literal_nfc_matching_handles_korean_and_invalid_source() {
    assert!(search_contains("한가나글".as_bytes(), "가나"));
    assert!(search_contains("가나다".as_bytes(), "나다"));
    assert!(search_contains(b"abababa 100%_", "ababa"));
    assert!(search_contains(b"100%_", "%_"));
    assert!(!search_contains(b"100xy", "%_"));
    assert!(!search_contains(&[0xff], "x"));
    assert!(!search_contains(b"text", ""));
}

#[tokio::test]
async fn cancellable_literal_matching_preserves_nfc_across_yield_boundaries() {
    let cancelled = AtomicBool::new(false);
    let sources = [
        "e\u{301} K 각 가나 100%_".to_owned(),
        format!("{}e\u{301}각a\u{315}\u{300}x", "x".repeat(4095)),
        format!("{}각{}", "ab".repeat(4096), "z".repeat(4096)),
    ];
    let queries = [
        "é",
        "e",
        "K",
        "각",
        "각",
        "가나",
        "a\u{300}\u{315}",
        "%_",
        "missing",
        "xé각",
        "",
    ];
    for source in &sources {
        let before = source.as_bytes().to_vec();
        let normalized: String = source.nfc().collect();
        for query in queries {
            let pattern: String = query.nfc().collect();
            let expected = !pattern.is_empty() && normalized.contains(&pattern);
            assert_eq!(search_contains(source.as_bytes(), query), expected);
            assert_eq!(
                search_contains_cancellable(source.as_bytes(), query, &cancelled).await,
                Some(expected)
            );
        }
        assert_eq!(source.as_bytes(), before);
    }
    assert_eq!(
        search_contains_cancellable(&[0xff], "x", &cancelled).await,
        Some(false)
    );
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        search_contains_cancellable(b"text", "x", &cancelled).await,
        None
    );
}

#[tokio::test]
async fn search_candidate_pages_limit_total_bytes_and_resume_without_skips() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let info: ServerInfo = serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2", "workspace_id": registration.workspace_id,
        "workspace_epoch": registration.workspace_epoch, "root_id": ROOT,
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
    let bytes = vec![b'x'; 13 * 1024 * 1024];
    let first = file(DRAFT, "first.md", &bytes);
    let second = file(CACHED, "second.md", &bytes);
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store.begin_bootstrap(&registration, &info).await.unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &InventoryPage {
                workspace_epoch: info.workspace_epoch,
                nodes: vec![root, first.clone(), second.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info)
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info)
        .await
        .unwrap();
    for node in [&first, &second] {
        assert!(
            store
                .cache_content(&registration, node, &bytes)
                .await
                .unwrap()
        );
    }
    let page = store
        .search_candidates(&registration, None, 100)
        .await
        .unwrap();
    assert_eq!(page.candidates.len(), 1);
    assert_eq!(page.candidates[0].node_id, first.node_id);
    assert!(
        page.candidates
            .iter()
            .map(|item| item.bytes().len())
            .sum::<usize>()
            <= 25 * 1024 * 1024
    );
    let next = page
        .next_after_id
        .expect("remaining document must have a cursor");
    let page = store
        .search_candidates(&registration, Some(next), 100)
        .await
        .unwrap();
    assert_eq!(page.candidates.len(), 1);
    assert_eq!(page.candidates[0].node_id, second.node_id);
    assert!(page.next_after_id.is_none());

    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let mut other = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
    )
    .await
    .unwrap();
    let mut corrupted = bytes.clone();
    corrupted[0] = b'y';
    sqlx::query("UPDATE cached_content SET bytes=? WHERE node_id=?")
        .bind(corrupted)
        .bind(DRAFT)
        .execute(&mut other)
        .await
        .unwrap();
    let page = store
        .search_candidates(&registration, None, 100)
        .await
        .unwrap();
    assert!(page.candidates.is_empty());
    assert_eq!((page.total_documents, page.available_documents), (1, 0));
    let next = page
        .next_after_id
        .expect("validated bytes must count toward the page budget");
    assert_eq!(next, first.node_id);
    let page = store
        .search_candidates(&registration, Some(next), 100)
        .await
        .unwrap();
    assert_eq!(page.candidates.len(), 1);
    assert_eq!(page.candidates[0].node_id, second.node_id);
    assert!(page.next_after_id.is_none());
    other.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn deleted_and_unlisted_dirty_drafts_are_separate_from_live_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let registration = binding();
    let info: ServerInfo = serde_json::from_value(json!({
        "protocol": "mdw-sync/1.2", "workspace_id": registration.workspace_id,
        "workspace_epoch": registration.workspace_epoch, "root_id": ROOT,
        "head_seq": "1", "journal_floor_seq": "0", "ready": true,
        "max_document_bytes": 5242880, "max_attachment_bytes": 26214400
    }))
    .unwrap();
    let mut root: NodeState = serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder", "deleted": false,
        "content_rev": null, "content_etag": null, "meta_rev": "root-meta",
        "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"", "content_hash": null,
        "size": null, "children_generation": "1", "last_change_seq": "1"
    }))
    .unwrap();
    let node = file(DRAFT, "draft.md", b"base");
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    store.activate_registration(&registration).await.unwrap();
    let session = store.begin_bootstrap(&registration, &info).await.unwrap();
    store
        .bootstrap_inventory(
            &registration,
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
        .bootstrap_finish_inventory(&registration, &session, &info)
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info)
        .await
        .unwrap();
    let opened = store
        .open_registered_document(&registration, DRAFT, b"base", "content-1")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &registration,
            DRAFT,
            &opened.edit_session_id,
            1,
            "보존할 초안".as_bytes(),
        )
        .await
        .unwrap();
    store
        .close_registered_document(&registration, DRAFT, &opened.edit_session_id)
        .await
        .unwrap();
    let operation = store
        .queue_checkpoint(&registration, DRAFT, 1)
        .await
        .unwrap();

    root.children_generation = notryx_domain::ChangeSeq::new(2);
    root.last_change_seq = notryx_domain::ChangeSeq::new(2);
    let mut deleted = node.clone();
    deleted.deleted = true;
    deleted.last_change_seq = notryx_domain::ChangeSeq::new(2);
    deleted.content_rev = Some("content-2".to_owned().try_into().unwrap());
    deleted.content_etag = Some("\"content-2\"".into());
    deleted.meta_rev = "meta-2".to_owned().try_into().unwrap();
    deleted.meta_etag = "\"meta-2\"".into();
    deleted.node_etag = "\"node-2\"".into();
    let changes: ChangesPage = serde_json::from_value(json!({
        "workspace_epoch": registration.workspace_epoch, "from_exclusive": "1", "through_seq": "2",
        "covered_through": "2", "has_more": false, "nodes": [root.clone(), deleted]
    }))
    .unwrap();
    store
        .apply_changes(&registration, &changes, notryx_domain::ChangeSeq::new(2))
        .await
        .unwrap();

    let page = store
        .search_candidates(&registration, None, 100)
        .await
        .unwrap();
    assert_eq!((page.total_documents, page.available_documents), (0, 0));
    assert_eq!(
        (
            page.recovery_total_documents,
            page.recovery_available_documents
        ),
        (1, 1)
    );
    assert_eq!(page.candidates.len(), 1);
    assert_eq!(page.candidates[0].node_id, node.node_id);
    assert_eq!(page.candidates[0].scope, SearchScope::Recovery);
    assert_eq!(page.candidates[0].source, SearchSource::Draft);
    assert!(search_contains(page.candidates[0].bytes(), "초안"));

    let mut info2 = info.clone();
    info2.head_seq = notryx_domain::ChangeSeq::new(2);
    let session = store.begin_bootstrap(&registration, &info2).await.unwrap();
    store
        .bootstrap_inventory(
            &registration,
            &session,
            &InventoryPage {
                workspace_epoch: info2.workspace_epoch,
                nodes: vec![root.clone()],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&registration, &session, &info2)
        .await
        .unwrap();
    store
        .bootstrap_publish(&registration, &session, &info2)
        .await
        .unwrap();
    let page = store
        .search_candidates(&registration, None, 100)
        .await
        .unwrap();
    assert_eq!((page.total_documents, page.available_documents), (0, 0));
    assert_eq!(
        (
            page.recovery_total_documents,
            page.recovery_available_documents
        ),
        (1, 1)
    );
    assert_eq!(page.candidates.len(), 1);
    assert_eq!(page.candidates[0].node_id, node.node_id);
    assert_eq!(page.candidates[0].scope, SearchScope::Recovery);

    let mut replacement = registration.clone();
    replacement.device_id = Uuid::new_v4().to_string();
    replacement.device_stream_id = Uuid::new_v4().to_string();
    store.activate_registration(&replacement).await.unwrap();
    let session = store.begin_bootstrap(&replacement, &info2).await.unwrap();
    store
        .bootstrap_inventory(
            &replacement,
            &session,
            &InventoryPage {
                workspace_epoch: info2.workspace_epoch,
                nodes: vec![root],
                next_after_id: None,
            },
        )
        .await
        .unwrap();
    store
        .bootstrap_finish_inventory(&replacement, &session, &info2)
        .await
        .unwrap();
    store
        .bootstrap_publish(&replacement, &session, &info2)
        .await
        .unwrap();
    let page = store
        .search_candidates(&replacement, None, 100)
        .await
        .unwrap();
    assert_eq!(
        (
            page.recovery_total_documents,
            page.recovery_available_documents
        ),
        (0, 0)
    );
    assert!(page.candidates.is_empty());
    assert_eq!(
        store
            .read_registered_checkpoint(&registration, DRAFT)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        "보존할 초안".as_bytes()
    );
    let mut frozen_operation = operation.clone();
    frozen_operation.frozen = true;
    assert_eq!(
        store
            .read_outbox_operation(&registration, &operation.operation_id)
            .await
            .unwrap(),
        Some(frozen_operation)
    );
    store.close().await.unwrap();
}
