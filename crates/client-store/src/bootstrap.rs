use crate::{ClientStore, RegistrationBinding, StoreError, storage_error};
use notryx_domain::{ChangeSeq, mutation::normalized_name_key};
use notryx_protocol::{ChangesPage, InventoryPage, NodeKind, NodeState, ServerInfo, StrongEtag};
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub(crate) const UPGRADE_V6: &str = "
    CREATE TABLE sync_bootstrap (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        session TEXT NOT NULL UNIQUE,
        root_id TEXT NOT NULL,
        h0 TEXT NOT NULL,
        h1 TEXT,
        cursor TEXT NOT NULL,
        inventory_after TEXT,
        inventory_done INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE bootstrap_nodes (
        registration_id INTEGER NOT NULL REFERENCES sync_bootstrap(registration_id) ON DELETE CASCADE,
        node_id TEXT NOT NULL,
        state TEXT NOT NULL,
        PRIMARY KEY(registration_id, node_id)
    );
    CREATE TABLE remote_index (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        root_id TEXT NOT NULL,
        cursor TEXT NOT NULL
    );
    CREATE TABLE remote_nodes (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        node_id TEXT NOT NULL,
        state TEXT NOT NULL,
        content_ready INTEGER NOT NULL DEFAULT 0 CHECK(content_ready IN (0, 1)),
        PRIMARY KEY(registration_id, node_id)
    );
    PRAGMA user_version=6;
";

fn invalid() -> StoreError {
    StoreError::InvalidRequest
}

fn decimal(value: &str) -> Result<u64, StoreError> {
    let number = value
        .parse::<u64>()
        .map_err(|_| StoreError::UnsupportedFormat)?;
    if number.to_string() != value {
        return Err(StoreError::UnsupportedFormat);
    }
    Ok(number)
}

pub(crate) fn encode_node(node: &NodeState) -> Result<String, StoreError> {
    let json = serde_json::to_string(node).map_err(|_| invalid())?;
    let decoded: NodeState = serde_json::from_str(&json).map_err(|_| invalid())?;
    if decoded != *node {
        return Err(invalid());
    }
    if StrongEtag::from_meta_revision(&node.meta_rev)
        .map_err(|_| invalid())?
        .as_str()
        != node.meta_etag
        || StrongEtag::parse(&node.node_etag).is_err()
    {
        return Err(invalid());
    }
    if let Some(revision) = &node.content_rev {
        if node.content_etag.as_deref()
            != Some(
                StrongEtag::from_content_revision(revision)
                    .map_err(|_| invalid())?
                    .as_str(),
            )
        {
            return Err(invalid());
        }
        let hash = node
            .content_hash
            .as_deref()
            .and_then(|value| value.strip_prefix("sha256:"))
            .ok_or_else(invalid)?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid());
        }
    }
    Ok(json)
}

fn equal_size(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        },
        _ => false,
    }
}

fn equal_state(a: &NodeState, b: &NodeState) -> bool {
    if !equal_size(a.size.as_deref(), b.size.as_deref()) {
        return false;
    }
    let mut normalized = a.clone();
    normalized.size.clone_from(&b.size);
    normalized == *b
}

