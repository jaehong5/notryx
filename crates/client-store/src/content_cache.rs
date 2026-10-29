use crate::{ClientStore, RegistrationBinding, StoreError, storage_error};
use notryx_domain::NodeId;
use notryx_protocol::{NodeKind, NodeState, StrongEtag};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};

pub(crate) const UPGRADE_V7: &str = "
    CREATE TABLE cached_content (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        node_id TEXT NOT NULL,
        content_rev TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        size TEXT NOT NULL,
        bytes BLOB NOT NULL CHECK(length(bytes) <= 26214400),
        last_access INTEGER NOT NULL CHECK(last_access >= 0),
        PRIMARY KEY(registration_id, node_id)
    );
    CREATE INDEX cached_content_lru ON cached_content(last_access,registration_id,node_id);
    CREATE TABLE cached_content_clock (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        last_access INTEGER NOT NULL CHECK(last_access >= 0)
    );
    INSERT INTO cached_content_clock(singleton,last_access) VALUES(1,0);
    PRAGMA user_version = 7;
";

const MAX_CACHE_BYTES: usize = 25 * 1024 * 1024;
const DEFAULT_CACHE_BUDGET: usize = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedContent {
    pub content_rev: String,
    pub bytes: Vec<u8>,
    pub current: bool,
}

fn hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(71);
    text.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(&mut text, "{byte:02x}");
    }
    text
}

fn valid_cached_revision(value: &str) -> bool {
    notryx_domain::ContentRevision::new(value)
        .ok()
        .is_some_and(|revision| StrongEtag::from_content_revision(&revision).is_ok())
}

pub(crate) fn validated_content(node: &NodeState) -> Result<(String, &str, u64), StoreError> {
    let value = serde_json::to_value(node).map_err(|_| StoreError::InvalidRequest)?;
    let decoded: NodeState =
        serde_json::from_value(value).map_err(|_| StoreError::InvalidRequest)?;
    if decoded != *node || node.kind != NodeKind::File || node.deleted {
        return Err(StoreError::InvalidRequest);
    }
    let rev = node
        .content_rev
        .as_ref()
        .ok_or(StoreError::InvalidRequest)?;
    let etag = StrongEtag::from_content_revision(rev).map_err(|_| StoreError::InvalidRequest)?;
    if node.content_etag.as_deref() != Some(etag.as_str()) {
        return Err(StoreError::InvalidRequest);
    }
    let digest = node
        .content_hash
        .as_deref()
        .ok_or(StoreError::InvalidRequest)?;
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or(StoreError::InvalidRequest)?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(StoreError::InvalidRequest);
    }
    let size = node
        .size
        .as_deref()
        .ok_or(StoreError::InvalidRequest)?
        .parse::<u64>()
        .map_err(|_| StoreError::InvalidRequest)?;
    Ok((rev.as_str().to_owned(), digest, size))
}

async fn next_access(connection: &mut SqliteConnection) -> Result<i64, StoreError> {
    sqlx::query_scalar("UPDATE cached_content_clock SET last_access=last_access+1 WHERE singleton=1 AND last_access<9223372036854775807 RETURNING last_access")
        .fetch_optional(connection)
        .await
        .map_err(storage_error)?
        .ok_or(StoreError::Storage)
}

