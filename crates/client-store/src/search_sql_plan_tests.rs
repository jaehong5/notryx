use super::*;
use notryx_protocol::NodeState;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";

fn document_id(index: usize) -> String {
    format!("019a1000-0000-7000-8001-{index:012x}")
}

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

fn root() -> NodeState {
    serde_json::from_value(json!({
        "node_id": ROOT, "parent_id": null, "name": "", "kind": "folder", "deleted": false,
        "content_rev": null, "content_etag": null, "meta_rev": "root-meta",
        "meta_etag": "\"root-meta\"", "node_etag": "\"root-node\"", "content_hash": null,
        "size": null, "children_generation": "1", "last_change_seq": "1"
    }))
    .unwrap()
}

fn file(id: &str, name: &str, bytes: &[u8], deleted: bool) -> NodeState {
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    serde_json::from_value(json!({
        "node_id": id, "parent_id": ROOT, "name": name, "kind": "file", "deleted": deleted,
        "content_rev": "content-1", "content_etag": "\"content-1\"", "meta_rev": "meta-1",
        "meta_etag": "\"meta-1\"", "node_etag": "\"node-1\"", "content_hash": format!("sha256:{digest}"),
        "size": bytes.len().to_string(), "children_generation": "0", "last_change_seq": "1"
    })).unwrap()
}

async fn empty_store() -> (tempfile::TempDir, ClientStore, RegistrationBinding, i64) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("drafts.db"))
        .await
        .unwrap();
    let binding = binding();
    store.activate_registration(&binding).await.unwrap();
    let registration: i64 =
        sqlx::query_scalar("SELECT id FROM client_registrations WHERE status='active'")
            .fetch_one(&mut store.connection)
            .await
            .unwrap();
    (directory, store, binding, registration)
}

async fn insert_index(store: &mut ClientStore, registration: i64, nodes: &[NodeState]) {
    let mut tx = store.connection.begin().await.unwrap();
    sqlx::query("INSERT INTO remote_index(registration_id, root_id, cursor) VALUES(?,?,?)")
        .bind(registration)
        .bind(ROOT)
        .bind("1")
        .execute(&mut *tx)
        .await
        .unwrap();
    for node in nodes {
        sqlx::query("INSERT INTO remote_nodes(registration_id, node_id, state, content_ready) VALUES(?,?,?,0)")
            .bind(registration).bind(node.node_id.to_string()).bind(serde_json::to_string(node).unwrap())
            .execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
}

async fn scale_fixture(
    documents: usize,
    sparse_registrations: bool,
) -> (tempfile::TempDir, ClientStore, RegistrationBinding) {
    let (directory, mut store, binding, registration) = empty_store().await;
    let mut tx = store.connection.begin().await.unwrap();
    sqlx::query("INSERT INTO remote_index(registration_id, root_id, cursor) VALUES(?,?,?)")
        .bind(registration)
        .bind(ROOT)
        .bind("1")
        .execute(&mut *tx)
        .await
        .unwrap();
    let root = root();
    sqlx::query(
        "INSERT INTO remote_nodes(registration_id, node_id, state, content_ready) VALUES(?,?,?,0)",
    )
    .bind(registration)
    .bind(ROOT)
    .bind(serde_json::to_string(&root).unwrap())
    .execute(&mut *tx)
    .await
    .unwrap();
    for index in 1..=documents {
        let id = document_id(index);
        let node = file(&id, "synthetic.md", b"", false);
        sqlx::query("INSERT INTO remote_nodes(registration_id, node_id, state, content_ready) VALUES(?,?,?,0)")
            .bind(registration).bind(&id).bind(serde_json::to_string(&node).unwrap()).execute(&mut *tx).await.unwrap();
        if sparse_registrations {
            sqlx::query("INSERT INTO registered_documents(registration_id, document_id, storage_id) VALUES(?,?,?)")
                .bind(registration).bind(&id).bind(format!("r_{index:032x}"))
                .execute(&mut *tx).await.unwrap();
        }
    }
    tx.commit().await.unwrap();
    (directory, store, binding)
}

async fn first_page_vm_steps(documents: usize, sparse: bool) -> u64 {
    let (_directory, mut store, binding) = scale_fixture(documents, sparse).await;
    let calls = Arc::new(AtomicU64::new(0));
    let observed = calls.clone();
    {
        let mut handle = store.connection.lock_handle().await.unwrap();
        handle.set_progress_handler(100, move || {
            observed.fetch_add(1, Ordering::Relaxed);
            true
        });
    }
    let page = store.search_candidates(&binding, None, 1).await.unwrap();
    {
        let mut handle = store.connection.lock_handle().await.unwrap();
        handle.remove_progress_handler();
    }
    let steps = calls.load(Ordering::Relaxed) * 100;
    assert!(
        steps > 0,
        "the progress handler must observe real SQLite work"
    );
    assert!(page.candidates.is_empty());
    assert_eq!((page.available_documents, page.total_documents), (0, 0));
    assert_eq!(
        (
            page.recovery_available_documents,
            page.recovery_total_documents
        ),
        (0, 0)
    );
    assert_eq!(page.next_after_id, Some(ROOT.parse().unwrap()));
    store.close().await.unwrap();
    steps
}

#[tokio::test]
async fn actual_search_sql_query_plan_is_observable() {
    let (_directory, mut store, _binding) = scale_fixture(32, true).await;
    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&mut store.connection)
        .await
        .unwrap();
    let registration: i64 =
        sqlx::query_scalar("SELECT id FROM client_registrations WHERE status='active'")
            .fetch_one(&mut store.connection)
            .await
            .unwrap();
    let statement = format!("EXPLAIN QUERY PLAN {SEARCH_CANDIDATES_SQL}");
    let rows = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(registration)
        .bind("")
        .bind(2i64)
        .fetch_all(&mut store.connection)
        .await
        .unwrap();
    let details: Vec<String> = rows
        .iter()
        .map(|row| row.try_get("detail").unwrap())
        .collect();
    assert!(!version.is_empty());
    assert!(!details.is_empty());
    println!("product SQLx SQLite {version}; actual search SQL plan: {details:#?}");
    store.close().await.unwrap();
}