fn validate_observations(a: &NodeState, b: &NodeState) -> Result<(), StoreError> {
    if a.node_id != b.node_id || a.kind != b.kind {
        return Err(invalid());
    }
    if a.content_rev.is_some()
        && a.content_rev == b.content_rev
        && (a.content_hash != b.content_hash
            || !equal_size(a.size.as_deref(), b.size.as_deref())
            || a.deleted != b.deleted)
    {
        return Err(invalid());
    }
    if a.meta_rev == b.meta_rev
        && (a.parent_id != b.parent_id || a.name != b.name || a.deleted != b.deleted)
    {
        return Err(invalid());
    }
    if a.kind == NodeKind::Folder
        && ((a.last_change_seq < b.last_change_seq
            && a.children_generation > b.children_generation)
            || (a.last_change_seq > b.last_change_seq
                && a.children_generation < b.children_generation))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_info(binding: &RegistrationBinding, info: &ServerInfo) -> Result<(), StoreError> {
    if info.protocol != notryx_protocol::CONTRACT
        || !info.ready
        || info.workspace_id.to_string() != binding.workspace_id
        || info.workspace_epoch.to_string() != binding.workspace_epoch
        || info.journal_floor_seq > info.head_seq
    {
        return Err(invalid());
    }
    Ok(())
}

async fn stage_row(
    connection: &mut SqliteConnection,
    registration: i64,
    session: &str,
) -> Result<sqlx::sqlite::SqliteRow, StoreError> {
    sqlx::query("SELECT root_id,h0,h1,cursor,inventory_after,inventory_done FROM sync_bootstrap WHERE registration_id=? AND session=?")
        .bind(registration).bind(session).fetch_optional(connection).await.map_err(storage_error)?
        .ok_or(StoreError::StaleSession)
}

async fn apply_stage_node(
    connection: &mut SqliteConnection,
    registration: i64,
    node: &NodeState,
) -> Result<(), StoreError> {
    let encoded = encode_node(node)?;
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT state FROM bootstrap_nodes WHERE registration_id=? AND node_id=?",
    )
    .bind(registration)
    .bind(node.node_id.to_string())
    .fetch_optional(&mut *connection)
    .await
    .map_err(storage_error)?;
    if let Some(previous) = previous {
        let old: NodeState =
            serde_json::from_str(&previous).map_err(|_| StoreError::UnsupportedFormat)?;
        validate_observations(&old, node)?;
        if old.last_change_seq > node.last_change_seq {
            return Ok(());
        }
        if old.last_change_seq == node.last_change_seq {
            return if equal_state(&old, node) {
                Ok(())
            } else {
                Err(invalid())
            };
        }
    }
    sqlx::query("INSERT INTO bootstrap_nodes(registration_id,node_id,state) VALUES(?,?,?) ON CONFLICT(registration_id,node_id) DO UPDATE SET state=excluded.state")
        .bind(registration).bind(node.node_id.to_string()).bind(encoded).execute(connection).await.map_err(storage_error)?;
    Ok(())
}

pub(crate) fn decode_rows(rows: &[sqlx::sqlite::SqliteRow]) -> Result<Vec<NodeState>, StoreError> {
    rows.iter()
        .map(|row| {
            let id: &str = row.try_get("node_id").map_err(storage_error)?;
            let raw: &str = row.try_get("state").map_err(storage_error)?;
            let node: NodeState =
                serde_json::from_str(raw).map_err(|_| StoreError::UnsupportedFormat)?;
            if node.node_id.to_string() != id || encode_node(&node).is_err() {
                return Err(StoreError::UnsupportedFormat);
            }
            Ok(node)
        })
        .collect()
}

pub(crate) fn validate_tree(root_id: &str, nodes: &[NodeState]) -> Result<(), StoreError> {
    let by_id: HashMap<String, &NodeState> =
        nodes.iter().map(|n| (n.node_id.to_string(), n)).collect();
    if by_id.len() != nodes.len() {
        return Err(invalid());
    }
    let root = by_id.get(root_id).ok_or_else(invalid)?;
    if root.deleted
        || root.kind != NodeKind::Folder
        || root.parent_id.is_some()
        || !root.name.is_empty()
    {
        return Err(invalid());
    }
    if nodes
        .iter()
        .any(|node| node.node_id.to_string() != root_id && node.last_change_seq.get() == 0)
    {
        return Err(invalid());
    }
    let mut names = HashSet::new();
    for node in nodes.iter().filter(|node| !node.deleted) {
        if node.node_id.to_string() == root_id {
            continue;
        }
        let parent = node.parent_id.ok_or_else(invalid)?.to_string();
        let parent_node = by_id.get(&parent).ok_or_else(invalid)?;
        if parent_node.deleted
            || parent_node.kind != NodeKind::Folder
            || node.last_change_seq.get() == 0
        {
            return Err(invalid());
        }
        if !names.insert((parent, normalized_name_key(&node.name))) {
            return Err(invalid());
        }
    }
    let mut verified = HashSet::new();
    verified.insert(root_id.to_owned());
    for node in nodes.iter().filter(|node| !node.deleted) {
        let mut path = HashSet::new();
        let mut current = node.node_id.to_string();
        while !verified.contains(&current) {
            if !path.insert(current.clone()) {
                return Err(invalid());
            }
            let parent = by_id
                .get(&current)
                .and_then(|n| n.parent_id)
                .ok_or_else(invalid)?;
            current = parent.to_string();
        }
        verified.extend(path);
    }
    Ok(())
}