async fn make_room(
    connection: &mut SqliteConnection,
    registration: i64,
    node_id: &str,
    incoming_bytes: usize,
    budget: usize,
) -> Result<(), StoreError> {
    if incoming_bytes > budget {
        return Err(StoreError::DocumentTooLarge);
    }
    let total: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(length(bytes)),0) FROM cached_content")
            .fetch_one(&mut *connection)
            .await
            .map_err(storage_error)?;
    let replaced: Option<i64> = sqlx::query_scalar(
        "SELECT length(bytes) FROM cached_content WHERE registration_id=? AND node_id=?",
    )
    .bind(registration)
    .bind(node_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(storage_error)?;
    let mut projected = u64::try_from(total)
        .map_err(|_| StoreError::UnsupportedFormat)?
        .checked_sub(
            u64::try_from(replaced.unwrap_or(0)).map_err(|_| StoreError::UnsupportedFormat)?,
        )
        .and_then(|n| n.checked_add(incoming_bytes as u64))
        .ok_or(StoreError::Storage)?;
    while projected > budget as u64 {
        let oldest = sqlx::query("SELECT registration_id,node_id,length(bytes) AS byte_count FROM cached_content WHERE registration_id<>? OR node_id<>? ORDER BY last_access,registration_id,node_id LIMIT 1")
            .bind(registration).bind(node_id).fetch_optional(&mut *connection).await.map_err(storage_error)?
            .ok_or(StoreError::Storage)?;
        let old_registration: i64 = oldest.try_get("registration_id").map_err(storage_error)?;
        let old_node: String = oldest.try_get("node_id").map_err(storage_error)?;
        let bytes: i64 = oldest.try_get("byte_count").map_err(storage_error)?;
        sqlx::query("DELETE FROM cached_content WHERE registration_id=? AND node_id=?")
            .bind(old_registration)
            .bind(&old_node)
            .execute(&mut *connection)
            .await
            .map_err(storage_error)?;
        sqlx::query(
            "UPDATE remote_nodes SET content_ready=0 WHERE registration_id=? AND node_id=?",
        )
        .bind(old_registration)
        .bind(&old_node)
        .execute(&mut *connection)
        .await
        .map_err(storage_error)?;
        projected = projected
            .checked_sub(u64::try_from(bytes).map_err(|_| StoreError::UnsupportedFormat)?)
            .ok_or(StoreError::UnsupportedFormat)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteFileObservation {
    Pending,
    Missing,
    Live(NodeState),
    Deleted(NodeState),
}

impl ClientStore {
    pub async fn observe_remote_file(
        &mut self,
        binding: &RegistrationBinding,
        node: NodeId,
    ) -> Result<RemoteFileObservation, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        observe_remote_file_on_connection(&mut self.connection, registration, node).await
    }

    pub async fn current_remote_file(
        &mut self,
        binding: &RegistrationBinding,
        node: NodeId,
    ) -> Result<Option<NodeState>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(node.to_string())
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let current: NodeState =
            serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
        if current.node_id != node {
            return Err(StoreError::UnsupportedFormat);
        }
        if current.deleted || current.kind != NodeKind::File {
            return Ok(None);
        }
        validated_content(&current).map_err(|_| StoreError::UnsupportedFormat)?;
        Ok(Some(current))
    }

    pub async fn remote_content_matches(
        &mut self,
        binding: &RegistrationBinding,
        expected: &NodeState,
    ) -> Result<bool, StoreError> {
        let (revision, digest, size) = validated_content(expected)?;
        let registration = self.registration_id(binding, true).await?;
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(expected.node_id.to_string())
        .fetch_optional(&mut self.connection)
        .await
        .map_err(storage_error)?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let current: NodeState =
            serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
        if current.node_id != expected.node_id {
            return Err(StoreError::UnsupportedFormat);
        }
        if current.deleted || current.kind != NodeKind::File {
            return Ok(false);
        }
        let (current_revision, current_hash, current_size) =
            validated_content(&current).map_err(|_| StoreError::UnsupportedFormat)?;
        Ok(current_revision == revision && current_hash == digest && current_size == size)
    }

    pub async fn reuse_cached_content(
        &mut self,
        binding: &RegistrationBinding,
        node_id: NodeId,
    ) -> Result<bool, StoreError> {
        self.reuse_cached_content_with_budget(binding, node_id, DEFAULT_CACHE_BUDGET)
            .await
    }

    async fn reuse_cached_content_with_budget(
        &mut self,
        binding: &RegistrationBinding,
        node_id: NodeId,
        budget: usize,
    ) -> Result<bool, StoreError> {
        if self
            .read_cached_content(binding, node_id)
            .await?
            .is_some_and(|cached| cached.current)
        {
            return Ok(true);
        }
        let registration = self.registration_id(binding, true).await?;
        let id = node_id.to_string();
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let Some(raw) = raw else { return Ok(false) };
        let node: NodeState =
            serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
        if node.node_id != node_id {
            return Err(StoreError::UnsupportedFormat);
        }
        if node.deleted || node.kind != NodeKind::File {
            return Ok(false);
        }
        let (revision, digest, size) =
            validated_content(&node).map_err(|_| StoreError::UnsupportedFormat)?;
        if size > MAX_CACHE_BYTES as u64 {
            return Ok(false);
        }
        let candidate = sqlx::query("SELECT cc.registration_id,cc.content_rev,cc.content_hash,cc.size,cc.bytes FROM cached_content cc JOIN client_registrations cr ON cr.id=cc.registration_id WHERE cr.workspace_id=? AND cc.registration_id<>? AND cc.node_id=? AND cc.content_hash=? ORDER BY cc.registration_id DESC LIMIT 1")
            .bind(&binding.workspace_id).bind(registration).bind(&id).bind(digest)
            .fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let previous_registration: i64 = candidate
            .try_get("registration_id")
            .map_err(storage_error)?;
        let previous_revision: String = candidate.try_get("content_rev").map_err(storage_error)?;
        let previous_digest: String = candidate.try_get("content_hash").map_err(storage_error)?;
        let previous_size: String = candidate.try_get("size").map_err(storage_error)?;
        let bytes: Vec<u8> = candidate.try_get("bytes").map_err(storage_error)?;
        let valid = valid_cached_revision(&previous_revision)
            && previous_size.parse::<u64>().ok() == Some(size)
            && bytes.len() as u64 == size
            && previous_digest == digest
            && hash(&bytes) == digest;
        if !valid {
            sqlx::query(
                "UPDATE remote_nodes SET content_ready=0 WHERE registration_id=? AND node_id=?",
            )
            .bind(previous_registration)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            tx.commit().await.map_err(storage_error)?;
            return Err(StoreError::UnsupportedFormat);
        }
        make_room(&mut tx, registration, &id, bytes.len(), budget).await?;
        let access = next_access(&mut tx).await?;
        sqlx::query("INSERT INTO cached_content(registration_id,node_id,content_rev,content_hash,size,bytes,last_access) VALUES(?,?,?,?,?,?,?) ON CONFLICT(registration_id,node_id) DO UPDATE SET content_rev=excluded.content_rev,content_hash=excluded.content_hash,size=excluded.size,bytes=excluded.bytes,last_access=excluded.last_access")
            .bind(registration).bind(&id).bind(revision).bind(digest).bind(size.to_string()).bind(bytes)
            .bind(access)
            .execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query(
            "UPDATE remote_nodes SET content_ready=1 WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    pub async fn cache_content(
        &mut self,
        binding: &RegistrationBinding,
        expected: &NodeState,
        bytes: &[u8],
    ) -> Result<bool, StoreError> {
        self.cache_content_with_budget(binding, expected, bytes, DEFAULT_CACHE_BUDGET)
            .await
    }

    async fn cache_content_with_budget(
        &mut self,
        binding: &RegistrationBinding,
        expected: &NodeState,
        bytes: &[u8],
        budget: usize,
    ) -> Result<bool, StoreError> {
        if bytes.len() > MAX_CACHE_BYTES {
            return Err(StoreError::DocumentTooLarge);
        }
        let (revision, digest, size) = validated_content(expected)?;
        if size != bytes.len() as u64 || hash(bytes) != digest {
            return Err(StoreError::InvalidRequest);
        }
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT state FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(expected.node_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let current: NodeState =
            serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
        if current.node_id != expected.node_id {
            return Err(StoreError::UnsupportedFormat);
        }
        if current.deleted || current.kind != NodeKind::File {
            return Ok(false);
        }
        let (current_rev, current_hash, current_size) =
            validated_content(&current).map_err(|_| StoreError::UnsupportedFormat)?;
        if current_rev != revision || current_hash != digest || current_size != size {
            return Ok(false);
        }
        make_room(
            &mut tx,
            registration,
            &expected.node_id.to_string(),
            bytes.len(),
            budget,
        )
        .await?;
        let access = next_access(&mut tx).await?;
        sqlx::query("INSERT INTO cached_content(registration_id,node_id,content_rev,content_hash,size,bytes,last_access) VALUES(?,?,?,?,?,?,?) ON CONFLICT(registration_id,node_id) DO UPDATE SET content_rev=excluded.content_rev,content_hash=excluded.content_hash,size=excluded.size,bytes=excluded.bytes,last_access=excluded.last_access")
            .bind(registration).bind(expected.node_id.to_string()).bind(revision)
            .bind(digest).bind(size.to_string()).bind(bytes).bind(access).execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query(
            "UPDATE remote_nodes SET content_ready=1 WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(expected.node_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    pub async fn read_cached_content(
        &mut self,
        binding: &RegistrationBinding,
        node_id: NodeId,
    ) -> Result<Option<CachedContent>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let id = node_id.to_string();
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let cached = sqlx::query("SELECT content_rev,content_hash,size,bytes FROM cached_content WHERE registration_id=? AND node_id=?")
            .bind(registration).bind(&id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let remote = sqlx::query(
            "SELECT state,content_ready FROM remote_nodes WHERE registration_id=? AND node_id=?",
        )
        .bind(registration)
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let Some(row) = cached else {
            if remote
                .as_ref()
                .is_some_and(|r| r.try_get::<i64, _>("content_ready").ok() == Some(1))
            {
                sqlx::query(
                    "UPDATE remote_nodes SET content_ready=0 WHERE registration_id=? AND node_id=?",
                )
                .bind(registration)
                .bind(&id)
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
                tx.commit().await.map_err(storage_error)?;
            }
            return Ok(None);
        };
        let content_rev: String = row.try_get("content_rev").map_err(storage_error)?;
        let digest: String = row.try_get("content_hash").map_err(storage_error)?;
        let size: String = row.try_get("size").map_err(storage_error)?;
        let bytes: Vec<u8> = row.try_get("bytes").map_err(storage_error)?;
        let valid = valid_cached_revision(&content_rev)
            && size.parse::<usize>().ok() == Some(bytes.len())
            && bytes.len() <= MAX_CACHE_BYTES
            && hash(&bytes) == digest;
        if !valid {
            sqlx::query(
                "UPDATE remote_nodes SET content_ready=0 WHERE registration_id=? AND node_id=?",
            )
            .bind(registration)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            tx.commit().await.map_err(storage_error)?;
            return Err(StoreError::UnsupportedFormat);
        }
        let (current, ready) = if let Some(remote) = remote {
            let raw: String = remote.try_get("state").map_err(storage_error)?;
            let node: NodeState =
                serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
            if node.node_id != node_id {
                return Err(StoreError::UnsupportedFormat);
            }
            let ready: i64 = remote.try_get("content_ready").map_err(storage_error)?;
            if !matches!(ready, 0 | 1) {
                return Err(StoreError::UnsupportedFormat);
            }
            let current = if !node.deleted && node.kind == NodeKind::File {
                let (rev, remote_digest, remote_size) =
                    validated_content(&node).map_err(|_| StoreError::UnsupportedFormat)?;
                rev == content_rev && remote_digest == digest && remote_size == bytes.len() as u64
            } else {
                false
            };
            (current, Some(ready))
        } else {
            (false, None)
        };
        if ready.is_some_and(|value| value != i64::from(current)) {
            sqlx::query(
                "UPDATE remote_nodes SET content_ready=? WHERE registration_id=? AND node_id=?",
            )
            .bind(current)
            .bind(registration)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }
        let access = next_access(&mut tx).await?;
        sqlx::query(
            "UPDATE cached_content SET last_access=? WHERE registration_id=? AND node_id=?",
        )
        .bind(access)
        .bind(registration)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(Some(CachedContent {
            content_rev,
            bytes,
            current,
        }))
    }
}

pub(crate) async fn observe_remote_file_on_connection(
    connection: &mut SqliteConnection,
    registration: i64,
    node: NodeId,
) -> Result<RemoteFileObservation, StoreError> {
    let row = sqlx::query(
            "SELECT i.cursor,i.root_id,n.state,r.state AS root_state FROM remote_index i LEFT JOIN remote_nodes n ON n.registration_id=i.registration_id AND n.node_id=? LEFT JOIN remote_nodes r ON r.registration_id=i.registration_id AND r.node_id=i.root_id WHERE i.registration_id=?",
        )
        .bind(node.to_string())
        .bind(registration)
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage_error)?;
    let Some(row) = row else {
        return Ok(RemoteFileObservation::Pending);
    };
    let cursor: String = row.try_get("cursor").map_err(storage_error)?;
    let cursor_number = cursor
        .parse::<u64>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if cursor_number.to_string() != cursor {
        return Err(StoreError::UnsupportedFormat);
    }
    let root: String = row.try_get("root_id").map_err(storage_error)?;
    let root_id = root
        .parse::<NodeId>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    let root_state: Option<String> = row.try_get("root_state").map_err(storage_error)?;
    let root_state = root_state.ok_or(StoreError::UnsupportedFormat)?;
    let root_node: NodeState =
        serde_json::from_str(&root_state).map_err(|_| StoreError::UnsupportedFormat)?;
    if root_node.node_id != root_id
        || root_node.parent_id.is_some()
        || root_node.kind != NodeKind::Folder
        || root_node.deleted
        || !root_node.name.is_empty()
        || root_node.last_change_seq.get() > cursor_number
    {
        return Err(StoreError::UnsupportedFormat);
    }
    crate::bootstrap::encode_node(&root_node).map_err(|_| StoreError::UnsupportedFormat)?;
    let raw: Option<String> = row.try_get("state").map_err(storage_error)?;
    let Some(raw) = raw else {
        return Ok(RemoteFileObservation::Missing);
    };
    let current: NodeState =
        serde_json::from_str(&raw).map_err(|_| StoreError::UnsupportedFormat)?;
    if current.node_id != node
        || current.node_id == root_id
        || current.kind != NodeKind::File
        || current.parent_id.is_none()
        || current.last_change_seq.get() == 0
        || current.last_change_seq.get() > cursor_number
    {
        return Err(StoreError::UnsupportedFormat);
    }
    crate::bootstrap::encode_node(&current).map_err(|_| StoreError::UnsupportedFormat)?;
    if current.deleted {
        Ok(RemoteFileObservation::Deleted(current))
    } else {
        Ok(RemoteFileObservation::Live(current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notryx_protocol::{InventoryPage, ServerInfo};
    use serde_json::json;
    use sqlx::SqliteConnection;
    use sqlx::sqlite::SqliteConnectOptions;
    use uuid::Uuid;

    const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
    const IDS: [&str; 4] = [
        "019a1000-0000-7000-8000-000000000002",
        "019a1000-0000-7000-8000-000000000003",
        "019a1000-0000-7000-8000-000000000004",
        "019a1000-0000-7000-8000-000000000005",
    ];

    async fn setup() -> (
        tempfile::TempDir,
        ClientStore,
        RegistrationBinding,
        Vec<NodeState>,
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
        let nodes: Vec<NodeState> = IDS.iter().enumerate().map(|(i, id)| {
            let bytes = vec![b'a' + i as u8; 2];
            serde_json::from_value(json!({
                "node_id": id, "parent_id": ROOT, "name": format!("{i}.md"), "kind": "file",
                "deleted": false, "content_rev": format!("content-{i}"), "content_etag": format!("\"content-{i}\""),
                "meta_rev": format!("meta-{i}"), "meta_etag": format!("\"meta-{i}\""),
                "node_etag": format!("\"node-{i}\""), "content_hash": hash(&bytes),
                "size": "2", "children_generation": "0", "last_change_seq": "1"
            })).unwrap()
        }).collect();
        let mut store = ClientStore::open(directory.path().join("drafts.db"))
            .await
            .unwrap();
        store.activate_registration(&binding).await.unwrap();
        let session = store.begin_bootstrap(&binding, &info).await.unwrap();
        let mut inventory = vec![root];
        inventory.extend(nodes.iter().cloned());
        store
            .bootstrap_inventory(
                &binding,
                &session,
                &InventoryPage {
                    workspace_epoch: info.workspace_epoch,
                    nodes: inventory,
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
        (directory, store, binding, nodes)
    }

    #[tokio::test]
    async fn cache_eviction_invalidates_search_token_and_previous_candidate() {
        let (_directory, mut store, binding, nodes) = setup().await;
        for (node, bytes) in [(&nodes[0], b"aa"), (&nodes[1], b"bb")] {
            assert!(
                store
                    .cache_content_with_budget(&binding, node, bytes, 4)
                    .await
                    .unwrap()
            );
        }
        let page = store.search_candidates(&binding, None, 100).await.unwrap();
        let first = page
            .candidates
            .iter()
            .find(|candidate| candidate.node_id == nodes[0].node_id)
            .unwrap();
        assert_eq!(first.bytes(), b"aa");
        let before = store.search_mutation_token().await.unwrap();
        assert!(
            store
                .cache_content_with_budget(&binding, &nodes[2], b"cc", 4)
                .await
                .unwrap()
        );
        let after = store.search_mutation_token().await.unwrap();
        assert_ne!(before, after);
        assert!(
            !store
                .search_candidate_is_current(&binding, first)
                .await
                .unwrap()
        );
        assert!(
            store
                .read_cached_content(&binding, nodes[0].node_id)
                .await
                .unwrap()
                .is_none()
        );
        let next = store.search_candidates(&binding, None, 100).await.unwrap();
        assert_eq!(next.available_documents, 2);
        assert_eq!(next.total_documents, 4);
        assert!(
            !next
                .candidates
                .iter()
                .any(|candidate| candidate.node_id == nodes[0].node_id)
        );
    }

    #[tokio::test]
    async fn clean_cache_lru_evicts_oldest_and_keeps_draft_base_and_outbox() {
        let (_directory, mut store, binding, nodes) = setup().await;
        let opened = store
            .open_registered_document(&binding, IDS[1], b"bb", "content-1")
            .await
            .unwrap();
        store
            .save_registered_checkpoint(&binding, IDS[1], &opened.edit_session_id, 1, b"user edit")
            .await
            .unwrap();
        store
            .close_registered_document(&binding, IDS[1], &opened.edit_session_id)
            .await
            .unwrap();
        let queued = store.queue_checkpoint(&binding, IDS[1], 1).await.unwrap();
        for (i, node) in nodes.iter().take(3).enumerate() {
            let bytes = vec![b'a' + i as u8; 2];
            assert_eq!(
                store
                    .cache_content_with_budget(&binding, node, &bytes, 6)
                    .await,
                Ok(true)
            );
        }
        assert!(
            store
                .read_cached_content(&binding, nodes[0].node_id)
                .await
                .unwrap()
                .unwrap()
                .current
        );
        assert_eq!(
            store
                .cache_content_with_budget(&binding, &nodes[3], b"dd", 6)
                .await,
            Ok(true)
        );
        assert!(
            store
                .read_cached_content(&binding, nodes[1].node_id)
                .await
                .unwrap()
                .is_none()
        );
        for (i, node) in nodes.iter().enumerate().filter(|(i, _)| *i != 1) {
            let cached = store
                .read_cached_content(&binding, node.node_id)
                .await
                .unwrap()
                .unwrap();
            assert!(cached.current);
            assert_eq!(cached.bytes, vec![b'a' + i as u8; 2]);
        }
        let ready: i64 =
            sqlx::query_scalar("SELECT content_ready FROM remote_nodes WHERE node_id=?")
                .bind(IDS[1])
                .fetch_one(&mut store.connection)
                .await
                .unwrap();
        assert_eq!(ready, 0);
        let draft = store
            .read_registered_checkpoint(&binding, IDS[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"user edit");
        assert_eq!(draft.base_bytes, b"bb");
        assert_eq!(
            store
                .read_outbox_operation(&binding, &queued.operation_id)
                .await
                .unwrap()
                .unwrap(),
            queued
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn cache_eviction_rolls_back_when_new_content_cannot_be_published() {
        let (directory, mut store, binding, nodes) = setup().await;
        assert_eq!(
            store
                .cache_content_with_budget(&binding, &nodes[0], b"aa", 4)
                .await,
            Ok(true)
        );
        assert_eq!(
            store
                .cache_content_with_budget(&binding, &nodes[1], b"bb", 4)
                .await,
            Ok(true)
        );
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(directory.path().join("drafts.db")),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_cache_before_insert BEFORE INSERT ON cached_content WHEN NEW.node_id='019a1000-0000-7000-8000-000000000004' BEGIN SELECT RAISE(ABORT, 'synthetic'); END")
            .execute(&mut db).await.unwrap();
        assert_eq!(
            store
                .cache_content_with_budget(&binding, &nodes[2], b"cc", 4)
                .await,
            Err(StoreError::Storage)
        );
        for (node, bytes) in [(nodes[0].node_id, b"aa"), (nodes[1].node_id, b"bb")] {
            let cached = store
                .read_cached_content(&binding, node)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(cached.bytes, bytes);
            assert!(cached.current);
        }
        assert!(
            store
                .read_cached_content(&binding, nodes[2].node_id)
                .await
                .unwrap()
                .is_none()
        );
        db.close().await.unwrap();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn reuse_across_registration_obeys_the_same_clean_cache_budget() {
        let (_directory, mut store, old, nodes) = setup().await;
        assert_eq!(
            store
                .cache_content_with_budget(&old, &nodes[0], b"aa", 4)
                .await,
            Ok(true)
        );
        assert_eq!(
            store
                .cache_content_with_budget(&old, &nodes[1], b"bb", 4)
                .await,
            Ok(true)
        );
        let replacement = RegistrationBinding {
            server_origin: old.server_origin.clone(),
            workspace_id: old.workspace_id.clone(),
            workspace_epoch: Uuid::new_v4().to_string(),
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
        };
        store.activate_registration(&replacement).await.unwrap();
        let info: ServerInfo = serde_json::from_value(json!({
            "protocol": "mdw-sync/1.2", "workspace_id": replacement.workspace_id,
            "workspace_epoch": replacement.workspace_epoch, "root_id": ROOT,
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
        let session = store.begin_bootstrap(&replacement, &info).await.unwrap();
        let mut inventory = vec![root];
        inventory.extend(nodes.iter().cloned());
        store
            .bootstrap_inventory(
                &replacement,
                &session,
                &InventoryPage {
                    workspace_epoch: info.workspace_epoch,
                    nodes: inventory,
                    next_after_id: None,
                },
            )
            .await
            .unwrap();
        store
            .bootstrap_finish_inventory(&replacement, &session, &info)
            .await
            .unwrap();
        store
            .bootstrap_publish(&replacement, &session, &info)
            .await
            .unwrap();
        assert_eq!(
            store
                .reuse_cached_content_with_budget(&replacement, nodes[0].node_id, 4)
                .await,
            Ok(true)
        );
        let current = store
            .read_cached_content(&replacement, nodes[0].node_id)
            .await
            .unwrap()
            .unwrap();
        assert!(current.current);
        assert_eq!(current.bytes, b"aa");
        let total: i64 = sqlx::query_scalar("SELECT SUM(length(bytes)) FROM cached_content")
            .fetch_one(&mut store.connection)
            .await
            .unwrap();
        assert_eq!(total, 4);
        let old_ready: i64 = sqlx::query_scalar("SELECT r.content_ready FROM remote_nodes r JOIN client_registrations c ON c.id=r.registration_id WHERE c.device_id=? AND r.node_id=?")
            .bind(&old.device_id).bind(IDS[0]).fetch_one(&mut store.connection).await.unwrap();
        assert_eq!(old_ready, 0);
        store.close().await.unwrap();
    }
}
