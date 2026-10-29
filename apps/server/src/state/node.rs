use super::{State, canonical_u64, db_error, decode, parse};
use crate::{Error, NodeSnapshot};
use notryx_domain::{ChangeSeq, NodeId};
use notryx_protocol::NodeKind;
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteRow};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(super) fn snapshot(row: &SqliteRow) -> Result<NodeSnapshot, Error> {
    let parent: Option<String> = row.try_get("parent_id").map_err(db_error)?;
    let content_rev: Option<String> = row.try_get("content_rev").map_err(db_error)?;
    let size: Option<String> = row.try_get("size").map_err(db_error)?;
    Ok(NodeSnapshot {
        node_id: parse(row.try_get("node_id").map_err(db_error)?)?,
        parent_id: parent.as_deref().map(parse).transpose()?,
        name: row.try_get("name").map_err(db_error)?,
        kind: match row.try_get::<&str, _>("kind").map_err(db_error)? {
            "file" => NodeKind::File,
            "folder" => NodeKind::Folder,
            _ => return Err(Error::InvalidFormat),
        },
        content_rev: content_rev
            .map(|value| value.try_into().map_err(|_| Error::InvalidFormat))
            .transpose()?,
        meta_rev: row
            .try_get::<String, _>("meta_rev")
            .map_err(db_error)?
            .try_into()
            .map_err(|_| Error::InvalidFormat)?,
        content_hash: row.try_get("content_hash").map_err(db_error)?,
        size: size.as_deref().map(canonical_u64).transpose()?,
        children_generation: canonical_u64(row.try_get("children_generation").map_err(db_error)?)?,
        last_change_seq: ChangeSeq::new(canonical_u64(
            row.try_get("last_change_seq").map_err(db_error)?,
        )?),
        deleted: match row.try_get::<i64, _>("deleted") {
            Ok(0) => false,
            Ok(1) => true,
            Err(sqlx::Error::ColumnNotFound(_)) => false,
            _ => return Err(Error::InvalidFormat),
        },
    })
}

pub(crate) fn node_post_state(node: &NodeSnapshot) -> serde_json::Value {
    serde_json::json!({
        "node_id": node.node_id, "parent_id": node.parent_id, "name": node.name,
        "kind": node.kind, "content_rev": node.content_rev, "meta_rev": node.meta_rev,
        "content_hash": node.content_hash, "size": node.size.map(|size| size.to_string()),
        "children_generation": node.children_generation.to_string(),
        "last_change_seq": node.last_change_seq, "deleted": node.deleted
    })
}

