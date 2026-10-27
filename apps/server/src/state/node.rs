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

impl State {
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

pub(super) async fn validate_tree(
    connection: &mut SqliteConnection,
    root_id: NodeId,
    head: u64,
    version: i64,
) -> Result<(), Error> {
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
            let post: Option<String> =
                sqlx::query_scalar("SELECT post_state FROM node_changes WHERE seq=? AND node_id=?")
                    .bind(node.last_change_seq.get().to_string())
                    .bind(node.node_id.to_string())
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(db_error)?;
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
        validate_namespace_history(connection, root_id, &nodes, version).await?;
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

async fn validate_namespace_history(
    connection: &mut SqliteConnection,
    root_id: NodeId,
    nodes: &HashMap<NodeId, NodeSnapshot>,
    version: i64,
) -> Result<(), Error> {
    let rows = sqlx::query("SELECT seq,node_id,post_state FROM node_changes")
        .fetch_all(connection)
        .await
        .map_err(db_error)?;
    let mut changes = BTreeMap::<u64, HashMap<NodeId, NamespaceState>>::new();
    for row in rows {
        let seq = canonical_u64(row.try_get("seq").map_err(db_error)?)?;
        let id: NodeId = parse(row.try_get("node_id").map_err(db_error)?)?;
        let post: serde_json::Value = decode(row.try_get("post_state").map_err(db_error)?)?;
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
        if changes.entry(seq).or_default().insert(id, state).is_some() {
            return Err(Error::InvalidFormat);
        }
    }
    let mut previous = HashMap::from([(
        root_id,
        NamespaceState {
            parent_id: None,
            name: String::new(),
            kind: NodeKind::Folder,
            generation: 0,
            deleted: false,
        },
    )]);
    for posts in changes.values() {
        let mut affected = HashSet::new();
        for (id, post) in posts {
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
                .is_none_or(|node| node.deleted || node.kind != NodeKind::Folder)
            {
                return Err(Error::InvalidFormat);
            }
        }
        for (id, post) in posts {
            let expected = previous
                .get(id)
                .map_or(0, |old| old.generation)
                .checked_add(u64::from(affected.contains(id)))
                .ok_or(Error::InvalidFormat)?;
            if post.generation != expected {
                return Err(Error::InvalidFormat);
            }
            previous.insert(*id, post.clone());
        }
    }
    if previous.len() != nodes.len()
        || nodes
            .iter()
            .any(|(id, node)| previous.get(id) != Some(&NamespaceState::from(node)))
    {
        return Err(Error::InvalidFormat);
    }
    Ok(())
}
