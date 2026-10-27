use crate::{
    ChangeSeq, Error, NodeId, NodeSnapshot, OperationId, Outcome, Phase, Receipt, Workspace,
};
use notryx_domain::MetaRevision;
use notryx_protocol::NodeKind;
use notryx_server_storage::{FileStamp, FolderStamp, ObservedEntry};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverySummary {
    pub operation_id: OperationId,
    pub resolved: bool,
    pub retained_objects: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDetail {
    pub summary: RecoverySummary,
    pub paths: Vec<String>,
    pub objects: Vec<String>,
    pub next_after_hash: Option<String>,
}

impl Workspace {
    pub async fn list_recovery(
        &mut self,
        after: Option<OperationId>,
        limit: u16,
    ) -> Result<Vec<RecoverySummary>, Error> {
        self.state.recovery_list(after, limit).await
    }

    pub async fn inspect_recovery(&mut self, id: OperationId) -> Result<RecoveryDetail, Error> {
        self.state.recovery_detail(id).await
    }

    pub async fn inspect_recovery_page(
        &mut self,
        id: OperationId,
        after_hash: Option<&str>,
    ) -> Result<RecoveryDetail, Error> {
        self.state.recovery_detail_page(id, after_hash).await
    }

    pub async fn resolve_recovery(&mut self, id: OperationId) -> Result<Receipt, Error> {
        let detail = self.state.recovery_detail(id).await?;
        if !detail.summary.resolved {
            self.ready = false;
            if !self.state.is_quarantined(id).await? {
                return Err(Error::InvalidFormat);
            }
            self.state.validate_current().await?;
            let all = self.state.all_nodes().await?;
            let mut paths = BTreeMap::new();
            for node in &all {
                if !node.deleted {
                    paths.insert(self.state.node_path(node.node_id).await?, node.node_id);
                }
            }
            let mut observed = BTreeMap::new();
            let mut listings = BTreeMap::new();
            let mut hashes = BTreeSet::new();
            for root in &detail.paths {
                let parent = root.rsplit_once('/').map_or("", |(parent, _)| parent);
                listings.insert(parent.to_owned(), self.recovery_listing(parent)?);
            }
            let mut queue = detail.paths.clone();
            while let Some(path) = queue.pop() {
                if observed.contains_key(&path) {
                    continue;
                }
                let entry = match self.files.observe_entry(&path)? {
                    ObservedEntry::Absent => Observation::Absent,
                    ObservedEntry::File(file) => {
                        hashes.insert(self.put_object(&file.bytes)?);
                        Observation::File(file.stamp)
                    }
                    ObservedEntry::Folder { stamp, .. } => {
                        let names = self.recovery_listing(&path)?;
                        for name in &names {
                            queue.push(format!("{path}/{name}"));
                        }
                        listings.insert(path.clone(), names);
                        Observation::Folder(stamp)
                    }
                };
                observed.insert(path, entry);
            }
            let mut affected = Vec::new();
            for (path, node_id) in &paths {
                if detail.paths.iter().any(|root| in_subtree(path, root)) {
                    let node = all
                        .iter()
                        .find(|n| n.node_id == *node_id)
                        .ok_or(Error::InvalidFormat)?;
                    if let Some(hash) = &node.content_hash {
                        let bytes = self.files.read_object(hash)?;
                        hashes.insert(self.put_object(&bytes)?);
                    }
                    affected.push((path.clone(), *node_id));
                }
            }
            self.state.retain_resolution_objects(id, &hashes).await?;
            let changes = plan_changes(
                all,
                paths,
                affected,
                &observed,
                self.state.head_seq().await?.get(),
            )?;
            for (path, entry) in &observed {
                match entry {
                    Observation::Absent => self.sync_absent(path)?,
                    Observation::File(stamp) => self.sync_observed(path, stamp)?,
                    Observation::Folder(stamp) => self.sync_folder(path, stamp)?,
                }
            }
            self.validate_recovery_objects().await?;
            self.point(Phase::BeforeResolveFinish)?;
            // Directory timestamps can stay unchanged when names change on coarse filesystems.
            for (path, names) in &listings {
                if self.recovery_listing(path)? != *names {
                    return Err(Error::ExternalChange);
                }
            }
            for (path, expected) in &observed {
                if !expected.matches(&self.files.observe_entry(path)?) {
                    return Err(Error::ExternalChange);
                }
            }
            self.state.finish_resolution(id, &changes).await?;
            self.point(Phase::AfterResolveFinish)?;
        }
        self.state.validate_current().await?;
        self.validate_documents().await?;
        self.cleanup_temporary_files()?;
        self.ready = self.state.pending().await?.is_empty();
        Ok(Receipt {
            operation_id: id,
            outcome: Outcome::RecoveryConflict,
        })
    }

    pub async fn release_recovery(&mut self, id: OperationId) -> Result<(), Error> {
        self.state.release_recovery(id).await
    }

    pub(crate) async fn validate_recovery_objects(&mut self) -> Result<(), Error> {
        for hash in self.state.recovery_hashes().await? {
            self.files.read_object(&hash)?;
        }
        Ok(())
    }

    fn recovery_listing(&self, path: &str) -> Result<Vec<String>, Error> {
        let names = self.files.list_children(path)?;
        let mut keys = HashSet::new();
        if names
            .iter()
            .any(|name| !keys.insert(notryx_domain::mutation::normalized_name_key(name)))
        {
            return Err(Error::ExternalChange);
        }
        Ok(names)
    }
}

enum Observation {
    Absent,
    File(FileStamp),
    Folder(FolderStamp),
}

impl Observation {
    fn kind(&self) -> Option<NodeKind> {
        match self {
            Self::Absent => None,
            Self::File(_) => Some(NodeKind::File),
            Self::Folder(_) => Some(NodeKind::Folder),
        }
    }

    fn matches(&self, current: &ObservedEntry) -> bool {
        match (self, current) {
            (Self::Absent, ObservedEntry::Absent) => true,
            (Self::File(expected), ObservedEntry::File(current)) => *expected == current.stamp,
            (Self::Folder(expected), ObservedEntry::Folder { stamp, .. }) => expected == stamp,
            _ => false,
        }
    }
}

fn in_subtree(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn new_meta() -> MetaRevision {
    MetaRevision::new(Uuid::new_v4().to_string()).expect("nonempty UUID")
}

fn plan_changes(
    all: Vec<NodeSnapshot>,
    mut paths: BTreeMap<String, NodeId>,
    mut affected: Vec<(String, NodeId)>,
    observed: &BTreeMap<String, Observation>,
    mut head: u64,
) -> Result<Vec<Vec<NodeSnapshot>>, Error> {
    let mut nodes: HashMap<_, _> = all.into_iter().map(|node| (node.node_id, node)).collect();
    let mut changes = Vec::new();
    affected.sort_by(|a, b| {
        b.0.matches('/')
            .count()
            .cmp(&a.0.matches('/').count())
            .then_with(|| a.0.cmp(&b.0))
    });
    for (path, id) in affected {
        let before = nodes.get(&id).ok_or(Error::InvalidFormat)?;
        if observed.get(&path).and_then(Observation::kind) == Some(before.kind) {
            continue;
        }
        if nodes
            .values()
            .any(|n| !n.deleted && n.parent_id == Some(id))
        {
            return Err(Error::InvalidFormat);
        }
        let mut tombstone = before.clone();
        tombstone.deleted = true;
        tombstone.meta_rev = new_meta();
        if tombstone.kind == NodeKind::File {
            tombstone.content_rev = Some(crate::revision());
        }
        emit(&mut nodes, &mut changes, &mut head, tombstone, true)?;
        paths.remove(&path);
    }
    let mut live: Vec<_> = observed
        .iter()
        .filter(|(_, o)| o.kind().is_some())
        .collect();
    live.sort_by(|a, b| {
        a.0.matches('/')
            .count()
            .cmp(&b.0.matches('/').count())
            .then_with(|| a.0.cmp(b.0))
    });
    for (path, observation) in live {
        if let Some(id) = paths.get(path) {
            let before = nodes.get(id).ok_or(Error::InvalidFormat)?;
            if let Observation::File(stamp) = observation
                && (before.content_hash.as_deref() != Some(stamp.hash.as_str())
                    || before.size != Some(stamp.size))
            {
                let mut updated = before.clone();
                updated.content_hash = Some(stamp.hash.clone());
                updated.size = Some(stamp.size);
                updated.content_rev = Some(crate::revision());
                emit(&mut nodes, &mut changes, &mut head, updated, false)?;
            }
            continue;
        }
        let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = *paths.get(parent_path).ok_or(Error::ExternalChange)?;
        if nodes
            .get(&parent)
            .is_none_or(|n| n.deleted || n.kind != NodeKind::Folder)
        {
            return Err(Error::ExternalChange);
        }
        let (content_rev, content_hash, size) = match observation {
            Observation::File(stamp) => (
                Some(crate::revision()),
                Some(stamp.hash.clone()),
                Some(stamp.size),
            ),
            _ => (None, None, None),
        };
        let node = NodeSnapshot {
            node_id: crate::node_id(),
            parent_id: Some(parent),
            name: name.into(),
            kind: observation.kind().ok_or(Error::InvalidFormat)?,
            content_rev,
            meta_rev: new_meta(),
            content_hash,
            size,
            children_generation: 0,
            last_change_seq: ChangeSeq::new(0),
            deleted: false,
        };
        paths.insert(path.clone(), node.node_id);
        emit(&mut nodes, &mut changes, &mut head, node, true)?;
    }
    Ok(changes)
}

fn emit(
    nodes: &mut HashMap<NodeId, NodeSnapshot>,
    changes: &mut Vec<Vec<NodeSnapshot>>,
    head: &mut u64,
    mut node: NodeSnapshot,
    namespace: bool,
) -> Result<(), Error> {
    *head = head.checked_add(1).ok_or(Error::InvalidFormat)?;
    node.last_change_seq = ChangeSeq::new(*head);
    let mut posts = vec![node.clone()];
    if namespace {
        let parent_id = node.parent_id.ok_or(Error::InvalidFormat)?;
        let mut parent = nodes.get(&parent_id).ok_or(Error::InvalidFormat)?.clone();
        if parent.deleted || parent.kind != NodeKind::Folder {
            return Err(Error::InvalidFormat);
        }
        parent.children_generation = parent
            .children_generation
            .checked_add(1)
            .ok_or(Error::InvalidFormat)?;
        parent.last_change_seq = node.last_change_seq;
        nodes.insert(parent_id, parent.clone());
        posts.push(parent);
    }
    nodes.insert(node.node_id, node);
    changes.push(posts);
    Ok(())
}