pub(crate) fn journal_snapshot(
    text: &str,
    id: NodeId,
    seq: ChangeSeq,
) -> Result<NodeSnapshot, Error> {
    let original: serde_json::Value = decode(text)?;
    let mut value = original.clone();
    let object = value.as_object_mut().ok_or(Error::InvalidFormat)?;
    for field in ["size", "children_generation"] {
        let item = object.get_mut(field).ok_or(Error::InvalidFormat)?;
        if !item.is_null() {
            let number = canonical_u64(item.as_str().ok_or(Error::InvalidFormat)?)?;
            *item = serde_json::Value::from(number);
        }
    }
    let snapshot: NodeSnapshot = serde_json::from_value(value).map_err(|_| Error::InvalidFormat)?;
    if snapshot.node_id != id
        || snapshot.last_change_seq != seq
        || node_post_state(&snapshot) != original
    {
        return Err(Error::InvalidFormat);
    }
    if snapshot.content_hash.as_ref().is_some_and(|hash| {
        hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(Error::InvalidFormat);
    }
    let representation = snapshot.representation()?;
    let encoded = serde_json::to_value(&representation).map_err(super::serialization_error)?;
    serde_json::from_value::<notryx_protocol::NodeState>(encoded)
        .map_err(|_| Error::InvalidFormat)?;
    Ok(snapshot)
}

impl State {
    pub(crate) async fn journal_position(&mut self) -> Result<(ChangeSeq, ChangeSeq), Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let row = sqlx::query(
            "SELECT epoch,head_seq,journal_floor_seq FROM workspaces WHERE singleton=1",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        let epoch: String = row.try_get("epoch").map_err(db_error)?;
        if epoch != self.epoch.to_string() {
            return Err(Error::InvalidFormat);
        }
        let head = canonical_u64(row.try_get("head_seq").map_err(db_error)?)?;
        let floor = canonical_u64(row.try_get("journal_floor_seq").map_err(db_error)?)?;
        if floor > head {
            return Err(Error::InvalidFormat);
        }
        tx.commit().await.map_err(db_error)?;
        Ok((ChangeSeq::new(head), ChangeSeq::new(floor)))
    }

    pub async fn changes(
        &mut self,
        after: ChangeSeq,
        through: Option<ChangeSeq>,
    ) -> Result<notryx_protocol::ChangesPage, Error> {
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let row = sqlx::query(
            "SELECT epoch,head_seq,journal_floor_seq FROM workspaces WHERE singleton=1",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        let epoch: String = row.try_get("epoch").map_err(db_error)?;
        if epoch != self.epoch.to_string() {
            return Err(Error::InvalidFormat);
        }
        let head = canonical_u64(row.try_get("head_seq").map_err(db_error)?)?;
        let floor = canonical_u64(row.try_get("journal_floor_seq").map_err(db_error)?)?;
        if floor > head {
            return Err(Error::InvalidFormat);
        }
        if after.get() < floor {
            return Err(Error::ResyncRequired);
        }
        let through = through.unwrap_or(ChangeSeq::new(head));
        if after > through || through.get() > head {
            return Err(Error::InvalidRequest);
        }
        let mut page = notryx_protocol::ChangesPage {
            workspace_epoch: self.epoch,
            from_exclusive: after,
            through_seq: through,
            covered_through: after,
            has_more: after < through,
            nodes: Vec::new(),
        };
        for _ in 0..100 {
            if page.covered_through == through {
                break;
            }
            let seq = ChangeSeq::new(
                page.covered_through
                    .get()
                    .checked_add(1)
                    .ok_or(Error::InvalidFormat)?,
            );
            let present: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM change_sets WHERE seq=?)")
                    .bind(seq.get().to_string())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db_error)?;
            if !present {
                return Err(Error::InvalidFormat);
            }
            let original_len = page.nodes.len();
            let previous = page.covered_through;
            page.covered_through = seq;
            page.has_more = seq < through;
            let mut response_bytes = serde_json::to_vec(&page)
                .map_err(super::serialization_error)?
                .len();
            let mut previous_id: Option<String> = None;
            let mut oversized = false;
            loop {
                // Keep oversized records in SQLite instead of materializing them in the process.
                let row = sqlx::query("SELECT node_id, CASE WHEN length(CAST(post_state AS BLOB)) <= 1048576 THEN post_state ELSE NULL END AS post_state FROM node_changes WHERE seq=? AND (? IS NULL OR node_id COLLATE BINARY > ?) ORDER BY node_id COLLATE BINARY LIMIT 1")
                    .bind(seq.get().to_string())
                    .bind(&previous_id)
                    .bind(&previous_id)
                    .fetch_optional(&mut *tx).await.map_err(db_error)?;
                let Some(row) = row else {
                    break;
                };
                let id_text: String = row.try_get("node_id").map_err(db_error)?;
                let id: NodeId = parse(&id_text)?;
                if id_text != id.to_string() {
                    return Err(Error::InvalidFormat);
                }
                let post: Option<String> = row.try_get("post_state").map_err(db_error)?;
                let Some(post) = post else {
                    oversized = true;
                    break;
                };
                let node = journal_snapshot(&post, id, seq)?.representation()?;
                let node_bytes = serde_json::to_vec(&node)
                    .map_err(super::serialization_error)?
                    .len();
                response_bytes = response_bytes
                    .checked_add(node_bytes + usize::from(!page.nodes.is_empty()))
                    .ok_or(Error::InvalidFormat)?;
                if response_bytes > 1024 * 1024 {
                    oversized = true;
                    break;
                }
                page.nodes.push(node);
                previous_id = Some(id_text);
            }
            if oversized {
                if original_len == 0 {
                    return Err(Error::ChangesPageLimit);
                }
                page.nodes.truncate(original_len);
                page.covered_through = previous;
                page.has_more = true;
                break;
            }
            if page.nodes.len() == original_len {
                return Err(Error::InvalidFormat);
            }
        }
        tx.commit().await.map_err(db_error)?;
        Ok(page)
    }

    pub async fn all_nodes(&mut self) -> Result<Vec<NodeSnapshot>, Error> {
        sqlx::query("SELECT * FROM nodes")
            .fetch_all(&mut self.connection)
            .await
            .map_err(db_error)?
            .iter()
            .map(snapshot)
            .collect()
    }

    pub async fn child_exists_except(
        &mut self,
        parent: NodeId,
        name: &str,
        except: NodeId,
    ) -> Result<bool, Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id=? AND normalized_name_key=? AND deleted=0 AND node_id<>?)")
            .bind(parent.to_string()).bind(notryx_domain::mutation::normalized_name_key(name))
            .bind(except.to_string()).fetch_one(&mut self.connection).await.map_err(db_error)
    }

    pub async fn inventory_nodes(
        &mut self,
        after_id: Option<NodeId>,
        limit: u16,
    ) -> Result<Vec<NodeSnapshot>, Error> {
        self.list_nodes(after_id, limit, false).await
    }

    pub async fn trash_nodes(
        &mut self,
        after_id: Option<NodeId>,
        limit: u16,
    ) -> Result<Vec<NodeSnapshot>, Error> {
        self.list_nodes(after_id, limit, true).await
    }

    async fn list_nodes(
        &mut self,
        after_id: Option<NodeId>,
        limit: u16,
        deleted: bool,
    ) -> Result<Vec<NodeSnapshot>, Error> {
        if !(1..=100).contains(&limit) {
            return Err(Error::InvalidRequest);
        }
        let mut tx = self.connection.begin().await.map_err(db_error)?;
        let epoch: String = sqlx::query_scalar("SELECT epoch FROM workspaces WHERE singleton=1")
            .fetch_one(&mut *tx)
            .await
            .map_err(db_error)?;
        if epoch != self.epoch.to_string() {
            return Err(Error::InvalidFormat);
        }
        // Canonical UUID text has the same order as UUID bytes under BINARY collation.
        let rows = sqlx::query("SELECT * FROM nodes WHERE deleted=? AND node_id COLLATE BINARY > ? ORDER BY node_id COLLATE BINARY LIMIT ?")
            .bind(deleted)
            .bind(after_id.map(|id| id.to_string()).unwrap_or_default())
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *tx).await.map_err(db_error)?;
        let nodes = rows.iter().map(snapshot).collect::<Result<Vec<_>, _>>()?;
        tx.commit().await.map_err(db_error)?;
        Ok(nodes)
    }

    pub async fn node(&mut self, node_id: NodeId) -> Result<Option<NodeSnapshot>, Error> {
        sqlx::query("SELECT * FROM nodes WHERE node_id=?")
            .bind(node_id.to_string())
            .fetch_optional(&mut self.connection)
            .await
            .map_err(db_error)?
            .as_ref()
            .map(snapshot)
            .transpose()
    }

    pub async fn known_file_child(
        &mut self,
        parent_id: NodeId,
        name: &str,
    ) -> Result<Option<NodeId>, Error> {
        notryx_domain::mutation::validate_name(name).map_err(|_| Error::InvalidRequest)?;
        let node_id: Option<String> = sqlx::query_scalar(
            "SELECT child.node_id FROM nodes AS child JOIN nodes AS parent ON parent.node_id=child.parent_id WHERE child.parent_id=? AND child.name COLLATE BINARY=? COLLATE BINARY AND child.kind='file' AND child.deleted=0 AND parent.kind='folder' AND parent.deleted=0",
        )
        .bind(parent_id.to_string())
        .bind(name)
        .fetch_optional(&mut self.connection)
        .await
        .map_err(db_error)?;
        node_id.as_deref().map(parse).transpose()
    }

    pub async fn known_folder_child(
        &mut self,
        parent_id: NodeId,
        name: &str,
    ) -> Result<bool, Error> {
        notryx_domain::mutation::validate_name(name).map_err(|_| Error::InvalidRequest)?;
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id=? AND name COLLATE BINARY=? COLLATE BINARY AND kind='folder' AND deleted=0)",
        )
        .bind(parent_id.to_string())
        .bind(name)
        .fetch_one(&mut self.connection)
        .await
        .map_err(db_error)
    }

    pub async fn node_path(&mut self, node_id: NodeId) -> Result<String, Error> {
        let mut current = node_id;
        let mut seen = HashSet::new();
        let mut segments = Vec::new();
        loop {
            if !seen.insert(current) {
                return Err(Error::InvalidFormat);
            }
            let node = self.node(current).await?.ok_or(Error::InvalidFormat)?;
            if node.deleted || (current != node_id && node.kind != NodeKind::Folder) {
                return Err(Error::InvalidFormat);
            }
            if let Some(parent) = node.parent_id {
                notryx_domain::mutation::validate_name(&node.name)
                    .map_err(|_| Error::InvalidFormat)?;
                segments.push(node.name);
                current = parent;
            } else {
                if current != self.root_id || node.kind != NodeKind::Folder || !node.name.is_empty()
                {
                    return Err(Error::InvalidFormat);
                }
                break;
            }
        }
        segments.reverse();
        let path = segments.join("/");
        if path.len() > 1024 {
            return Err(Error::InvalidFormat);
        }
        Ok(path)
    }

    pub async fn child_exists(&mut self, parent: NodeId, name: &str) -> Result<bool, Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id=? AND normalized_name_key=? AND deleted=0)",
        )
        .bind(parent.to_string())
        .bind(notryx_domain::mutation::normalized_name_key(name))
        .fetch_one(&mut self.connection)
        .await
        .map_err(db_error)
    }
}

