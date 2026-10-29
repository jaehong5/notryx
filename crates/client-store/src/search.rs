use crate::{ClientStore, RegistrationBinding, StoreError, storage_error};
use notryx_domain::NodeId;
use notryx_protocol::{NodeKind, NodeState, StrongEtag};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_normalization::UnicodeNormalization;

#[cfg(test)]
#[path = "search_observer_tests.rs"]
mod observer_tests;

#[cfg(test)]
#[path = "search_sql_plan_tests.rs"]
mod sql_plan_tests;

#[cfg(test)]
#[path = "search_stage_profile.rs"]
mod stage_profile;

// Once the remote page contains k distinct IDs, no draft ID after its kth ID can enter the first k of the union.
const SEARCH_CANDIDATES_SQL: &str = "WITH remote_page AS MATERIALIZED (
                SELECT node_id FROM remote_nodes
                WHERE registration_id=?1 AND node_id>?2 ORDER BY node_id LIMIT ?3
             ), draft_page AS MATERIALIZED (
                SELECT d.document_id AS node_id FROM registered_documents d
                JOIN draft_checkpoints dc ON dc.document_id=d.storage_id
                WHERE d.registration_id=?1 AND d.document_id>?2
                    AND d.document_id<=COALESCE(
                        (SELECT CASE WHEN COUNT(*)=?3 THEN MAX(node_id) END FROM remote_page),
                        (SELECT MAX(document_id) FROM registered_documents WHERE registration_id=?1)
                    )
                ORDER BY d.document_id LIMIT ?3
             )
             SELECT ids.node_id,n.state,d.storage_id,c.generation,c.format_version,
                length(c.bytes) AS draft_len,length(b.bytes) AS base_len,b.revision AS base_rev,
                c.bytes <> b.bytes AS draft_changed,
                EXISTS(SELECT 1 FROM client_outbox o WHERE o.registration_id=?1
                    AND o.document_id=ids.node_id AND o.operation_kind='content'
                    AND o.status IN ('queued','sent','conflict')) AS pending,
                EXISTS(SELECT 1 FROM client_outbox o JOIN outbox_streams s ON s.registration_id=o.registration_id
                    WHERE o.registration_id=d.registration_id AND o.document_id=d.document_id
                    AND o.operation_kind='content' AND o.status='committed' AND o.terminal_kind IS NULL
                    AND o.operation_seq<=s.confirmed_through AND o.generation=c.generation
                    AND o.payload_retired=0 AND o.bytes=c.bytes AND o.committed_revision=b.revision) AS confirmed,
                cc.content_rev AS cache_rev,cc.content_hash AS cache_hash,cc.size AS cache_size,
                length(cc.bytes) AS cache_len
             FROM (
                SELECT node_id FROM remote_page
                UNION
                SELECT node_id FROM draft_page
             ) ids
             LEFT JOIN remote_nodes n ON n.registration_id=?1 AND n.node_id=ids.node_id
             LEFT JOIN registered_documents d ON d.registration_id=?1 AND d.document_id=ids.node_id
             LEFT JOIN draft_checkpoints c ON c.document_id=d.storage_id
             LEFT JOIN draft_bases b ON b.document_id=d.storage_id
             LEFT JOIN cached_content cc ON cc.registration_id=?1 AND cc.node_id=ids.node_id
             WHERE ids.node_id>?2 ORDER BY ids.node_id LIMIT ?3";

const MAX_PAGE_BYTES: usize = 25 * 1024 * 1024;

pub struct SearchObserver {
    database_path: std::path::PathBuf,
    lock: std::fs::File,
    reader: Option<ClientStore>,
}

