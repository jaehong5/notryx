use super::*;
use notryx_protocol::{InventoryPage, ServerInfo};
use serde_json::json;
use std::hint::black_box;
use std::time::Instant;
use uuid::Uuid;

const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
const DOCUMENT: &str = "019a1000-0000-7000-8000-000000000002";
const REPEATS: usize = 3;

fn record(size: usize, source: &str, stage: &str, start: Instant) {
    println!(
        "size={size} source={source} stage={stage} elapsed_us={}",
        start.elapsed().as_micros()
    );
}

fn synthetic_bytes(size: usize) -> Vec<u8> {
    let mut bytes = vec![b'x'; size];
    let prefix = "시작 각가 나다 ".as_bytes();
    let suffix = " 마침".as_bytes();
    bytes[..prefix.len()].copy_from_slice(prefix);
    bytes[size - suffix.len()..].copy_from_slice(suffix);
    bytes
}

async fn fixture(bytes: &[u8]) -> (tempfile::TempDir, ClientStore, RegistrationBinding) {
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
        "node_id": DOCUMENT, "parent_id": ROOT, "name": "synthetic.md", "kind": "file", "deleted": false,
        "content_rev": "content-1", "content_etag": "\"content-1\"", "meta_rev": "meta-1",
        "meta_etag": "\"meta-1\"", "node_etag": "\"node-1\"", "content_hash": hash(bytes),
        "size": bytes.len().to_string(), "children_generation": "0", "last_change_seq": "1"
    }))
    .unwrap();
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
                nodes: vec![root, node.clone()],
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
    assert!(store.cache_content(&binding, &node, bytes).await.unwrap());
    (directory, store, binding)
}

async fn profile_case(
    store: &mut ClientStore,
    binding: &RegistrationBinding,
    expected: &[u8],
    source: SearchSource,
) {
    let size = expected.len();
    let label = match source {
        SearchSource::Cache => "cache",
        SearchSource::Draft => "dirty_draft",
    };
    let expected_hash = hash(expected);
    let cancelled = AtomicBool::new(false);
    let mut observer = store.search_observer().unwrap();
    let after = Some(ROOT.parse().unwrap());
    let mut candidate = None;
    for iteration in 0..REPEATS {
        let start = Instant::now();
        let page = observer
            .observe(binding, after, &cancelled)
            .await
            .unwrap()
            .unwrap();
        record(
            size,
            label,
            if iteration == 0 {
                "observer_first_connection"
            } else {
                "observer_reused_connection"
            },
            start,
        );
        assert_eq!((page.available_documents, page.total_documents), (1, 1));
        assert_eq!(
            (
                page.recovery_available_documents,
                page.recovery_total_documents
            ),
            (0, 0)
        );
        assert!(page.next_after_id.is_none());
        assert_eq!(page.candidates.len(), 1);
        let observed = page.candidates.into_iter().next().unwrap();
        assert_eq!(observed.node_id.to_string(), DOCUMENT);
        assert_eq!(observed.source, source);
        assert_eq!(observed.scope, SearchScope::Workspace);
        assert!(
            observed.bytes() == expected,
            "observed bytes differ from fixture"
        );
        assert_eq!(hash(observed.bytes()), expected_hash);
        candidate = Some(observed);
    }
    let candidate = candidate.unwrap();
    let reader = observer.reader.as_mut().unwrap();
    let registration = reader.registration_id(binding, true).await.unwrap();
    let storage_id: Option<String> = sqlx::query_scalar(
        "SELECT storage_id FROM registered_documents WHERE registration_id=? AND document_id=?",
    )
    .bind(registration)
    .bind(DOCUMENT)
    .fetch_optional(&mut reader.connection)
    .await
    .unwrap();
    for _ in 0..REPEATS {
        let start = Instant::now();
        let rows = sqlx::query(SEARCH_CANDIDATES_SQL)
            .bind(registration)
            .bind(ROOT)
            .bind(2_i64)
            .fetch_all(&mut reader.connection)
            .await
            .unwrap();
        record(size, label, "candidate_sql", start);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("node_id"), DOCUMENT);
        let start = Instant::now();
        let bytes: Vec<u8> = match source {
            SearchSource::Cache => sqlx::query_scalar(
                "SELECT bytes FROM cached_content WHERE registration_id=? AND node_id=?",
            )
            .bind(registration)
            .bind(DOCUMENT)
            .fetch_one(&mut reader.connection)
            .await
            .unwrap(),
            SearchSource::Draft => {
                sqlx::query_scalar("SELECT bytes FROM draft_checkpoints WHERE document_id=?")
                    .bind(storage_id.as_ref().unwrap())
                    .fetch_one(&mut reader.connection)
                    .await
                    .unwrap()
            }
        };
        record(size, label, "bytes_select", start);
        assert!(bytes == expected, "selected bytes differ from fixture");
        let start = Instant::now();
        let current = reader
            .search_candidate_is_current(binding, &candidate)
            .await
            .unwrap();
        record(size, label, "candidate_current", start);
        assert!(current);
        let start = Instant::now();
        let digest = hash_with_hook(expected, Some(&cancelled), &mut None)
            .await
            .unwrap();
        record(
            size,
            label,
            if source == SearchSource::Draft {
                "hash_counterfactual_dirty_draft_cancel_checks_yield"
            } else {
                "hash_cache_cancel_checks_yield"
            },
            start,
        );
        assert_eq!(digest, expected_hash);
        let start = Instant::now();
        let valid = std::str::from_utf8(black_box(expected)).is_ok();
        record(size, label, "utf8", start);
        assert!(valid);
        for (name, query, matches) in [
            ("contains_early", "시작", true),
            ("contains_late", "마침", true),
            ("contains_absent", "없는표식", false),
            ("contains_nfc_query_nfd_source", "나다", true),
            ("contains_nfd_query_nfc_source", "각가", true),
        ] {
            let start = Instant::now();
            let actual = search_contains(black_box(expected), black_box(query));
            record(size, label, name, start);
            assert_eq!(actual, matches);
        }
    }
}

#[tokio::test]
#[ignore = "manual synthetic profiling; use an external timeout and a quiet isolated environment"]
async fn synthetic_search_stage_profile() {
    println!(
        "synthetic data only; three samples per case; build mode={}, arch={}, os={}",
        if cfg!(debug_assertions) {
            "debug assertions enabled"
        } else {
            "debug assertions disabled"
        },
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!(
        "Use an external timeout and record the device and exact build profile. Debug assertions do not determine the optimization level. Stages overlap: do not add or subtract them. The dirty draft hash is counterfactual: the observer skips that hash. This does not measure 10k documents, p95, UI paint, release performance, or the 100 ms gate."
    );
    for size in [32 * 1024, 5 * 1024 * 1024, 25 * 1024 * 1024] {
        let bytes = synthetic_bytes(size);
        let (_directory, mut store, binding) = fixture(&bytes).await;
        profile_case(&mut store, &binding, &bytes, SearchSource::Cache).await;
        if size <= 5 * 1024 * 1024 {
            let opened = store
                .open_registered_document(&binding, DOCUMENT, &bytes, "content-1")
                .await
                .unwrap();
            let mut draft = bytes.clone();
            draft[size / 2] = b'y';
            store
                .save_registered_checkpoint(&binding, DOCUMENT, &opened.edit_session_id, 1, &draft)
                .await
                .unwrap();
            profile_case(&mut store, &binding, &draft, SearchSource::Draft).await;
            store
                .close_registered_document(&binding, DOCUMENT, &opened.edit_session_id)
                .await
                .unwrap();
        }
        store.close().await.unwrap();
    }
}