pub(crate) async fn validate_tree(
    connection: &mut SqliteConnection,
    root_id: NodeId,
    head: u64,
    version: i64,
) -> Result<(), Error> {
    let floor = if version >= 10 {
        let value: String =
            sqlx::query_scalar("SELECT journal_floor_seq FROM workspaces WHERE singleton=1")
                .fetch_one(&mut *connection)
                .await
                .map_err(db_error)?;
        canonical_u64(&value)?
    } else {
        0
    };
    let rows = sqlx::query("SELECT * FROM nodes")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    let mut nodes = std::collections::HashMap::new();
    for row in &rows {
        let node = snapshot(row)?;
        if row.try_get::<&str, _>("node_id").map_err(db_error)? != node.node_id.to_string()
            || node.last_change_seq.get() > head
        {
            return Err(Error::InvalidFormat);
        }
        if node.node_id == root_id {
            if node.deleted
                || node.parent_id.is_some()
                || !node.name.is_empty()
                || node.kind != NodeKind::Folder
            {
                return Err(Error::InvalidFormat);
            }
        } else if node.parent_id.is_none()
            || node.last_change_seq.get() == 0
            || notryx_domain::mutation::validate_name(&node.name).is_err()
            || (version < 4 && (node.parent_id != Some(root_id) || node.kind != NodeKind::File))
        {
            return Err(Error::InvalidFormat);
        }
        if row
            .try_get::<&str, _>("normalized_name_key")
            .map_err(db_error)?
            != notryx_domain::mutation::normalized_name_key(&node.name)
        {
            return Err(Error::InvalidFormat);
        }
        match node.kind {
            NodeKind::File => {
                if node.content_rev.is_none()
                    || node.content_hash.is_none()
                    || node.size.is_none()
                    || node.children_generation != 0
                {
                    return Err(Error::InvalidFormat);
                }
            }
            NodeKind::Folder => {
                if node.content_rev.is_some() || node.content_hash.is_some() || node.size.is_some()
                {
                    return Err(Error::InvalidFormat);
                }
            }
        }
        if node.last_change_seq.get() != 0 {
            let post: Option<String> = if node.last_change_seq.get() <= floor {
                sqlx::query_scalar(
                    "SELECT post_state FROM journal_baseline WHERE seq=? AND node_id=?",
                )
                .bind(node.last_change_seq.get().to_string())
                .bind(node.node_id.to_string())
                .fetch_optional(&mut *connection)
                .await
                .map_err(db_error)?
            } else {
                sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq=? AND node_id=?")
                    .bind(node.last_change_seq.get().to_string())
                    .bind(node.node_id.to_string())
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(db_error)?
            };
            if decode::<serde_json::Value>(&post.ok_or(Error::InvalidFormat)?)?
                != node_post_state(&node)
            {
                return Err(Error::InvalidFormat);
            }
        }
        nodes.insert(node.node_id, node);
    }
    if !nodes.contains_key(&root_id) {
        return Err(Error::InvalidFormat);
    }
    let mut child_counts = std::collections::HashMap::<NodeId, u64>::new();
    for node in nodes.values() {
        if let Some(parent) = node.parent_id {
            *child_counts.entry(parent).or_default() += 1;
        }
    }
    for node in nodes.values() {
        if node.deleted {
            let parent = nodes
                .get(&node.parent_id.ok_or(Error::InvalidFormat)?)
                .ok_or(Error::InvalidFormat)?;
            if parent.kind != NodeKind::Folder {
                return Err(Error::InvalidFormat);
            }
            continue;
        }
        let mut current = node;
        let mut seen = HashSet::new();
        let mut length = 0;
        while let Some(parent_id) = current.parent_id {
            if !seen.insert(current.node_id) {
                return Err(Error::InvalidFormat);
            }
            length += current.name.len() + usize::from(length != 0);
            if length > 1024 {
                return Err(Error::InvalidFormat);
            }
            current = nodes.get(&parent_id).ok_or(Error::InvalidFormat)?;
            if current.deleted || current.kind != NodeKind::Folder {
                return Err(Error::InvalidFormat);
            }
        }
        if current.node_id != root_id {
            return Err(Error::InvalidFormat);
        }
        if version < 6 && node.kind == NodeKind::Folder {
            let children = child_counts.get(&node.node_id).copied().unwrap_or_default();
            if node.children_generation != children {
                return Err(Error::InvalidFormat);
            }
        }
    }
    if version >= 6 {
        validate_namespace_history(connection, root_id, &nodes, version, floor, head).await?;
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
struct NamespaceState {
    parent_id: Option<NodeId>,
    name: String,
    kind: NodeKind,
    generation: u64,
    deleted: bool,
}

impl From<&NodeSnapshot> for NamespaceState {
    fn from(node: &NodeSnapshot) -> Self {
        Self {
            parent_id: node.parent_id,
            name: node.name.clone(),
            kind: node.kind,
            generation: node.children_generation,
            deleted: node.deleted,
        }
    }
}

async fn baseline_snapshots(
    connection: &mut SqliteConnection,
    root_id: NodeId,
    floor: u64,
) -> Result<HashMap<NodeId, NodeSnapshot>, Error> {
    let rows = sqlx::query("SELECT node_id,seq,post_state FROM journal_baseline")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    let mut nodes = HashMap::new();
    let mut live_names = HashSet::new();
    for row in rows {
        let raw_id: String = row.try_get("node_id").map_err(db_error)?;
        let id: NodeId = parse(&raw_id)?;
        let seq = canonical_u64(row.try_get("seq").map_err(db_error)?)?;
        if raw_id != id.to_string() || seq > floor || (seq == 0 && id != root_id) {
            return Err(Error::InvalidFormat);
        }
        let post: String = row.try_get("post_state").map_err(db_error)?;
        let node = journal_snapshot(&post, id, ChangeSeq::new(seq))?;
        if id == root_id {
            if node.deleted
                || node.parent_id.is_some()
                || !node.name.is_empty()
                || node.kind != NodeKind::Folder
            {
                return Err(Error::InvalidFormat);
            }
        } else if node.parent_id.is_none()
            || notryx_domain::mutation::validate_name(&node.name).is_err()
        {
            return Err(Error::InvalidFormat);
        }
        match node.kind {
            NodeKind::File
                if node.content_rev.is_none()
                    || node.content_hash.is_none()
                    || node.size.is_none()
                    || node.children_generation != 0 =>
            {
                return Err(Error::InvalidFormat);
            }
            NodeKind::Folder
                if node.content_rev.is_some()
                    || node.content_hash.is_some()
                    || node.size.is_some() =>
            {
                return Err(Error::InvalidFormat);
            }
            _ => {}
        }
        if !node.deleted
            && id != root_id
            && !live_names.insert((
                node.parent_id,
                notryx_domain::mutation::normalized_name_key(&node.name),
            ))
        {
            return Err(Error::InvalidFormat);
        }
        if nodes.insert(id, node).is_some() {
            return Err(Error::InvalidFormat);
        }
    }
    if !nodes.contains_key(&root_id) {
        return Err(Error::InvalidFormat);
    }
    for node in nodes.values() {
        if node.deleted {
            let parent = nodes
                .get(&node.parent_id.ok_or(Error::InvalidFormat)?)
                .ok_or(Error::InvalidFormat)?;
            if parent.kind != NodeKind::Folder {
                return Err(Error::InvalidFormat);
            }
            continue;
        }
        let mut current = node;
        let mut seen = HashSet::new();
        let mut length = 0_usize;
        while let Some(parent) = current.parent_id {
            if !seen.insert(current.node_id) {
                return Err(Error::InvalidFormat);
            }
            length += current.name.len() + usize::from(length != 0);
            if length > 1024 {
                return Err(Error::InvalidFormat);
            }
            current = nodes.get(&parent).ok_or(Error::InvalidFormat)?;
            if current.deleted || current.kind != NodeKind::Folder {
                return Err(Error::InvalidFormat);
            }
        }
        if current.node_id != root_id {
            return Err(Error::InvalidFormat);
        }
    }
    Ok(nodes)
}

async fn validate_namespace_history(
    connection: &mut SqliteConnection,
    root_id: NodeId,
    nodes: &HashMap<NodeId, NodeSnapshot>,
    version: i64,
    floor: u64,
    head: u64,
) -> Result<(), Error> {
    let rows = sqlx::query("SELECT seq,node_id,post_state FROM node_changes")
        .fetch_all(&mut *connection)
        .await
        .map_err(db_error)?;
    let mut changes =
        BTreeMap::<u64, HashMap<NodeId, (NamespaceState, Option<NodeSnapshot>)>>::new();
    for row in rows {
        let seq = canonical_u64(row.try_get("seq").map_err(db_error)?)?;
        if seq <= floor || seq > head {
            return Err(Error::InvalidFormat);
        }
        let raw_id: String = row.try_get("node_id").map_err(db_error)?;
        let id: NodeId = parse(&raw_id)?;
        let raw_post: String = row.try_get("post_state").map_err(db_error)?;
        let checked = if version >= 10 {
            if raw_id != id.to_string() {
                return Err(Error::InvalidFormat);
            }
            Some(journal_snapshot(&raw_post, id, ChangeSeq::new(seq))?)
        } else {
            None
        };
        let post: serde_json::Value = decode(&raw_post)?;
        if post["node_id"].as_str() != Some(id.to_string().as_str())
            || post["last_change_seq"].as_str() != Some(seq.to_string().as_str())
            || !post["deleted"].is_boolean()
            || (version < 7 && post["deleted"] != false)
        {
            return Err(Error::InvalidFormat);
        }
        let parent_id = if post["parent_id"].is_null() {
            None
        } else {
            Some(parse(
                post["parent_id"].as_str().ok_or(Error::InvalidFormat)?,
            )?)
        };
        let state = NamespaceState {
            deleted: post["deleted"].as_bool().ok_or(Error::InvalidFormat)?,
            parent_id,
            name: post["name"].as_str().ok_or(Error::InvalidFormat)?.into(),
            kind: serde_json::from_value(post["kind"].clone()).map_err(|_| Error::InvalidFormat)?,
            generation: canonical_u64(
                post["children_generation"]
                    .as_str()
                    .ok_or(Error::InvalidFormat)?,
            )?,
        };
        if changes
            .entry(seq)
            .or_default()
            .insert(id, (state, checked))
            .is_some()
        {
            return Err(Error::InvalidFormat);
        }
    }
    if changes.len() as u64 != head.checked_sub(floor).ok_or(Error::InvalidFormat)? {
        return Err(Error::InvalidFormat);
    }
    let mut previous_full = if version >= 10 {
        Some(baseline_snapshots(connection, root_id, floor).await?)
    } else {
        None
    };
    let mut seen_content_revisions = HashMap::<NodeId, HashSet<String>>::new();
    let mut seen_meta_revisions = HashMap::<NodeId, HashSet<String>>::new();
    if let Some(initial) = previous_full.as_ref() {
        for (id, node) in initial {
            if let Some(revision) = &node.content_rev {
                seen_content_revisions
                    .entry(*id)
                    .or_default()
                    .insert(revision.as_str().into());
            }
            seen_meta_revisions
                .entry(*id)
                .or_default()
                .insert(node.meta_rev.as_str().into());
        }
    }
    let mut previous = if floor == 0 {
        HashMap::from([(
            root_id,
            NamespaceState {
                parent_id: None,
                name: String::new(),
                kind: NodeKind::Folder,
                generation: 0,
                deleted: false,
            },
        )])
    } else {
        previous_full
            .as_ref()
            .ok_or(Error::InvalidFormat)?
            .iter()
            .map(|(id, node)| (*id, NamespaceState::from(node)))
            .collect()
    };
    for posts in changes.values() {
        let mut affected = HashSet::new();
        for (id, (post, _)) in posts {
            match previous.get(id) {
                Some(before) => {
                    if before.kind != post.kind {
                        return Err(Error::InvalidFormat);
                    }
                    if before.parent_id != post.parent_id
                        || before.name != post.name
                        || before.deleted != post.deleted
                    {
                        if *id == root_id {
                            return Err(Error::InvalidFormat);
                        }
                        if !before.deleted {
                            affected.insert(before.parent_id.ok_or(Error::InvalidFormat)?);
                        }
                        if !post.deleted {
                            affected.insert(post.parent_id.ok_or(Error::InvalidFormat)?);
                        }
                    }
                }
                None => {
                    if post.deleted || post.generation != 0 {
                        return Err(Error::InvalidFormat);
                    }
                    affected.insert(post.parent_id.ok_or(Error::InvalidFormat)?);
                }
            }
        }
        for parent in &affected {
            if posts
                .get(parent)
                .is_none_or(|(node, _)| node.deleted || node.kind != NodeKind::Folder)
            {
                return Err(Error::InvalidFormat);
            }
        }
        for (id, (post, full)) in posts {
            let expected = previous
                .get(id)
                .map_or(0, |old| old.generation)
                .checked_add(u64::from(affected.contains(id)))
                .ok_or(Error::InvalidFormat)?;
            if post.generation != expected {
                return Err(Error::InvalidFormat);
            }
            if let Some(before_and_after) = previous_full.as_mut() {
                let after = full.as_ref().ok_or(Error::InvalidFormat)?;
                if let Some(before) = before_and_after.get(id) {
                    if before.kind == NodeKind::File
                        && before.deleted != after.deleted
                        && before.content_rev == after.content_rev
                    {
                        return Err(Error::InvalidFormat);
                    }
                    if before.content_rev == after.content_rev
                        && (before.content_hash != after.content_hash || before.size != after.size)
                    {
                        return Err(Error::InvalidFormat);
                    }
                    if before.meta_rev == after.meta_rev
                        && (before.parent_id != after.parent_id
                            || before.name != after.name
                            || before.deleted != after.deleted)
                    {
                        return Err(Error::InvalidFormat);
                    }
                }
                let previous_content = before_and_after
                    .get(id)
                    .and_then(|node| node.content_rev.as_ref());
                if previous_content != after.content_rev.as_ref()
                    && let Some(revision) = &after.content_rev
                    && !seen_content_revisions
                        .entry(*id)
                        .or_default()
                        .insert(revision.as_str().into())
                {
                    return Err(Error::InvalidFormat);
                }
                let previous_meta = before_and_after.get(id).map(|node| &node.meta_rev);
                if previous_meta != Some(&after.meta_rev)
                    && !seen_meta_revisions
                        .entry(*id)
                        .or_default()
                        .insert(after.meta_rev.as_str().into())
                {
                    return Err(Error::InvalidFormat);
                }
                before_and_after.insert(*id, after.clone());
            }
            previous.insert(*id, post.clone());
        }
    }
    if previous_full.as_ref().is_some_and(|full| full != nodes)
        || previous.len() != nodes.len()
        || nodes
            .iter()
            .any(|(id, node)| previous.get(id) != Some(&NamespaceState::from(node)))
    {
        return Err(Error::InvalidFormat);
    }
    Ok(())
}