#[tokio::test]
async fn first_search_page_vm_work_is_bounded_as_metadata_grows() {
    let small = first_page_vm_steps(1_000, false).await;
    let large = first_page_vm_steps(10_000, false).await;
    println!(
        "approximate product SQLx SQLite VM instructions, metadata-only: 1000={small}, 10000={large}"
    );
    assert!(
        large < small.saturating_mul(4),
        "first-page work must grow less than fourfold from 1,000 to 10,000 documents"
    );
}

#[tokio::test]
async fn first_search_page_vm_work_is_bounded_with_sparse_registered_documents() {
    let small = first_page_vm_steps(1_000, true).await;
    let large = first_page_vm_steps(10_000, true).await;
    println!(
        "approximate product SQLx SQLite VM instructions, registered documents without checkpoints: 1000={small}, 10000={large}"
    );
    assert!(
        large < small.saturating_mul(4),
        "sparse registered documents must not cause unbounded first-page work"
    );
}

#[tokio::test]
async fn ordered_pages_preserve_union_and_coverage_for_workspace_and_recovery() {
    let (_directory, mut store, binding, registration) = empty_store().await;
    let live_id = document_id(1);
    let deleted_id = document_id(2);
    let unlisted_id = document_id(3);
    let attachment_id = document_id(4);
    let missing_id = document_id(5);
    let registered_id = document_id(6);
    let live = file(&live_id, "live.md", b"server", false);
    let deleted = file(&deleted_id, "deleted.md", b"deleted base", true);
    let attachment = file(&attachment_id, "attachment.txt", b"attachment", false);
    let missing = file(&missing_id, "missing.md", b"missing", false);
    let registered = file(&registered_id, "registered.md", b"registered", false);
    insert_index(
        &mut store,
        registration,
        &[
            root(),
            live.clone(),
            deleted,
            attachment,
            missing,
            registered,
        ],
    )
    .await;
    assert!(
        store
            .cache_content(&binding, &live, b"server")
            .await
            .unwrap()
    );
    for (id, base, draft) in [
        (&live_id, b"server".as_slice(), b"local live".as_slice()),
        (
            &deleted_id,
            b"deleted base".as_slice(),
            b"deleted draft".as_slice(),
        ),
        (
            &unlisted_id,
            b"unlisted base".as_slice(),
            b"unlisted draft".as_slice(),
        ),
    ] {
        let opened = store
            .open_registered_document(&binding, id, base, "content-1")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, id, &opened.edit_session_id, 1, draft)
            .await
            .unwrap();
        store
            .close_registered_document(&binding, id, &opened.edit_session_id)
            .await
            .unwrap();
    }
    let opened = store
        .open_registered_document(&binding, &registered_id, b"registered", "content-1")
        .await
        .unwrap();
    store
        .close_registered_document(&binding, &registered_id, &opened.edit_session_id)
        .await
        .unwrap();

    let expected = vec![
        (
            live_id,
            SearchSource::Draft,
            SearchScope::Workspace,
            b"local live".to_vec(),
        ),
        (
            deleted_id,
            SearchSource::Draft,
            SearchScope::Recovery,
            b"deleted draft".to_vec(),
        ),
        (
            unlisted_id,
            SearchSource::Draft,
            SearchScope::Recovery,
            b"unlisted draft".to_vec(),
        ),
    ];
    for limit in [1, 2, 3, 100] {
        let mut after = None;
        let mut candidates = Vec::new();
        let mut totals = (0, 0, 0, 0);
        for _ in 0..16 {
            let page = store
                .search_candidates(&binding, after, limit)
                .await
                .unwrap();
            totals.0 += page.available_documents;
            totals.1 += page.total_documents;
            totals.2 += page.recovery_available_documents;
            totals.3 += page.recovery_total_documents;
            candidates.extend(page.candidates.into_iter().map(|candidate| {
                (
                    candidate.node_id.to_string(),
                    candidate.source,
                    candidate.scope,
                    candidate.bytes().to_vec(),
                )
            }));
            if let (Some(previous), Some(next)) = (after, page.next_after_id) {
                assert!(next.to_string() > previous.to_string());
            }
            after = page.next_after_id;
            if after.is_none() {
                break;
            }
        }
        assert!(after.is_none(), "all pages must terminate");
        assert_eq!(
            candidates, expected,
            "candidate source, scope and bytes must match the independent expected result"
        );
        assert_eq!(totals, (1, 3, 2, 2));
    }
    store.close().await.unwrap();
}