impl SearchObserver {
    pub async fn candidates_are_current(
        &mut self,
        binding: &RegistrationBinding,
        candidates: &[SearchCandidate],
        cancelled: &AtomicBool,
    ) -> Result<bool, StoreError> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        crate::verify_locked_path(&self.lock, &self.database_path)?;
        let reader = self.reader.as_mut().ok_or(StoreError::InvalidRequest)?;
        for candidate in candidates {
            if !reader
                .search_candidate_is_current(binding, candidate)
                .await?
                || cancelled.load(Ordering::Acquire)
            {
                return Ok(false);
            }
        }
        crate::verify_locked_path(&self.lock, &self.database_path)?;
        Ok(!cancelled.load(Ordering::Acquire))
    }

    pub async fn observe(
        &mut self,
        binding: &RegistrationBinding,
        after: Option<NodeId>,
        cancelled: &AtomicBool,
    ) -> Result<Option<SearchPage>, StoreError> {
        self.observe_inner(binding, after, cancelled, async {})
            .await
    }

    async fn observe_inner<F: Future<Output = ()>>(
        &mut self,
        binding: &RegistrationBinding,
        after: Option<NodeId>,
        cancelled: &AtomicBool,
        pause: F,
    ) -> Result<Option<SearchPage>, StoreError> {
        self.observe_with_snapshot_hook(binding, after, cancelled, async {}, pause)
            .await
    }

    async fn observe_with_snapshot_hook<F: Future<Output = ()>, G: Future<Output = ()>>(
        &mut self,
        binding: &RegistrationBinding,
        after: Option<NodeId>,
        cancelled: &AtomicBool,
        snapshot_hook: F,
        pause: G,
    ) -> Result<Option<SearchPage>, StoreError> {
        self.observe_with_hooks(binding, after, cancelled, async {}, snapshot_hook, pause)
            .await
    }

    async fn observe_with_hooks<
        F: Future<Output = ()>,
        G: Future<Output = ()>,
        H: Future<Output = ()>,
    >(
        &mut self,
        binding: &RegistrationBinding,
        after: Option<NodeId>,
        cancelled: &AtomicBool,
        opened_hook: F,
        snapshot_hook: G,
        pause: H,
    ) -> Result<Option<SearchPage>, StoreError> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        crate::verify_locked_path(&self.lock, &self.database_path)?;
        if self.reader.is_none() {
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&self.database_path)
                .read_only(true)
                .busy_timeout(std::time::Duration::from_secs(2));
            let connection = sqlx::SqliteConnection::connect_with(&options)
                .await
                .map_err(storage_error)?;
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            crate::verify_locked_path(&self.lock, &self.database_path)?;
            opened_hook.await;
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            let mut reader = ClientStore {
                connection,
                sessions: Default::default(),
                _lock: self.lock.try_clone().map_err(storage_error)?,
                database_path: self.database_path.clone(),
            };
            let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
                .fetch_one(&mut reader.connection)
                .await
                .map_err(storage_error)?;
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            let version: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&mut reader.connection)
                .await
                .map_err(storage_error)?;
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            if application_id != crate::APPLICATION_ID || version != crate::SCHEMA_VERSION {
                return Err(StoreError::UnsupportedFormat);
            }
            crate::verify_locked_path(&self.lock, &self.database_path)?;
            self.reader = Some(reader);
        }
        let reader = self.reader.as_mut().expect("search reader initialized");
        let Some(page) = reader
            .search_candidates_with_hook(binding, after, 1, Some(cancelled), snapshot_hook, None)
            .await?
        else {
            return Ok(None);
        };
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        crate::verify_locked_path(&self.lock, &self.database_path)?;
        pause.await;
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        crate::verify_locked_path(&self.lock, &self.database_path)?;
        reader.registration_id(binding, true).await?;
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        for candidate in &page.candidates {
            if !reader
                .search_candidate_is_current(binding, candidate)
                .await?
            {
                return Ok(None);
            }
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
        }
        crate::verify_locked_path(&self.lock, &self.database_path)?;
        Ok(Some(page))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSource {
    Draft,
    Cache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Workspace,
    Recovery,
}

/// ```compile_fail
/// use notryx_client_store::SearchCandidate;
/// fn alter(candidate: &mut SearchCandidate) {
///     candidate.bytes.clear();
/// }
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct SearchCandidate {
    pub node_id: NodeId,
    pub source: SearchSource,
    pub scope: SearchScope,
    bytes: Vec<u8>,
    version: SearchVersion,
}

