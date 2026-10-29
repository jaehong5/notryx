use notryx_client_store::{
    RegistrationBinding, SearchCandidate, SearchObserver, SearchScope, StoreError,
    search_contains_cancellable,
};
use notryx_domain::NodeId;
use notryx_protocol::{
    WorkspaceSearchPage, WorkspaceSearchRequest, WorkspaceSearchResult, WorkspaceSearchScope,
    WorkspaceSearchSource,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::RuntimeState;

const MAX_RESULTS: usize = 200;
const MAX_RESULT_BYTES: usize = 25 * 1024 * 1024;
const MAX_REQUEST_SEQUENCE: u64 = 9_007_199_254_740_991;

fn request_sequence(value: &str) -> Result<u64, String> {
    let sequence = value.parse::<u64>().map_err(|_| "invalid_search")?;
    if sequence == 0 || sequence > MAX_REQUEST_SEQUENCE || sequence.to_string() != value {
        return Err("invalid_search".into());
    }
    Ok(sequence)
}

pub(crate) struct SearchSession {
    owner: String,
    request_id: String,
    query: String,
    scope: SearchScope,
    binding: RegistrationBinding,
    mutation_token: (i64, i64),
    generation: u64,
    next_after_id: Option<NodeId>,
    observer: Option<SearchObserver>,
    cancelled: Arc<AtomicBool>,
}

struct SearchScan {
    matches: Vec<SearchCandidate>,
    available_documents: u64,
    total_documents: u64,
    recovery_available_documents: u64,
    recovery_total_documents: u64,
    next_after_id: Option<NodeId>,
}

#[cfg(test)]
async fn scan_page(
    observer: &mut SearchObserver,
    binding: &RegistrationBinding,
    query: &str,
    scope: SearchScope,
    after: Option<NodeId>,
    cancelled: &AtomicBool,
    slice: Duration,
) -> Result<Option<SearchScan>, StoreError> {
    scan_page_with_limits(
        observer,
        binding,
        query,
        scope,
        after,
        cancelled,
        slice,
        MAX_RESULTS,
        MAX_RESULT_BYTES,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn scan_page_with_limits(
    observer: &mut SearchObserver,
    binding: &RegistrationBinding,
    query: &str,
    scope: SearchScope,
    after: Option<NodeId>,
    cancelled: &AtomicBool,
    slice: Duration,
    max_results: usize,
    max_bytes: usize,
) -> Result<Option<SearchScan>, StoreError> {
    scan_page_with_match_hook(
        observer,
        binding,
        query,
        scope,
        after,
        cancelled,
        slice,
        max_results,
        max_bytes,
        |_| {},
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn scan_page_with_match_hook(
    observer: &mut SearchObserver,
    binding: &RegistrationBinding,
    query: &str,
    scope: SearchScope,
    after: Option<NodeId>,
    cancelled: &AtomicBool,
    slice: Duration,
    max_results: usize,
    max_bytes: usize,
    mut match_boundary: impl FnMut(bool) + Send,
) -> Result<Option<SearchScan>, StoreError> {
    let started = Instant::now();
    let mut cursor = after;
    let mut bytes = 0usize;
    let mut scan = SearchScan {
        matches: Vec::new(),
        available_documents: 0,
        total_documents: 0,
        recovery_available_documents: 0,
        recovery_total_documents: 0,
        next_after_id: None,
    };
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let Some(page) = observer.observe(binding, cursor, cancelled).await? else {
            return Ok(None);
        };
        let candidate = page.candidates.into_iter().next();
        let matched = if let Some(candidate) = candidate
            .as_ref()
            .filter(|candidate| candidate.scope == scope)
        {
            match_boundary(true);
            let matched = search_contains_cancellable(candidate.bytes(), query, cancelled).await;
            match_boundary(false);
            matched
        } else {
            Some(false)
        };
        let Some(matched) = matched else {
            return Ok(None);
        };
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        if matched {
            let candidate = candidate.expect("matched candidate exists");
            if candidate.bytes().len() > max_bytes.saturating_sub(bytes) {
                scan.next_after_id = cursor;
                return Ok(Some(scan));
            }
            bytes += candidate.bytes().len();
            scan.matches.push(candidate);
        }
        scan.available_documents += page.available_documents;
        scan.total_documents += page.total_documents;
        scan.recovery_available_documents += page.recovery_available_documents;
        scan.recovery_total_documents += page.recovery_total_documents;
        cursor = page.next_after_id;
        scan.next_after_id = cursor;
        if cursor.is_none() || scan.matches.len() == max_results || started.elapsed() >= slice {
            return Ok(Some(scan));
        }
        tokio::task::yield_now().await;
    }
}

async fn scan_complete(
    observer: &mut SearchObserver,
    binding: &RegistrationBinding,
    query: &str,
    scope: SearchScope,
    after: Option<NodeId>,
    cancelled: &AtomicBool,
    slice: Duration,
) -> Result<Option<SearchScan>, StoreError> {
    scan_complete_with_hook(
        observer,
        binding,
        query,
        scope,
        after,
        cancelled,
        slice,
        async {},
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn scan_complete_with_hook<F: std::future::Future<Output = ()>>(
    observer: &mut SearchObserver,
    binding: &RegistrationBinding,
    query: &str,
    scope: SearchScope,
    after: Option<NodeId>,
    cancelled: &AtomicBool,
    slice: Duration,
    first_slice: F,
) -> Result<Option<SearchScan>, StoreError> {
    let mut scan = SearchScan {
        matches: Vec::new(),
        available_documents: 0,
        total_documents: 0,
        recovery_available_documents: 0,
        recovery_total_documents: 0,
        next_after_id: after,
    };
    let mut retained_bytes = 0usize;
    let mut first_slice = Some(first_slice);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let previous_cursor = scan.next_after_id;
        let Some(page) = scan_page_with_limits(
            observer,
            binding,
            query,
            scope,
            previous_cursor,
            cancelled,
            slice,
            MAX_RESULTS - scan.matches.len(),
            MAX_RESULT_BYTES - retained_bytes,
        )
        .await?
        else {
            return Ok(None);
        };
        if let Some(hook) = first_slice.take() {
            hook.await;
        }
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        retained_bytes += page
            .matches
            .iter()
            .map(|candidate| candidate.bytes().len())
            .sum::<usize>();
        scan.matches.extend(page.matches);
        scan.available_documents += page.available_documents;
        scan.total_documents += page.total_documents;
        scan.recovery_available_documents += page.recovery_available_documents;
        scan.recovery_total_documents += page.recovery_total_documents;
        scan.next_after_id = page.next_after_id;
        if scan.next_after_id.is_none()
            || scan.next_after_id == previous_cursor
            || scan.matches.len() == MAX_RESULTS
            || retained_bytes == MAX_RESULT_BYTES
        {
            return Ok(Some(scan));
        }
        tokio::task::yield_now().await;
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep explicit identity and state arguments at this boundary without changing existing callers."
)]
async fn search_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    request_id: &str,
    query: &str,
    scope: SearchScope,
    expected_binding: Option<&RegistrationBinding>,
    after: Option<NodeId>,
    #[cfg(test)] before_publish: Option<(
        tokio::sync::oneshot::Sender<usize>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> Result<Option<SearchScan>, String> {
    let sequence = request_sequence(request_id)?;
    let (snapshot, generation, binding, mutation_token, mut observer, cancelled) = {
        let mut state = runtime.lock().await;
        let snapshot = state.gate.snapshot(owner).map_err(str::to_owned)?;
        let binding = state
            .store
            .current_registration()
            .await
            .map_err(|error| format!("{error:?}"))?
            .ok_or("not_connected")?;
        if expected_binding.is_some_and(|expected| expected != &binding) {
            return Err("stale_registration".into());
        }
        let mutation_token = state
            .store
            .search_mutation_token()
            .await
            .map_err(|error| format!("{error:?}"))?;
        if let Some(after_id) = after {
            let valid = state.search_session.as_ref().is_some_and(|session| {
                session.owner == owner
                    && session.request_id == request_id
                    && session.query == query
                    && session.scope == scope
                    && session.binding == binding
                    && session.mutation_token == mutation_token
                    && session.next_after_id == Some(after_id)
                    && !session.cancelled.load(Ordering::Acquire)
                    && session.observer.is_some()
            });
            if !valid {
                let stale_session = state.search_session.as_ref().is_some_and(|session| {
                    session.owner == owner
                        && session.request_id == request_id
                        && (session.binding != binding || session.mutation_token != mutation_token)
                });
                if stale_session {
                    invalidate_search(&mut state);
                }
                return Err("search_invalidated".into());
            }
            let session = state
                .search_session
                .as_mut()
                .expect("validated search session");
            (
                snapshot,
                session.generation,
                binding,
                mutation_token,
                session.observer.take().expect("validated search observer"),
                session.cancelled.clone(),
            )
        } else {
            if sequence <= state.search_sequence {
                return Err("search_invalidated".into());
            }
            let observer = state
                .store
                .search_observer()
                .map_err(|error| format!("{error:?}"))?;
            if let Some(previous) = state.search_session.take() {
                previous.cancelled.store(true, Ordering::Release);
            }
            state.search_generation = state
                .search_generation
                .checked_add(1)
                .ok_or("search_limit")?;
            state.search_sequence = sequence;
            let generation = state.search_generation;
            let cancelled = Arc::new(AtomicBool::new(false));
            state.search_session = Some(SearchSession {
                owner: owner.into(),
                request_id: request_id.into(),
                query: query.into(),
                scope,
                binding: binding.clone(),
                mutation_token,
                generation,
                next_after_id: None,
                observer: None,
                cancelled: cancelled.clone(),
            });
            (
                snapshot,
                generation,
                binding,
                mutation_token,
                observer,
                cancelled,
            )
        }
    };
    let scan = scan_complete(
        &mut observer,
        &binding,
        query,
        scope,
        after,
        &cancelled,
        Duration::from_millis(100),
    )
    .await
    .map_err(|error| format!("{error:?}"));
    #[cfg(test)]
    if let Some((ready, resume)) = before_publish {
        let _ = ready.send(
            scan.as_ref()
                .ok()
                .and_then(Option::as_ref)
                .map_or(0, |page| page.matches.len()),
        );
        let _ = resume.await;
    }
    let scan = match scan {
        Ok(Some(scan)) => match observer
            .candidates_are_current(&binding, &scan.matches, &cancelled)
            .await
        {
            Ok(true) => Ok(Some(scan)),
            Ok(false) => Ok(None),
            Err(error) => Err(format!("{error:?}")),
        },
        result => result,
    };
    let mut state = runtime.lock().await;
    let same_session = state.search_session.as_ref().is_some_and(|session| {
        session.generation == generation && Arc::ptr_eq(&session.cancelled, &cancelled)
    });
    if !same_session {
        return Ok(None);
    }
    let final_check = async {
        state.gate.validate(&snapshot).map_err(str::to_owned)?;
        if cancelled.load(Ordering::Acquire)
            || state
                .store
                .current_registration()
                .await
                .map_err(|error| format!("{error:?}"))?
                .as_ref()
                != Some(&binding)
            || state
                .store
                .search_mutation_token()
                .await
                .map_err(|error| format!("{error:?}"))?
                != mutation_token
        {
            return Err("search_invalidated".to_owned());
        }
        Ok(())
    }
    .await;
    if let Err(error) = final_check {
        state.search_session.take();
        return Err(error);
    }
    match scan {
        Ok(Some(scan)) => {
            let session = state
                .search_session
                .as_mut()
                .expect("validated search session");
            session.next_after_id = scan.next_after_id;
            if scan.next_after_id.is_some() {
                session.observer = Some(observer);
            }
            Ok(Some(scan))
        }
        other => {
            state.search_session.take();
            other
        }
    }
}

pub(crate) fn invalidate_search(state: &mut RuntimeState) {
    if let Some(session) = state.search_session.take() {
        session.cancelled.store(true, Ordering::Release);
    }
}

pub(crate) fn replace_renderer(state: &mut RuntimeState, owner: String) {
    invalidate_search(state);
    state.search_sequence = 0;
    state.gate.replace_owner(owner);
}

pub(crate) async fn cancel_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    request_id: &str,
) -> Result<(), String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| format!("{error:?}"))?
        .as_ref()
        != Some(binding)
    {
        return Err("stale_registration".into());
    }
    let sequence = request_sequence(request_id)?;
    state.search_sequence = state.search_sequence.max(sequence);
    if state.search_session.as_ref().is_some_and(|session| {
        session.owner == owner && session.binding == *binding && session.request_id == request_id
    }) && let Some(session) = state.search_session.take()
    {
        session.cancelled.store(true, Ordering::Release);
    }
    Ok(())
}

pub(crate) async fn workspace_search_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    request: &WorkspaceSearchRequest,
) -> Result<WorkspaceSearchPage, String> {
    let scan = search_request_for_renderer(runtime, request)
        .await?
        .ok_or("search_invalidated")?;
    let results = scan
        .matches
        .into_iter()
        .map(|candidate| WorkspaceSearchResult {
            node_id: candidate.node_id.to_string(),
            node: candidate.node().cloned(),
            scope: match candidate.scope {
                SearchScope::Workspace => WorkspaceSearchScope::Workspace,
                SearchScope::Recovery => WorkspaceSearchScope::Recovery,
            },
            source: match candidate.source {
                notryx_client_store::SearchSource::Draft => WorkspaceSearchSource::Draft,
                notryx_client_store::SearchSource::Cache => WorkspaceSearchSource::Cache,
            },
        })
        .collect();
    let count = |value: u64| u32::try_from(value).map_err(|_| "search_limit".to_owned());
    Ok(WorkspaceSearchPage {
        request_id: request.request_id.clone(),
        binding: request.binding.clone(),
        results,
        available_documents: count(scan.available_documents)?,
        total_documents: count(scan.total_documents)?,
        recovery_available_documents: count(scan.recovery_available_documents)?,
        recovery_total_documents: count(scan.recovery_total_documents)?,
        next_after_id: scan.next_after_id.map(|id| id.to_string()),
    })
}

async fn search_request_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    request: &WorkspaceSearchRequest,
) -> Result<Option<SearchScan>, String> {
    if request.query.is_empty()
        || request.query.len() > 4096
        || request.request_id.is_empty()
        || request.request_id.len() > 64
    {
        return Err("invalid_search".into());
    }
    let binding = crate::from_contract(&request.binding);
    let scope = match request.scope {
        WorkspaceSearchScope::Workspace => SearchScope::Workspace,
        WorkspaceSearchScope::Recovery => SearchScope::Recovery,
    };
    let after = request
        .after_id
        .as_deref()
        .map(str::parse::<NodeId>)
        .transpose()
        .map_err(|_| "invalid_node")?;
    search_for_renderer(
        runtime,
        &request.renderer_session_id,
        &request.request_id,
        &request.query,
        scope,
        Some(&binding),
        after,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use notryx_client_store::{ClientStore, search_contains};
    use notryx_protocol::{InventoryPage, NodeState, ServerInfo};
    use serde_json::json;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
    const HASH: &str = "sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2";

    async fn corpus(documents: usize) -> (tempfile::TempDir, ClientStore, RegistrationBinding) {
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
        let mut nodes = vec![root];
        for number in 2..=documents + 1 {
            let id = format!("019a1000-0000-7000-8000-{number:012x}");
            nodes.push(serde_json::from_value(json!({
                "node_id": id, "parent_id": ROOT, "name": format!("{number}.md"),
                "kind": "file", "deleted": false, "content_rev": "content-1",
                "content_etag": "\"content-1\"", "meta_rev": "meta-1",
                "meta_etag": "\"meta-1\"", "node_etag": "\"node-1\"",
                "content_hash": HASH, "size": "4", "children_generation": "0", "last_change_seq": "1"
            })).unwrap());
        }
        let mut store = ClientStore::open(directory.path().join("drafts.db"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let session = store.begin_bootstrap(&binding, &info).await.unwrap();
        for (index, chunk) in nodes.chunks(100).enumerate() {
            let has_more = (index + 1) * 100 < nodes.len();
            store
                .bootstrap_inventory(
                    &binding,
                    &session,
                    &InventoryPage {
                        workspace_epoch: info.workspace_epoch,
                        nodes: chunk.to_vec(),
                        next_after_id: has_more.then(|| chunk.last().unwrap().node_id),
                    },
                )
                .await
                .unwrap();
        }
        store
            .bootstrap_finish_inventory(&binding, &session, &info)
            .await
            .unwrap();
        store
            .bootstrap_publish(&binding, &session, &info)
            .await
            .unwrap();
        for node in &nodes[1..nodes.len() - 1] {
            assert!(store.cache_content(&binding, node, b"base").await.unwrap());
        }
        (directory, store, binding)
    }

    #[tokio::test]
    async fn scan_limits_results_and_resumes_without_skipping_or_claiming_missing_cache() {
        let (_directory, store, binding) = corpus(202).await;
        let mut observer = store.search_observer().unwrap();
        let cancelled = AtomicBool::new(false);
        let first = scan_page(
            &mut observer,
            &binding,
            "ba",
            SearchScope::Workspace,
            None,
            &cancelled,
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.matches.len(), 200);
        assert_eq!(
            (first.available_documents, first.total_documents),
            (200, 200)
        );
        let after = first
            .next_after_id
            .expect("remaining documents must be reachable");
        let second = scan_page(
            &mut observer,
            &binding,
            "ba",
            SearchScope::Workspace,
            Some(after),
            &cancelled,
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second.matches.len(), 1);
        assert_eq!((second.available_documents, second.total_documents), (1, 2));
        assert!(second.next_after_id.is_none());
        let mut ids: Vec<_> = first
            .matches
            .iter()
            .chain(&second.matches)
            .map(|candidate| candidate.node_id.to_string())
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 201);
    }

    #[tokio::test]
    async fn search_automatically_continues_work_slices_until_the_result_limit() {
        let (_directory, store, binding) = corpus(202).await;
        let mut observer = store.search_observer().unwrap();
        let cancelled = AtomicBool::new(false);
        let first = scan_complete(
            &mut observer,
            &binding,
            "ba",
            SearchScope::Workspace,
            None,
            &cancelled,
            Duration::ZERO,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.matches.len(), 200);
        assert_eq!(
            (first.available_documents, first.total_documents),
            (200, 200)
        );
        let after = first
            .next_after_id
            .expect("remaining documents must be reachable");
        let second = scan_complete(
            &mut observer,
            &binding,
            "ba",
            SearchScope::Workspace,
            Some(after),
            &cancelled,
            Duration::ZERO,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second.matches.len(), 1);
        assert_eq!((second.available_documents, second.total_documents), (1, 2));
        assert!(second.next_after_id.is_none());
        let mut ids: Vec<_> = first
            .matches
            .iter()
            .chain(&second.matches)
            .map(|candidate| candidate.node_id.to_string())
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 201);
    }

    #[tokio::test]
    async fn work_slices_keep_byte_limit_cursor_and_coverage_without_skipping() {
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
        let bytes = vec![b'a'; 13 * 1024 * 1024];
        let nodes: Vec<NodeState> = (2..=3).map(|number| serde_json::from_value(json!({
            "node_id": format!("019a1000-0000-7000-8000-{number:012x}"),
            "parent_id": ROOT, "name": format!("{number}.md"), "kind": "file", "deleted": false,
            "content_rev": "content-1", "content_etag": "\"content-1\"", "meta_rev": "meta-1",
            "meta_etag": "\"meta-1\"", "node_etag": "\"node-1\"",
            "content_hash": "sha256:3c7e56414864582ad2698687dce9d4be668bdd134f93a9823be91fa71b85e779",
            "size": bytes.len().to_string(), "children_generation": "0", "last_change_seq": "1"
        })).unwrap()).collect();
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
                    nodes: std::iter::once(root).chain(nodes.iter().cloned()).collect(),
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
        for node in &nodes {
            assert!(store.cache_content(&binding, node, &bytes).await.unwrap());
        }
        let mut observer = store.search_observer().unwrap();
        let cancelled = AtomicBool::new(false);
        let first = scan_complete(
            &mut observer,
            &binding,
            "a",
            SearchScope::Workspace,
            None,
            &cancelled,
            Duration::ZERO,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.matches.len(), 1);
        assert_eq!(first.matches[0].node_id, nodes[0].node_id);
        assert_eq!((first.available_documents, first.total_documents), (1, 1));
        assert_eq!(first.next_after_id, Some(nodes[0].node_id));
        let second = scan_complete(
            &mut observer,
            &binding,
            "a",
            SearchScope::Workspace,
            first.next_after_id,
            &cancelled,
            Duration::ZERO,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second.matches.len(), 1);
        assert_eq!(second.matches[0].node_id, nodes[1].node_id);
        assert_eq!((second.available_documents, second.total_documents), (1, 1));
        assert!(second.next_after_id.is_none());
    }

    #[tokio::test]
    async fn cancelling_between_work_slices_stops_search_without_blocking_checkpoint() {
        let (_directory, mut store, binding) = corpus(2).await;
        let id = "019a1000-0000-7000-8000-000000000002";
        let opened = store
            .open_registered_document(&binding, id, b"base", "content-1")
            .await
            .unwrap();
        let mut observer = store.search_observer().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = cancelled.clone();
        let task_binding = binding.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            scan_complete_with_hook(
                &mut observer,
                &task_binding,
                "ba",
                SearchScope::Workspace,
                None,
                &task_cancelled,
                Duration::ZERO,
                async {
                    let _ = ready_tx.send(());
                    let _ = resume_rx.await;
                },
            )
            .await
        });
        ready_rx.await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            store.save_registered_checkpoint(
                &binding,
                id,
                &opened.edit_session_id,
                1,
                b"new draft",
            ),
        )
        .await
        .expect("search must not block checkpoint")
        .unwrap();
        cancelled.store(true, Ordering::Release);
        let _ = resume_tx.send(());
        assert!(matches!(task.await.unwrap(), Ok(None)));
        let draft = store
            .read_registered_checkpoint(&binding, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"new draft");
        assert_eq!(draft.base_bytes, b"base");
    }

    #[tokio::test]
    async fn cancellation_during_match_preserves_draft_and_allows_new_search() {
        let (_directory, mut store, binding) = corpus(2).await;
        let id = "019a1000-0000-7000-8000-000000000002";
        let opened = store
            .open_registered_document(&binding, id, b"base", "content-1")
            .await
            .unwrap();
        let mut bytes = vec![b'x'; 1024 * 1024];
        bytes.extend_from_slice("한글".as_bytes());
        store
            .save_registered_checkpoint(&binding, id, &opened.edit_session_id, 1, &bytes)
            .await
            .unwrap();

        let mut observer = store.search_observer().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = cancelled.clone();
        let task_binding = binding.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            scan_page_with_match_hook(
                &mut observer,
                &task_binding,
                "한글",
                SearchScope::Workspace,
                None,
                &task_cancelled,
                Duration::from_millis(100),
                MAX_RESULTS,
                MAX_RESULT_BYTES,
                move |entering| {
                    if entering && let Some(sender) = ready_tx.take() {
                        let _ = sender.send(());
                    }
                },
            )
            .await
        });
        ready_rx.await.unwrap();
        cancelled.store(true, Ordering::Release);
        assert!(matches!(task.await.unwrap(), Ok(None)));

        let checkpoint = store
            .read_registered_checkpoint(&binding, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.bytes, bytes);
        assert_eq!(checkpoint.base_bytes, b"base");
        let mut observer = store.search_observer().unwrap();
        let fresh = scan_page(
            &mut observer,
            &binding,
            "한글",
            SearchScope::Workspace,
            None,
            &AtomicBool::new(false),
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fresh.matches.len(), 1);
        assert_eq!(fresh.matches[0].bytes(), bytes);
    }

    #[tokio::test]
    #[ignore = "manual cancellation latency measurement"]
    async fn cancellation_during_large_native_match_finishes_within_profile_target() {
        let (_directory, mut store, binding) = corpus(2).await;
        let id = "019a1000-0000-7000-8000-000000000002";
        let opened = store
            .open_registered_document(&binding, id, b"base", "content-1")
            .await
            .unwrap();
        let mut bytes = "가".as_bytes().to_vec();
        bytes.resize(5 * 1024 * 1024, b'x');
        store
            .save_registered_checkpoint(&binding, id, &opened.edit_session_id, 1, &bytes)
            .await
            .unwrap();
        assert!(search_contains(&bytes, "가"));

        let mut observer = store.search_observer().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let order = Arc::new(std::sync::Mutex::new((None::<Instant>, None::<Instant>)));
        let thread_cancelled = cancelled.clone();
        let thread_order = order.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let (go_tx, go_rx) = std::sync::mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            ready_rx.recv().unwrap();
            go_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(25));
            let mut order = thread_order.lock().unwrap();
            thread_cancelled.store(true, Ordering::Release);
            order.0 = Some(Instant::now());
        });
        let mut entered = false;
        let match_order = order.clone();
        let result = scan_page_with_match_hook(
            &mut observer,
            &binding,
            "없는질의",
            SearchScope::Workspace,
            None,
            &cancelled,
            Duration::from_millis(100),
            MAX_RESULTS,
            MAX_RESULT_BYTES,
            move |entering| {
                if entering && !entered {
                    entered = true;
                    ready_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                } else if !entering {
                    match_order.lock().unwrap().1 = Some(Instant::now());
                }
            },
        )
        .await;
        let returned = Instant::now();
        thread.join().unwrap();
        let (cancelled_at, matcher_finished_at) = *order.lock().unwrap();
        let cancelled_at = cancelled_at.expect("cancellation was requested");
        let matcher_finished_at = matcher_finished_at.expect("matcher was called");
        let checkpoint = store
            .read_registered_checkpoint(&binding, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.bytes, bytes);
        assert_eq!(checkpoint.base_bytes, b"base");
        if cancelled_at < matcher_finished_at {
            assert!(matches!(result, Ok(None)));
            let elapsed = returned.duration_since(cancelled_at);
            eprintln!("cancellation during native matcher: {elapsed:?}");
            assert!(
                elapsed <= Duration::from_millis(100),
                "profile target exceeded: {elapsed:?}"
            );
        } else {
            eprintln!("matcher finished before cancellation; latency was not observed");
            assert!(result.is_ok());
        }
    }

    async fn paused_search(
        runtime: Arc<Mutex<RuntimeState>>,
        request_id: &str,
    ) -> (
        tokio::task::JoinHandle<Result<Option<SearchScan>, String>>,
        oneshot::Sender<()>,
    ) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let request_id = request_id.to_owned();
        let task = tokio::spawn(async move {
            search_for_renderer(
                &runtime,
                "owner",
                &request_id,
                "ba",
                SearchScope::Workspace,
                None,
                None,
                Some((ready_tx, resume_rx)),
            )
            .await
        });
        assert!(ready_rx.await.unwrap() > 0, "a match must be observed");
        (task, resume_tx)
    }

    #[tokio::test]
    async fn newer_search_prevents_old_results_from_being_published() {
        let (_directory, store, _) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (old, resume) = paused_search(runtime.clone(), "1").await;
        let new = search_for_renderer(
            &runtime,
            "owner",
            "2",
            "ba",
            SearchScope::Workspace,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(new.is_some());
        let _ = resume.send(());
        assert!(matches!(old.await.unwrap(), Ok(None) | Err(_)));
    }

    #[tokio::test]
    async fn explicit_cancel_prevents_paused_result_publication() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (old, resume) = paused_search(runtime.clone(), "1").await;
        cancel_for_renderer(&runtime, "owner", &binding, "1")
            .await
            .unwrap();
        let _ = resume.send(());
        assert!(matches!(old.await.unwrap(), Ok(None) | Err(_)));
    }

    #[tokio::test]
    async fn stale_cancel_cannot_cancel_another_search() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (current, resume) = paused_search(runtime.clone(), "2").await;
        let _ = cancel_for_renderer(&runtime, "owner", &binding, "1").await;
        let _ = resume.send(());
        assert!(matches!(current.await.unwrap(), Ok(Some(_))));
    }

    #[tokio::test]
    async fn stale_renderer_cannot_cancel_current_search() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (current, resume) = paused_search(runtime.clone(), "1").await;
        assert!(
            cancel_for_renderer(&runtime, "stale", &binding, "1")
                .await
                .is_err()
        );
        let _ = resume.send(());
        assert!(matches!(current.await.unwrap(), Ok(Some(_))));
    }

    #[tokio::test]
    async fn wrong_binding_cannot_cancel_current_search() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (current, resume) = paused_search(runtime.clone(), "1").await;
        let wrong = RegistrationBinding {
            device_id: Uuid::new_v4().to_string(),
            ..binding
        };
        assert!(
            cancel_for_renderer(&runtime, "owner", &wrong, "1")
                .await
                .is_err()
        );
        let _ = resume.send(());
        assert!(matches!(current.await.unwrap(), Ok(Some(_))));
    }

    fn request(binding: &RegistrationBinding) -> WorkspaceSearchRequest {
        WorkspaceSearchRequest {
            renderer_session_id: "owner".into(),
            binding: crate::to_contract(binding.clone()),
            request_id: "1".into(),
            query: "ba".into(),
            scope: notryx_protocol::WorkspaceSearchScope::Workspace,
            after_id: None,
        }
    }

    #[tokio::test]
    async fn request_rejects_other_registration_and_invalid_input() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let mut input = request(&binding);
        input.binding.device_id = Uuid::new_v4().to_string();
        assert!(matches!(
            search_request_for_renderer(&runtime, &input).await,
            Ok(None) | Err(_)
        ));
        input.binding = crate::to_contract(binding);
        input.query = String::new();
        assert!(matches!(
            search_request_for_renderer(&runtime, &input).await,
            Ok(None) | Err(_)
        ));
        input.query = "a".repeat(4097);
        assert!(matches!(
            search_request_for_renderer(&runtime, &input).await,
            Ok(None) | Err(_)
        ));
    }

    #[tokio::test]
    async fn recovery_scope_excludes_workspace_matches() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let mut input = request(&binding);
        input.scope = notryx_protocol::WorkspaceSearchScope::Recovery;
        let page = search_request_for_renderer(&runtime, &input)
            .await
            .unwrap()
            .unwrap();
        assert!(page.matches.is_empty());
    }

    #[tokio::test]
    async fn renderer_replacement_prevents_old_results_from_being_published() {
        let (_directory, store, _) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (old, resume) = paused_search(runtime.clone(), "1").await;
        runtime
            .lock()
            .await
            .gate
            .replace_owner("replacement".into());
        let _ = resume.send(());
        assert!(matches!(old.await.unwrap(), Ok(None) | Err(_)));
    }

    #[tokio::test]
    async fn registration_replacement_prevents_old_results_from_being_published() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (old, resume) = paused_search(runtime.clone(), "1").await;
        let replacement = RegistrationBinding {
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
            ..binding
        };
        runtime
            .lock()
            .await
            .store
            .activate_registration(&replacement)
            .await
            .unwrap();
        let _ = resume.send(());
        assert!(matches!(old.await.unwrap(), Ok(None) | Err(_)));
    }

    #[tokio::test]
    async fn changed_draft_prevents_old_results_from_being_published() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let (old, resume) = paused_search(runtime.clone(), "1").await;
        let mut state = runtime.lock().await;
        let id = "019a1000-0000-7000-8000-000000000002";
        let opened = state
            .store
            .open_registered_document(&binding, id, b"base", "content-1")
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(&binding, id, &opened.edit_session_id, 1, b"changed")
            .await
            .unwrap();
        drop(state);
        let _ = resume.send(());
        assert!(matches!(old.await.unwrap(), Ok(None) | Err(_)));
    }

    #[tokio::test]
    async fn continuation_invalidates_previous_slice_after_a_document_changes() {
        let (_directory, store, binding) = corpus(202).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let first = search_for_renderer(
            &runtime,
            "owner",
            "1",
            "ba",
            SearchScope::Workspace,
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let document = first
            .matches
            .first()
            .expect("first slice must contain a match");
        let cursor = first.next_after_id.expect("more documents must remain");
        let mut state = runtime.lock().await;
        let id = document.node_id.to_string();
        let opened = state
            .store
            .open_registered_document(&binding, &id, b"base", "content-1")
            .await
            .unwrap();
        state
            .store
            .save_registered_checkpoint(&binding, &id, &opened.edit_session_id, 1, b"changed")
            .await
            .unwrap();
        drop(state);
        let next = search_for_renderer(
            &runtime,
            "owner",
            "1",
            "ba",
            SearchScope::Workspace,
            None,
            Some(cursor),
            None,
        )
        .await;
        assert!(
            matches!(next, Ok(None) | Err(_)),
            "the previous results must be invalidated"
        );
    }

    #[tokio::test]
    async fn continuation_cannot_reuse_cursor_for_another_query() {
        let (_directory, store, _) = corpus(202).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let first = search_for_renderer(
            &runtime,
            "owner",
            "1",
            "ba",
            SearchScope::Workspace,
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let cursor = first.next_after_id.expect("more documents must remain");
        let next = search_for_renderer(
            &runtime,
            "owner",
            "1",
            "different",
            SearchScope::Workspace,
            None,
            Some(cursor),
            None,
        )
        .await;
        assert!(
            matches!(next, Ok(None) | Err(_)),
            "cursor belongs to the original query"
        );
    }

    #[tokio::test]
    async fn old_continuation_cannot_cancel_new_search() {
        let (_directory, store, _) = corpus(202).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let first = search_for_renderer(
            &runtime,
            "owner",
            "1",
            "ba",
            SearchScope::Workspace,
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let cursor = first.next_after_id.expect("more documents must remain");
        let (new, resume) = paused_search(runtime.clone(), "2").await;
        let old = search_for_renderer(
            &runtime,
            "owner",
            "1",
            "ba",
            SearchScope::Workspace,
            None,
            Some(cursor),
            None,
        )
        .await;
        assert!(matches!(old, Ok(None) | Err(_)));
        let _ = resume.send(());
        assert!(
            matches!(new.await.unwrap(), Ok(Some(_))),
            "old continuation must not cancel the new search"
        );
    }

    #[tokio::test]
    async fn cancel_before_start_rejects_late_start_without_cancelling_new_search() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        cancel_for_renderer(&runtime, "owner", &binding, "1")
            .await
            .unwrap();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let current_runtime = runtime.clone();
        let current = tokio::spawn(async move {
            search_for_renderer(
                &current_runtime,
                "owner",
                "2",
                "ba",
                SearchScope::Workspace,
                None,
                None,
                Some((ready_tx, resume_rx)),
            )
            .await
        });
        assert!(ready_rx.await.unwrap() > 0);
        let late = search_for_renderer(
            &runtime,
            "owner",
            "1",
            "ba",
            SearchScope::Workspace,
            None,
            None,
            None,
        )
        .await;
        let _ = resume_tx.send(());
        assert!(matches!(late, Ok(None) | Err(_)));
        assert!(
            matches!(current.await.unwrap(), Ok(Some(_))),
            "late start must not cancel a newer search"
        );
    }

    #[tokio::test]
    async fn completed_search_request_id_cannot_be_reused() {
        let (_directory, store, _) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        assert!(matches!(
            search_for_renderer(
                &runtime,
                "owner",
                "1",
                "ba",
                SearchScope::Workspace,
                None,
                None,
                None
            )
            .await,
            Ok(Some(_))
        ));
        assert!(matches!(
            search_for_renderer(
                &runtime,
                "owner",
                "1",
                "ba",
                SearchScope::Workspace,
                None,
                None,
                None
            )
            .await,
            Ok(None) | Err(_)
        ));
    }

    #[tokio::test]
    async fn stale_renderer_or_binding_cannot_record_search_cancellation() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let wrong = RegistrationBinding {
            device_id: Uuid::new_v4().to_string(),
            ..binding.clone()
        };
        assert!(
            cancel_for_renderer(&runtime, "stale", &binding, "999")
                .await
                .is_err()
        );
        assert!(
            cancel_for_renderer(&runtime, "owner", &wrong, "999")
                .await
                .is_err()
        );
        assert!(matches!(
            search_for_renderer(
                &runtime,
                "owner",
                "1",
                "ba",
                SearchScope::Workspace,
                Some(&binding),
                None,
                None
            )
            .await,
            Ok(Some(_))
        ));
    }

    #[tokio::test]
    async fn search_request_ids_are_positive_canonical_safe_integers() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let mut input = request(&binding);
        for invalid in ["0", "01", "-1", "9007199254740992", "not-a-number"] {
            input.request_id = invalid.into();
            assert!(
                search_request_for_renderer(&runtime, &input).await.is_err(),
                "invalid request ID must be rejected"
            );
        }
        input.request_id = "9007199254740991".into();
        assert!(matches!(
            search_request_for_renderer(&runtime, &input).await,
            Ok(Some(_))
        ));
    }

    #[tokio::test]
    async fn renderer_replacement_allows_new_owner_sequence_to_restart() {
        let (_directory, store, binding) = corpus(2).await;
        let runtime = Arc::new(Mutex::new(RuntimeState::new(store, "owner".into())));
        let mut input = request(&binding);
        input.request_id = "99".into();
        assert!(matches!(
            search_request_for_renderer(&runtime, &input).await,
            Ok(Some(_))
        ));
        {
            let mut state = runtime.lock().await;
            replace_renderer(&mut state, "replacement".into());
        }
        input.renderer_session_id = "replacement".into();
        input.request_id = "1".into();
        assert!(matches!(
            search_request_for_renderer(&runtime, &input).await,
            Ok(Some(_))
        ));
    }
}