impl ClientStore {
    pub async fn apply_changes(
        &mut self,
        binding: &RegistrationBinding,
        page: &ChangesPage,
        through: ChangeSeq,
    ) -> Result<(), StoreError> {
        let registration = self.registration_id(binding, true).await?;
        if page.workspace_epoch.to_string() != binding.workspace_epoch
            || page.through_seq != through
            || serde_json::to_vec(page).map_err(|_| invalid())?.len() > 1024 * 1024
        {
            return Err(invalid());
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let index = sqlx::query("SELECT root_id,cursor FROM remote_index WHERE registration_id=?")
            .bind(registration)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?
            .ok_or(StoreError::ReconciliationRequired)?;
        let root: String = index.try_get("root_id").map_err(storage_error)?;
        let cursor = decimal(index.try_get("cursor").map_err(storage_error)?)?;
        let covered = page.covered_through.get();
        if page.from_exclusive.get() != cursor
            || through.get() < cursor
            || covered < cursor
            || covered > through.get()
            || covered - cursor > 100
            || page.has_more != (covered < through.get())
            || (page.has_more && covered == cursor)
        {
            return Err(invalid());
        }
        let rows = sqlx::query(
            "SELECT node_id,state FROM remote_nodes WHERE registration_id=? ORDER BY node_id",
        )
        .bind(registration)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage_error)?;
        let mut nodes = decode_rows(&rows)?;
        if nodes.iter().any(|node| node.last_change_seq.get() > cursor)
            || validate_tree(&root, &nodes).is_err()
        {
            return Err(StoreError::UnsupportedFormat);
        }
        let mut positions: HashMap<_, _> = nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.node_id, i))
            .collect();
        let mut changed = HashSet::new();
        let mut invalidate_content = HashSet::new();
        let mut sequence = cursor;
        let mut i = 0;
        while i < page.nodes.len() {
            let seq = page.nodes[i].last_change_seq.get();
            if sequence.checked_add(1) != Some(seq) || seq > covered {
                return Err(invalid());
            }
            sequence = seq;
            let mut group_ids = HashSet::new();
            while i < page.nodes.len() && page.nodes[i].last_change_seq.get() == seq {
                let node = &page.nodes[i];
                encode_node(node)?;
                if !group_ids.insert(node.node_id) {
                    return Err(invalid());
                }
                if let Some(&position) = positions.get(&node.node_id) {
                    let old = &nodes[position];
                    if old.last_change_seq >= node.last_change_seq {
                        return Err(invalid());
                    }
                    validate_observations(old, node)?;
                    if old.content_rev != node.content_rev || old.deleted != node.deleted {
                        invalidate_content.insert(node.node_id);
                    }
                    nodes[position] = node.clone();
                } else {
                    positions.insert(node.node_id, nodes.len());
                    nodes.push(node.clone());
                    invalidate_content.insert(node.node_id);
                }
                changed.insert(node.node_id);
                i += 1;
            }
            validate_tree(&root, &nodes)?;
        }
        if sequence != covered {
            return Err(invalid());
        }
        for id in changed {
            let node = &nodes[positions[&id]];
            let json = encode_node(node)?;
            sqlx::query("INSERT INTO remote_nodes(registration_id,node_id,state,content_ready) VALUES(?,?,?,0) ON CONFLICT(registration_id,node_id) DO UPDATE SET state=excluded.state,content_ready=CASE WHEN ? THEN 0 ELSE remote_nodes.content_ready END")
                .bind(registration).bind(id.to_string()).bind(json)
                .bind(invalidate_content.contains(&id))
                .execute(&mut *tx).await.map_err(storage_error)?;
        }
        sqlx::query("UPDATE remote_index SET cursor=? WHERE registration_id=?")
            .bind(covered.to_string())
            .bind(registration)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    pub async fn begin_bootstrap(
        &mut self,
        binding: &RegistrationBinding,
        info: &ServerInfo,
    ) -> Result<String, StoreError> {
        validate_info(binding, info)?;
        let registration = self.registration_id(binding, true).await?;
        let previous = sqlx::query("SELECT i.root_id,i.cursor FROM remote_index i JOIN client_registrations r ON r.id=i.registration_id WHERE r.workspace_id=? AND r.workspace_epoch=?")
            .bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .fetch_all(&mut self.connection).await.map_err(storage_error)?;
        for row in previous {
            let root: String = row.try_get("root_id").map_err(storage_error)?;
            let cursor = decimal(row.try_get("cursor").map_err(storage_error)?)?;
            if root != info.root_id.to_string() || cursor > info.head_seq.get() {
                return Err(StoreError::ReconciliationRequired);
            }
        }
        let session = Uuid::new_v4().to_string();
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        sqlx::query("DELETE FROM sync_bootstrap WHERE registration_id=?")
            .bind(registration)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        sqlx::query("INSERT INTO sync_bootstrap(registration_id,session,root_id,h0,cursor) VALUES(?,?,?,?,?)")
            .bind(registration).bind(&session).bind(info.root_id.to_string())
            .bind(info.head_seq.get().to_string()).bind(info.head_seq.get().to_string())
            .execute(&mut *tx).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(session)
    }

    pub async fn bootstrap_inventory(
        &mut self,
        binding: &RegistrationBinding,
        session: &str,
        page: &InventoryPage,
    ) -> Result<(), StoreError> {
        let registration = self.registration_id(binding, true).await?;
        if page.workspace_epoch.to_string() != binding.workspace_epoch || page.nodes.len() > 100 {
            return Err(invalid());
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let state = stage_row(&mut tx, registration, session).await?;
        if state
            .try_get::<i64, _>("inventory_done")
            .map_err(storage_error)?
            != 0
            || state
                .try_get::<Option<String>, _>("h1")
                .map_err(storage_error)?
                .is_some()
        {
            return Err(invalid());
        }
        let mut after: Option<String> = state.try_get("inventory_after").map_err(storage_error)?;
        for node in &page.nodes {
            let id = node.node_id.to_string();
            if node.deleted || after.as_ref().is_some_and(|previous| previous >= &id) {
                return Err(invalid());
            }
            apply_stage_node(&mut tx, registration, node).await?;
            after = Some(id);
        }
        if let Some(next) = page.next_after_id
            && page.nodes.last().map(|node| node.node_id) != Some(next)
        {
            return Err(invalid());
        }
        sqlx::query(
            "UPDATE sync_bootstrap SET inventory_after=?,inventory_done=? WHERE registration_id=?",
        )
        .bind(after)
        .bind(page.next_after_id.is_none())
        .bind(registration)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    pub async fn bootstrap_finish_inventory(
        &mut self,
        binding: &RegistrationBinding,
        session: &str,
        info: &ServerInfo,
    ) -> Result<(), StoreError> {
        validate_info(binding, info)?;
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let state = stage_row(&mut tx, registration, session).await?;
        let h0 = decimal(state.try_get("h0").map_err(storage_error)?)?;
        if state
            .try_get::<i64, _>("inventory_done")
            .map_err(storage_error)?
            != 1
            || state
                .try_get::<Option<String>, _>("h1")
                .map_err(storage_error)?
                .is_some()
            || state
                .try_get::<String, _>("root_id")
                .map_err(storage_error)?
                != info.root_id.to_string()
            || info.head_seq.get() < h0
        {
            return Err(invalid());
        }
        if info.journal_floor_seq.get() > h0 {
            return Err(StoreError::BootstrapHistoryExpired);
        }
        let entries: Vec<String> =
            sqlx::query_scalar("SELECT state FROM bootstrap_nodes WHERE registration_id=?")
                .bind(registration)
                .fetch_all(&mut *tx)
                .await
                .map_err(storage_error)?;
        for entry in entries {
            let node: NodeState =
                serde_json::from_str(&entry).map_err(|_| StoreError::UnsupportedFormat)?;
            if node.last_change_seq > info.head_seq {
                return Err(invalid());
            }
        }
        sqlx::query("UPDATE sync_bootstrap SET h1=? WHERE registration_id=?")
            .bind(info.head_seq.get().to_string())
            .bind(registration)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    pub async fn bootstrap_changes(
        &mut self,
        binding: &RegistrationBinding,
        session: &str,
        page: &ChangesPage,
    ) -> Result<(), StoreError> {
        let registration = self.registration_id(binding, true).await?;
        if page.workspace_epoch.to_string() != binding.workspace_epoch
            || serde_json::to_vec(page).map_err(|_| invalid())?.len() > 1024 * 1024
        {
            return Err(invalid());
        }
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let state = stage_row(&mut tx, registration, session).await?;
        let cursor = decimal(state.try_get("cursor").map_err(storage_error)?)?;
        let h1 = decimal(
            state
                .try_get::<Option<&str>, _>("h1")
                .map_err(storage_error)?
                .ok_or_else(invalid)?,
        )?;
        let covered = page.covered_through.get();
        if page.from_exclusive.get() != cursor
            || page.through_seq.get() != h1
            || covered < cursor
            || covered > h1
            || page.has_more != (covered < h1)
            || (page.has_more && covered == cursor)
        {
            return Err(invalid());
        }
        let mut sequence = cursor;
        let mut group_nodes: HashMap<String, &NodeState> = HashMap::new();
        for node in &page.nodes {
            let seq = node.last_change_seq.get();
            if seq != sequence {
                if sequence.checked_add(1) != Some(seq) || seq > covered || seq - cursor > 100 {
                    return Err(invalid());
                }
                sequence = seq;
                group_nodes.clear();
            }
            if seq == cursor {
                return Err(invalid());
            }
            if let Some(previous) = group_nodes.insert(node.node_id.to_string(), node)
                && !equal_state(previous, node)
            {
                return Err(invalid());
            }
            apply_stage_node(&mut tx, registration, node).await?;
        }
        if sequence != covered {
            return Err(invalid());
        }
        sqlx::query("UPDATE sync_bootstrap SET cursor=? WHERE registration_id=?")
            .bind(covered.to_string())
            .bind(registration)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    pub async fn bootstrap_publish(
        &mut self,
        binding: &RegistrationBinding,
        session: &str,
        info: &ServerInfo,
    ) -> Result<(), StoreError> {
        validate_info(binding, info)?;
        let registration = self.registration_id(binding, true).await?;
        let mut tx = self.connection.begin().await.map_err(storage_error)?;
        let state = stage_row(&mut tx, registration, session).await?;
        let h1: Option<String> = state.try_get("h1").map_err(storage_error)?;
        let h1 = h1.ok_or_else(invalid)?;
        let h0 = decimal(state.try_get("h0").map_err(storage_error)?)?;
        let h1_number = decimal(&h1)?;
        if state
            .try_get::<String, _>("cursor")
            .map_err(storage_error)?
            != h1
            || info.head_seq.get() < h1_number
        {
            return Err(invalid());
        }
        let root_id: String = state.try_get("root_id").map_err(storage_error)?;
        if info.root_id.to_string() != root_id {
            return Err(invalid());
        }
        if info.journal_floor_seq.get() > h0 {
            return Err(StoreError::BootstrapHistoryExpired);
        }
        let rows = sqlx::query(
            "SELECT node_id,state FROM bootstrap_nodes WHERE registration_id=? ORDER BY node_id",
        )
        .bind(registration)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage_error)?;
        let nodes = decode_rows(&rows)?;
        if nodes
            .iter()
            .any(|node| node.last_change_seq.get() > h1_number)
        {
            return Err(StoreError::UnsupportedFormat);
        }
        validate_tree(&root_id, &nodes)?;
        let previous = sqlx::query("SELECT b.state AS new_state,n.state AS old_state FROM bootstrap_nodes b JOIN remote_nodes n ON n.node_id=b.node_id JOIN client_registrations r ON r.id=n.registration_id WHERE b.registration_id=? AND r.workspace_id=? AND r.workspace_epoch=?")
            .bind(registration).bind(&binding.workspace_id).bind(&binding.workspace_epoch)
            .fetch_all(&mut *tx).await.map_err(storage_error)?;
        for row in previous {
            let old: NodeState =
                serde_json::from_str(row.try_get("old_state").map_err(storage_error)?)
                    .map_err(|_| StoreError::UnsupportedFormat)?;
            let new: NodeState =
                serde_json::from_str(row.try_get("new_state").map_err(storage_error)?)
                    .map_err(|_| StoreError::UnsupportedFormat)?;
            if old.node_id != new.node_id || encode_node(&old).is_err() {
                return Err(StoreError::UnsupportedFormat);
            }
            validate_observations(&old, &new)?;
            if old.last_change_seq > new.last_change_seq
                || (old.last_change_seq == new.last_change_seq && !equal_state(&old, &new))
            {
                return Err(invalid());
            }
        }
        sqlx::query("DELETE FROM remote_nodes WHERE registration_id=?")
            .bind(registration)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        sqlx::query("INSERT INTO remote_nodes(registration_id,node_id,state) SELECT registration_id,node_id,state FROM bootstrap_nodes WHERE registration_id=?")
            .bind(registration).execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("INSERT INTO remote_index(registration_id,root_id,cursor) VALUES(?,?,?) ON CONFLICT(registration_id) DO UPDATE SET root_id=excluded.root_id,cursor=excluded.cursor")
            .bind(registration).bind(root_id).bind(h1).execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("DELETE FROM sync_bootstrap WHERE registration_id=?")
            .bind(registration)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    pub async fn sync_index(
        &mut self,
        binding: &RegistrationBinding,
    ) -> Result<Option<(ChangeSeq, Vec<NodeState>)>, StoreError> {
        let registration = self.registration_id(binding, true).await?;
        let index = sqlx::query("SELECT cursor,root_id FROM remote_index WHERE registration_id=?")
            .bind(registration)
            .fetch_optional(&mut self.connection)
            .await
            .map_err(storage_error)?;
        let Some(index) = index else {
            return Ok(None);
        };
        let cursor = decimal(index.try_get("cursor").map_err(storage_error)?)?;
        let root: String = index.try_get("root_id").map_err(storage_error)?;
        let rows = sqlx::query(
            "SELECT node_id,state FROM remote_nodes WHERE registration_id=? ORDER BY node_id",
        )
        .bind(registration)
        .fetch_all(&mut self.connection)
        .await
        .map_err(storage_error)?;
        let nodes = decode_rows(&rows)?;
        if nodes.iter().any(|node| node.last_change_seq.get() > cursor)
            || validate_tree(&root, &nodes).is_err()
        {
            return Err(StoreError::UnsupportedFormat);
        }
        Ok(Some((ChangeSeq::new(cursor), nodes)))
    }
}