impl SearchCandidate {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn node(&self) -> Option<&NodeState> {
        self.version.remote.as_ref()
    }
}

impl std::fmt::Debug for SearchCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SearchCandidate")
            .field("node_id", &self.node_id)
            .field("source", &self.source)
            .field("scope", &self.scope)
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchVersion {
    binding: RegistrationBinding,
    node_id: NodeId,
    source: SearchSource,
    scope: SearchScope,
    remote: Option<NodeState>,
    generation: Option<i64>,
    base_revision: Option<String>,
    pending: i64,
    confirmed: i64,
    cache_revision: Option<String>,
    cache_hash: Option<String>,
    cache_size: Option<String>,
    cache_len: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPage {
    pub candidates: Vec<SearchCandidate>,
    pub available_documents: u64,
    pub total_documents: u64,
    pub recovery_available_documents: u64,
    pub recovery_total_documents: u64,
    pub next_after_id: Option<NodeId>,
}

struct LiteralMatcher {
    pattern: Vec<char>,
    prefix: Vec<usize>,
    length: usize,
}

impl LiteralMatcher {
    fn new(query: &str) -> Option<Self> {
        let pattern: Vec<char> = query.nfc().collect();
        if pattern.is_empty() {
            return None;
        }
        let mut prefix = vec![0usize; pattern.len()];
        let mut length = 0;
        for i in 1..pattern.len() {
            while length > 0 && pattern[length] != pattern[i] {
                length = prefix[length - 1];
            }
            if pattern[length] == pattern[i] {
                length += 1;
            }
            prefix[i] = length;
        }
        Some(Self {
            pattern,
            prefix,
            length: 0,
        })
    }

    fn push(&mut self, character: char) -> bool {
        while self.length > 0 && self.pattern[self.length] != character {
            self.length = self.prefix[self.length - 1];
        }
        if self.pattern[self.length] == character {
            self.length += 1;
            return self.length == self.pattern.len();
        }
        false
    }
}

pub fn search_contains(bytes: &[u8], query: &str) -> bool {
    let Ok(source) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some(mut matcher) = LiteralMatcher::new(query) else {
        return false;
    };
    source.nfc().any(|character| matcher.push(character))
}

pub async fn search_contains_cancellable(
    bytes: &[u8],
    query: &str,
    cancelled: &AtomicBool,
) -> Option<bool> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let Ok(source) = std::str::from_utf8(bytes) else {
        return (!cancelled.load(Ordering::Acquire)).then_some(false);
    };
    let Some(mut matcher) = LiteralMatcher::new(query) else {
        return (!cancelled.load(Ordering::Acquire)).then_some(false);
    };
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    for (index, character) in source.nfc().enumerate() {
        if matcher.push(character) {
            return (!cancelled.load(Ordering::Acquire)).then_some(true);
        }
        if (index + 1) % 4096 == 0 {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            tokio::task::yield_now().await;
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
        }
    }
    (!cancelled.load(Ordering::Acquire)).then_some(false)
}

async fn hash_with_hook(
    bytes: &[u8],
    cancelled: Option<&AtomicBool>,
    progress: &mut Option<&mut (dyn FnMut(usize) + Send)>,
) -> Option<String> {
    let mut result = String::from("sha256:");
    use std::fmt::Write as _;
    let mut digest = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return None;
        }
        digest.update(chunk);
        if let Some(hook) = progress.as_deref_mut() {
            hook(chunk.len());
        }
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return None;
        }
        if cancelled.is_some() {
            tokio::task::yield_now().await;
        }
    }
    for byte in digest.finalize() {
        let _ = write!(&mut result, "{byte:02x}");
    }
    Some(result)
}

#[cfg(test)]
fn hash(bytes: &[u8]) -> String {
    let mut result = String::from("sha256:");
    use std::fmt::Write as _;
    for byte in Sha256::digest(bytes) {
        let _ = write!(&mut result, "{byte:02x}");
    }
    result
}

impl ClientStore {
    pub async fn search_mutation_token(&mut self) -> Result<(i64, i64), StoreError> {
        crate::verify_locked_path(&self._lock, &self.database_path)?;
        let row = sqlx::query(
            "SELECT total_changes() AS local_changes, data_version FROM pragma_data_version",
        )
        .fetch_one(&mut self.connection)
        .await
        .map_err(storage_error)?;
        crate::verify_locked_path(&self._lock, &self.database_path)?;
        Ok((
            row.try_get("local_changes").map_err(storage_error)?,
            row.try_get("data_version").map_err(storage_error)?,
        ))
    }

    pub fn search_observer(&self) -> Result<SearchObserver, StoreError> {
        crate::verify_locked_path(&self._lock, &self.database_path)?;
        Ok(SearchObserver {
            database_path: self.database_path.clone(),
            lock: self._lock.try_clone().map_err(storage_error)?,
            reader: None,
        })
    }

    pub async fn search_candidate_is_current(
        &mut self,
        binding: &RegistrationBinding,
        candidate: &SearchCandidate,
    ) -> Result<bool, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let version = &candidate.version;
        if version.binding != *binding
            || version.node_id != candidate.node_id
            || version.source != candidate.source
            || version.scope != candidate.scope
        {
            return Ok(false);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let active: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id=? AND status='active')",
        )
        .bind(registration)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        if active == 0 {
            return Err(StoreError::StaleRegistration);
        }
        let row = sqlx::query(
            "SELECT n.state,c.generation,b.revision AS base_rev,
                EXISTS(SELECT 1 FROM client_outbox o WHERE o.registration_id=?
                    AND o.document_id=target.node_id AND o.operation_kind='content'
                    AND o.status IN ('queued','sent','conflict')) AS pending,
                EXISTS(SELECT 1 FROM client_outbox o JOIN outbox_streams s ON s.registration_id=o.registration_id
                    WHERE o.registration_id=d.registration_id AND o.document_id=d.document_id
                    AND o.operation_kind='content' AND o.status='committed' AND o.terminal_kind IS NULL
                    AND o.operation_seq<=s.confirmed_through AND o.generation=c.generation
                    AND o.payload_retired=0 AND o.bytes=c.bytes AND o.committed_revision=b.revision) AS confirmed,
                cc.content_rev AS cache_rev,cc.content_hash AS cache_hash,cc.size AS cache_size,
                length(cc.bytes) AS cache_len
             FROM (SELECT ? AS node_id) target
             LEFT JOIN remote_nodes n ON n.registration_id=? AND n.node_id=target.node_id
             LEFT JOIN registered_documents d ON d.registration_id=? AND d.document_id=target.node_id
             LEFT JOIN draft_checkpoints c ON c.document_id=d.storage_id
             LEFT JOIN draft_bases b ON b.document_id=d.storage_id
             LEFT JOIN cached_content cc ON cc.registration_id=? AND cc.node_id=target.node_id",
        )
        .bind(registration)
        .bind(candidate.node_id.to_string())
        .bind(registration)
        .bind(registration)
        .bind(registration)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        let raw: Option<&str> = row.try_get("state").map_err(storage_error)?;
        let remote: Option<NodeState> = raw
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| StoreError::UnsupportedFormat)?;
        if remote
            .as_ref()
            .is_some_and(|node| node.node_id != candidate.node_id)
        {
            return Err(StoreError::UnsupportedFormat);
        }
        let generation: Option<i64> = row.try_get("generation").map_err(storage_error)?;
        let base_revision: Option<String> = row.try_get("base_rev").map_err(storage_error)?;
        let pending: i64 = row.try_get("pending").map_err(storage_error)?;
        let confirmed: i64 = row.try_get("confirmed").map_err(storage_error)?;
        if remote != version.remote
            || generation != version.generation
            || base_revision != version.base_revision
            || pending != version.pending
            || confirmed != version.confirmed
        {
            return Ok(false);
        }
        if candidate.source == SearchSource::Cache {
            let cache_revision: Option<String> = row.try_get("cache_rev").map_err(storage_error)?;
            let cache_hash: Option<String> = row.try_get("cache_hash").map_err(storage_error)?;
            let cache_size: Option<String> = row.try_get("cache_size").map_err(storage_error)?;
            let cache_len: Option<i64> = row.try_get("cache_len").map_err(storage_error)?;
            if cache_revision != version.cache_revision
                || cache_hash != version.cache_hash
                || cache_size != version.cache_size
                || cache_len != version.cache_len
            {
                return Ok(false);
            }
        }
        let matches: i64 = match candidate.source {
            SearchSource::Draft => sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM registered_documents d
                    JOIN draft_checkpoints c ON c.document_id=d.storage_id
                    WHERE d.registration_id=? AND d.document_id=? AND c.bytes=?)",
            ),
            SearchSource::Cache => sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM cached_content
                    WHERE registration_id=? AND node_id=? AND bytes=?)",
            ),
        }
        .bind(registration)
        .bind(candidate.node_id.to_string())
        .bind(&candidate.bytes)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(matches == 1)
    }

    pub async fn search_candidates(
        &mut self,
        binding: &RegistrationBinding,
        after: Option<NodeId>,
        limit: u16,
    ) -> Result<SearchPage, StoreError> {
        self.search_candidates_with_hook(binding, after, limit, None, async {}, None)
            .await?
            .ok_or(StoreError::InvalidRequest)
    }

    async fn search_candidates_with_hook<F: Future<Output = ()>>(
        &mut self,
        binding: &RegistrationBinding,
        after: Option<NodeId>,
        limit: u16,
        cancelled: Option<&AtomicBool>,
        snapshot_hook: F,
        mut hash_progress: Option<&mut (dyn FnMut(usize) + Send)>,
    ) -> Result<Option<SearchPage>, StoreError> {
        if limit == 0 || limit > 100 {
            return Err(StoreError::InvalidRequest);
        }
        let registration = self.registration_id(binding, true).await?;
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM client_registrations WHERE id=? AND status='active')",
        )
        .bind(registration)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        if active == 0 {
            return Err(StoreError::StaleRegistration);
        }
        let index: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM remote_index WHERE registration_id=?)")
                .bind(registration)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        if index == 0 {
            return Err(StoreError::ReconciliationRequired);
        }
        snapshot_hook.await;
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        let rows = sqlx::query(SEARCH_CANDIDATES_SQL)
            .bind(registration)
            .bind(after.map(|id| id.to_string()).unwrap_or_default())
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage_error)?;
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        let mut result = SearchPage {
            candidates: Vec::new(),
            available_documents: 0,
            total_documents: 0,
            recovery_available_documents: 0,
            recovery_total_documents: 0,
            next_after_id: None,
        };
        let mut used = 0usize;
        let mut last = after;
        let mut pending_candidates = Vec::new();
        for (position, row) in rows.iter().enumerate() {
            if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
                return Ok(None);
            }
            if position == usize::from(limit) {
                result.next_after_id = last;
                break;
            }
            let raw_id: String = row.try_get("node_id").map_err(storage_error)?;
            let id: NodeId = raw_id.parse().map_err(|_| StoreError::UnsupportedFormat)?;
            let raw: Option<&str> = row.try_get("state").map_err(storage_error)?;
            let node: Option<NodeState> = raw
                .map(serde_json::from_str)
                .transpose()
                .map_err(|_| StoreError::UnsupportedFormat)?;
            if node.as_ref().is_some_and(|node| node.node_id != id) {
                return Err(StoreError::UnsupportedFormat);
            }
            let draft_len: Option<i64> = row.try_get("draft_len").map_err(storage_error)?;
            let base_len: Option<i64> = row.try_get("base_len").map_err(storage_error)?;
            let changed: Option<i64> = row.try_get("draft_changed").map_err(storage_error)?;
            let pending: i64 = row.try_get("pending").map_err(storage_error)?;
            let confirmed: i64 = row.try_get("confirmed").map_err(storage_error)?;
            if draft_len.is_some() && (base_len.is_none() || changed.is_none()) {
                return Err(StoreError::UnsupportedFormat);
            }
            let dirty =
                draft_len.is_some() && (changed == Some(1) || pending == 1 || confirmed == 0);
            let scope = match node.as_ref() {
                Some(node)
                    if !node.deleted
                        && node.kind == NodeKind::File
                        && node.name.to_ascii_lowercase().ends_with(".md") =>
                {
                    SearchScope::Workspace
                }
                None | Some(NodeState { deleted: true, .. }) if dirty => SearchScope::Recovery,
                _ => {
                    last = Some(id);
                    continue;
                }
            };
            let content = if scope == SearchScope::Workspace {
                let node = node.as_ref().ok_or(StoreError::UnsupportedFormat)?;
                let revision = node
                    .content_rev
                    .as_ref()
                    .ok_or(StoreError::UnsupportedFormat)?;
                let etag = StrongEtag::from_content_revision(revision)
                    .map_err(|_| StoreError::UnsupportedFormat)?;
                if node.content_etag.as_deref() != Some(etag.as_str()) {
                    return Err(StoreError::UnsupportedFormat);
                }
                let digest = node
                    .content_hash
                    .as_deref()
                    .ok_or(StoreError::UnsupportedFormat)?;
                let size: usize = node
                    .size
                    .as_deref()
                    .ok_or(StoreError::UnsupportedFormat)?
                    .parse()
                    .map_err(|_| StoreError::UnsupportedFormat)?;
                Some((revision.as_str(), digest, size))
            } else {
                None
            };
            let cache_len: Option<i64> = row.try_get("cache_len").map_err(storage_error)?;
            let cache_rev: Option<String> = row.try_get("cache_rev").map_err(storage_error)?;
            let cache_hash: Option<String> = row.try_get("cache_hash").map_err(storage_error)?;
            let cache_size: Option<String> = row.try_get("cache_size").map_err(storage_error)?;
            let current_cache = content.is_some_and(|(revision, digest, size)| {
                cache_len.and_then(|length| usize::try_from(length).ok()) == Some(size)
                    && size <= MAX_PAGE_BYTES
                    && cache_rev.as_deref() == Some(revision)
                    && cache_hash.as_deref() == Some(digest)
                    && cache_size
                        .as_deref()
                        .and_then(|value| value.parse::<usize>().ok())
                        == Some(size)
            });
            let base_rev: Option<String> = row.try_get("base_rev").map_err(storage_error)?;
            let clean_draft = draft_len.is_some()
                && !dirty
                && content.is_some_and(|(revision, _, _)| base_rev.as_deref() == Some(revision));
            // A successful conditional write proves its expected revision precedes its commit;
            // operation sequence orders only writes in this registration's stream.
            let committed_hash = if draft_len.is_some() && !dirty && !clean_draft {
                if let (Some((revision, _, _)), Some(base_revision)) =
                    (content, base_rev.as_deref())
                {
                    let matched: Option<String> = sqlx::query_scalar(
                        "SELECT latest.payload_hash FROM client_outbox earlier
                            JOIN client_outbox latest ON latest.registration_id=earlier.registration_id
                                AND latest.document_id=earlier.document_id
                                AND latest.operation_kind='content'
                                AND latest.status='committed' AND latest.terminal_kind IS NULL
                                AND latest.operation_seq>=earlier.operation_seq
                            WHERE earlier.registration_id=? AND earlier.document_id=?
                                AND earlier.operation_kind='content'
                                AND earlier.status='committed' AND earlier.terminal_kind IS NULL
                                AND earlier.expected_revision=? AND latest.committed_revision=?
                            ORDER BY latest.operation_seq DESC LIMIT 1",
                    )
                    .bind(registration)
                    .bind(id.to_string())
                    .bind(revision)
                    .bind(base_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                    if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
                        return Ok(None);
                    }
                    matched
                } else {
                    None
                }
            } else {
                None
            };
            let source = if dirty || clean_draft || committed_hash.is_some() {
                Some((
                    SearchSource::Draft,
                    draft_len
                        .and_then(|length| usize::try_from(length).ok())
                        .ok_or(StoreError::UnsupportedFormat)?,
                ))
            } else if current_cache {
                Some((
                    SearchSource::Cache,
                    content.ok_or(StoreError::UnsupportedFormat)?.2,
                ))
            } else {
                None
            };
            if let Some((_, length)) = source {
                if length > MAX_PAGE_BYTES {
                    return Err(StoreError::UnsupportedFormat);
                }
                if used
                    .checked_add(length)
                    .is_none_or(|total| total > MAX_PAGE_BYTES)
                {
                    result.next_after_id = last;
                    break;
                }
            }
            match scope {
                SearchScope::Workspace => result.total_documents += 1,
                SearchScope::Recovery => result.recovery_total_documents += 1,
            }
            if let Some((source, length)) = source {
                let bytes: Vec<u8> = match source {
                    SearchSource::Draft => {
                        let generation: i64 = row.try_get("generation").map_err(storage_error)?;
                        let format: i64 = row.try_get("format_version").map_err(storage_error)?;
                        if generation <= 0
                            || format != 1
                            || draft_len.is_none_or(|n| n > crate::MAX_DOCUMENT_BYTES as i64)
                            || base_len.is_none_or(|n| n > crate::MAX_DOCUMENT_BYTES as i64)
                        {
                            return Err(StoreError::UnsupportedFormat);
                        }
                        let storage_id: String =
                            row.try_get("storage_id").map_err(storage_error)?;
                        sqlx::query_scalar(
                            "SELECT bytes FROM draft_checkpoints WHERE document_id=?",
                        )
                        .bind(storage_id)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(storage_error)?
                    }
                    SearchSource::Cache => sqlx::query_scalar(
                        "SELECT bytes FROM cached_content WHERE registration_id=? AND node_id=?",
                    )
                    .bind(registration)
                    .bind(&raw_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(storage_error)?,
                };
                if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
                    return Ok(None);
                }
                if bytes.len() != length {
                    return Err(StoreError::UnsupportedFormat);
                }
                used += bytes.len();
                let expected_hash = committed_hash
                    .map(|hash| (hash, length))
                    .or_else(|| content.map(|(_, digest, size)| (digest.to_owned(), size)));
                pending_candidates.push((
                    SearchCandidate {
                        node_id: id,
                        source,
                        scope,
                        bytes,
                        version: SearchVersion {
                            binding: binding.clone(),
                            node_id: id,
                            source,
                            scope,
                            remote: node.clone(),
                            generation: row.try_get("generation").map_err(storage_error)?,
                            base_revision: base_rev,
                            pending,
                            confirmed,
                            cache_revision: cache_rev,
                            cache_hash,
                            cache_size,
                            cache_len,
                        },
                    },
                    source == SearchSource::Draft && dirty,
                    expected_hash,
                ));
            }
            last = Some(id);
        }
        tx.commit().await.map_err(storage_error)?;
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        for (candidate, skip_remote_hash, expected_hash) in pending_candidates {
            if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
                return Ok(None);
            }
            let valid_hash = if skip_remote_hash {
                true
            } else if let Some((digest, size)) = expected_hash {
                if candidate.bytes.len() != size {
                    false
                } else {
                    let Some(actual) =
                        hash_with_hook(&candidate.bytes, cancelled, &mut hash_progress).await
                    else {
                        return Ok(None);
                    };
                    actual == digest
                }
            } else {
                false
            };
            if valid_hash && std::str::from_utf8(&candidate.bytes).is_ok() {
                match candidate.scope {
                    SearchScope::Workspace => result.available_documents += 1,
                    SearchScope::Recovery => result.recovery_available_documents += 1,
                }
                result.candidates.push(candidate);
            }
        }
        if cancelled.is_some_and(|value| value.load(Ordering::Acquire)) {
            return Ok(None);
        }
        Ok(Some(result))
    }
}
