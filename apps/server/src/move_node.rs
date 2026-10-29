use crate::state::move_node::MoveIntent;
use crate::{Error, MoveNode, NodeSnapshot, Outcome, Phase, Receipt, Workspace};
use notryx_domain::{MetaRevision, NodeId};
use notryx_protocol::NodeKind;
use notryx_server_storage::{EntryStamp, ObservedEntry, StorageError};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

impl Workspace {
    pub async fn move_node(&mut self, request: MoveNode) -> Result<Receipt, Error> {
        self.move_node_authorized(request, None).await
    }

    pub(crate) async fn move_node_authorized(
        &mut self,
        request: MoveNode,
        token: Option<&str>,
    ) -> Result<Receipt, Error> {
        if request.epoch != self.epoch() {
            return Err(Error::WorkspaceMismatch);
        }
        if request.operation_seq == 0
            || request.expected_meta_rev.as_str().len() > 256
            || notryx_domain::mutation::validate_name(&request.name).is_err()
        {
            return Err(Error::InvalidRequest);
        }
        let fingerprint = crate::hash(
            &serde_json::to_vec(&(
                "move_node/v1",
                request.epoch,
                request.stream_id,
                request.operation_seq,
                request.operation_id,
                request.node_id,
                &request.expected_meta_rev,
                request.expected_parent_id,
                request.parent_id,
                &request.name,
            ))
            .map_err(crate::state::serialization_error)?,
        );
        if let Some(previous) = self
            .state
            .previous_identity(
                request.stream_id,
                request.operation_seq,
                request.operation_id,
                Some(&fingerprint),
            )
            .await?
        {
            return Ok(previous);
        }
        self.authorize_admission(token, request.stream_id).await?;
        self.state
            .validate_admission(request.stream_id, request.operation_seq)
            .await?;
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        let Some(before) = self.state.node(request.node_id).await? else {
            return self
                .move_terminal(&request, &fingerprint, Outcome::NotFound, token)
                .await;
        };
        if before.deleted {
            return self
                .move_terminal(&request, &fingerprint, Outcome::NodeDeleted, token)
                .await;
        }
        if before.parent_id.is_none() {
            return self
                .move_terminal(&request, &fingerprint, Outcome::RootImmutable, token)
                .await;
        }
        if before.meta_rev != request.expected_meta_rev {
            return self
                .move_terminal(
                    &request,
                    &fingerprint,
                    Outcome::MetadataPreconditionFailed {
                        current: before.meta_rev,
                    },
                    token,
                )
                .await;
        }
        if before.parent_id != Some(request.expected_parent_id) {
            return self
                .move_terminal(
                    &request,
                    &fingerprint,
                    Outcome::ExpectedParentMismatch,
                    token,
                )
                .await;
        }
        let Some(parent) = self.state.node(request.parent_id).await? else {
            return self
                .move_terminal(&request, &fingerprint, Outcome::NotFound, token)
                .await;
        };
        if parent.deleted {
            return self
                .move_terminal(&request, &fingerprint, Outcome::NodeDeleted, token)
                .await;
        }
        if parent.kind != NodeKind::Folder {
            return self
                .move_terminal(&request, &fingerprint, Outcome::ParentNotFolder, token)
                .await;
        }
        if self
            .move_would_cycle(before.node_id, parent.node_id)
            .await?
        {
            return self
                .move_terminal(&request, &fingerprint, Outcome::CycleDetected, token)
                .await;
        }
        let old_path = self.state.node_path(before.node_id).await?;
        let new_path = self.create_path(&parent, &request.name).await?;
        self.validate_moved_paths(&before, &new_path).await?;
        let source_stamp = self.observe_move_source(&before, &old_path)?;
        if old_path == new_path {
            return self
                .move_terminal(&request, &fingerprint, Outcome::Moved(before), token)
                .await;
        }
        if self
            .state
            .child_exists_except(parent.node_id, &request.name, before.node_id)
            .await?
        {
            return self
                .move_terminal(&request, &fingerprint, Outcome::DestinationExists, token)
                .await;
        }
        match self.files.ensure_move_available(&old_path, &new_path) {
            Ok(()) => {}
            Err(StorageError::AlreadyExists) => {
                return self
                    .move_terminal(&request, &fingerprint, Outcome::DestinationExists, token)
                    .await;
            }
            Err(error) => return Err(error.into()),
        }
        let mut parents_before = vec![
            self.state
                .node(before.parent_id.ok_or(Error::InvalidFormat)?)
                .await?
                .ok_or(Error::InvalidFormat)?,
        ];
        if parents_before[0].node_id != parent.node_id {
            parents_before.push(parent);
        }
        for parent in &parents_before {
            parent
                .children_generation
                .checked_add(1)
                .ok_or(Error::InvalidFormat)?;
        }
        let mut proposed = before.clone();
        proposed.parent_id = Some(request.parent_id);
        proposed.name = request.name.clone();
        proposed.meta_rev = MetaRevision::new(Uuid::new_v4().to_string()).expect("generated UUID");
        proposed.last_change_seq = self.state.next_change_seq().await?;
        let destination_parent = Some(self.destination_parent_identity(&new_path)?);
        let intent = MoveIntent {
            operation_id: request.operation_id,
            before,
            proposed,
            parents_before,
            old_path,
            new_path,
            destination_parent,
            source_stamp,
            observed_hashes: Vec::new(),
        };
        self.point(Phase::BeforeAdmission)?;
        self.authorize_admission(token, request.stream_id).await?;
        self.ready = false;
        self.state
            .admit_move(&request, &fingerprint, &Outcome::Pending, Some(&intent))
            .await?;
        self.point(Phase::AfterAdmission)?;
        self.verify_mutation_root()?;
        self.verify_destination_parent(&intent.new_path, intent.destination_parent)?;
        #[cfg(test)]
        self.point(Phase::BeforeDestinationOpen)?;
        self.publish_move(&intent)?;
        self.point(Phase::BeforeFinish)?;
        self.verify_mutation_root()?;
        self.verify_destination_parent(&intent.new_path, intent.destination_parent)?;
        let receipt = self.state.finish_move(&intent, true).await?;
        self.point(Phase::AfterFinish)?;
        self.ready = true;
        Ok(receipt)
    }

    async fn move_terminal(
        &mut self,
        request: &MoveNode,
        fingerprint: &str,
        outcome: Outcome,
        token: Option<&str>,
    ) -> Result<Receipt, Error> {
        self.authorize_admission(token, request.stream_id).await?;
        self.ready = false;
        self.state
            .admit_move(request, fingerprint, &outcome, None)
            .await?;
        self.ready = true;
        Ok(Receipt {
            operation_id: request.operation_id,
            outcome,
        })
    }

    fn observe_move_source(
        &mut self,
        before: &NodeSnapshot,
        path: &str,
    ) -> Result<EntryStamp, Error> {
        match self.files.observe_entry(path)? {
            ObservedEntry::File(observed)
                if before.kind == NodeKind::File
                    && before.content_hash.as_deref() == Some(observed.stamp.hash.as_str())
                    && before.size == Some(observed.stamp.size) =>
            {
                self.put_object(&observed.bytes)?;
                Ok(EntryStamp::File(observed.stamp))
            }
            ObservedEntry::Folder { stamp, .. } if before.kind == NodeKind::Folder => {
                Ok(EntryStamp::Folder(stamp))
            }
            _ => Err(Error::ExternalChange),
        }
    }

    async fn move_would_cycle(&mut self, node: NodeId, parent: NodeId) -> Result<bool, Error> {
        let mut current = Some(parent);
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if id == node {
                return Ok(true);
            }
            if !seen.insert(id) {
                return Err(Error::InvalidFormat);
            }
            let current_node = self.state.node(id).await?.ok_or(Error::InvalidFormat)?;
            if current_node.kind != NodeKind::Folder {
                return Err(Error::InvalidFormat);
            }
            current = current_node.parent_id;
        }
        Ok(false)
    }

    async fn validate_moved_paths(
        &mut self,
        node: &NodeSnapshot,
        new_path: &str,
    ) -> Result<(), Error> {
        if new_path.len() > 1024 {
            return Err(Error::InvalidRequest);
        }
        if node.kind != NodeKind::Folder {
            return Ok(());
        }
        let nodes = self.state.all_nodes().await?;
        let mut children = HashMap::<NodeId, Vec<&NodeSnapshot>>::new();
        for child in nodes.iter().filter(|node| !node.deleted) {
            if let Some(parent) = child.parent_id {
                children.entry(parent).or_default().push(child);
            }
        }
        let mut stack = vec![(node.node_id, new_path.len())];
        let mut seen = HashSet::new();
        while let Some((id, length)) = stack.pop() {
            if !seen.insert(id) {
                return Err(Error::InvalidFormat);
            }
            if let Some(direct) = children.get(&id) {
                for child in direct {
                    let length = length + 1 + child.name.len();
                    if length > 1024 {
                        return Err(Error::InvalidRequest);
                    }
                    stack.push((child.node_id, length));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn validate_move_intent(&mut self, intent: &MoveIntent) -> Result<(), Error> {
        let before = &intent.before;
        let proposed = &intent.proposed;
        if self.state.node(before.node_id).await? != Some(before.clone())
            || before.deleted
            || proposed.deleted
            || before.parent_id.is_none()
            || before.node_id != proposed.node_id
            || before.kind != proposed.kind
            || before.content_rev != proposed.content_rev
            || before.content_hash != proposed.content_hash
            || before.size != proposed.size
            || before.children_generation != proposed.children_generation
            || before.meta_rev == proposed.meta_rev
            || proposed.last_change_seq != self.state.next_change_seq().await?
            || notryx_domain::mutation::validate_name(&proposed.name).is_err()
            || self.state.node_path(before.node_id).await? != intent.old_path
            || intent.old_path == intent.new_path
        {
            return Err(Error::InvalidFormat);
        }
        let destination = proposed.parent_id.ok_or(Error::InvalidFormat)?;
        let parent = self
            .state
            .node(destination)
            .await?
            .ok_or(Error::InvalidFormat)?;
        if parent.deleted
            || parent.kind != NodeKind::Folder
            || self.move_would_cycle(before.node_id, destination).await?
            || self.create_path(&parent, &proposed.name).await? != intent.new_path
        {
            return Err(Error::InvalidFormat);
        }
        self.validate_moved_paths(before, &intent.new_path)
            .await
            .map_err(|_| Error::InvalidFormat)?;
        let expected: HashSet<_> = [before.parent_id.ok_or(Error::InvalidFormat)?, destination]
            .into_iter()
            .collect();
        let actual: HashSet<_> = intent
            .parents_before
            .iter()
            .map(|parent| parent.node_id)
            .collect();
        if actual != expected || actual.len() != intent.parents_before.len() {
            return Err(Error::InvalidFormat);
        }
        for parent in &intent.parents_before {
            if parent.deleted
                || parent.kind != NodeKind::Folder
                || self.state.node(parent.node_id).await? != Some(parent.clone())
            {
                return Err(Error::InvalidFormat);
            }
            parent
                .children_generation
                .checked_add(1)
                .ok_or(Error::InvalidFormat)?;
        }
        match &intent.source_stamp {
            EntryStamp::File(stamp)
                if before.kind == NodeKind::File
                    && before.content_hash.as_deref() == Some(stamp.hash.as_str())
                    && before.size == Some(stamp.size) =>
            {
                if self.files.read_object(&stamp.hash)?.len() as u64 != stamp.size {
                    return Err(Error::InvalidFormat);
                }
            }
            EntryStamp::Folder(_) if before.kind == NodeKind::Folder => {}
            _ => return Err(Error::InvalidFormat),
        }
        for hash in &intent.observed_hashes {
            self.files.read_object(hash)?;
        }
        Ok(())
    }

    pub(crate) async fn recover_move(&mut self, intent: &MoveIntent) -> Result<bool, Error> {
        self.validate_move_intent(intent).await?;
        if self.state.is_quarantined(intent.operation_id).await? {
            return Ok(false);
        }
        self.verify_mutation_root()?;
        if self
            .verify_destination_parent(&intent.new_path, intent.destination_parent)
            .is_err()
        {
            self.state.quarantine_move(intent, Vec::new()).await?;
            return Ok(false);
        }
        if let Some(hash) = &intent.before.content_hash {
            let bytes = self.files.read_object(hash)?;
            self.put_object(&bytes)?;
        }
        let source = self.files.observe_entry(&intent.old_path)?;
        let destination = self.files.observe_entry(&intent.new_path)?;
        let source_stamp = entry_stamp(&source);
        let destination_stamp = entry_stamp(&destination);
        let applied = if destination_stamp
            .as_ref()
            .is_some_and(|stamp| intent.source_stamp.matches_after_move(stamp))
            && matches!(source, ObservedEntry::Absent)
        {
            Some(true)
        } else if source_stamp.as_ref() == Some(&intent.source_stamp)
            && matches!(destination, ObservedEntry::Absent)
        {
            Some(false)
        } else {
            None
        };
        if let Some(applied) = applied {
            self.sync_move_observed(intent, applied)?;
            self.point(Phase::BeforeRecoveryFinish)?;
            self.verify_mutation_root()?;
            if self
                .verify_destination_parent(&intent.new_path, intent.destination_parent)
                .is_err()
            {
                self.state.quarantine_move(intent, Vec::new()).await?;
                return Ok(false);
            }
            self.state.finish_move(intent, applied).await?;
            self.point(Phase::AfterRecoveryFinish)?;
            return Ok(true);
        }
        let mut hashes = Vec::new();
        for (path, observed) in [(&intent.old_path, source), (&intent.new_path, destination)] {
            match observed {
                ObservedEntry::File(file) => {
                    self.sync_observed(path, &file.stamp)?;
                    let hash = self.put_object(&file.bytes)?;
                    if !hashes.contains(&hash) {
                        hashes.push(hash);
                    }
                }
                ObservedEntry::Folder { stamp, .. } => self.sync_folder(path, &stamp)?,
                ObservedEntry::Absent => {}
            }
        }
        self.verify_mutation_root()?;
        self.state.quarantine_move(intent, hashes).await?;
        Ok(false)
    }

    fn publish_move(&mut self, intent: &MoveIntent) -> Result<(), Error> {
        #[cfg(test)]
        return self
            .files
            .move_entry_guarded_with_hook(
                &intent.old_path,
                &intent.new_path,
                &intent.source_stamp,
                intent.destination_parent.ok_or(Error::ExternalChange)?,
                |stage| {
                    if let Some(hook) = &mut self.hook {
                        hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                    }
                    Ok(())
                },
            )
            .map(|_| ())
            .map_err(Into::into);
        #[cfg(not(test))]
        self.files
            .move_entry_guarded(
                &intent.old_path,
                &intent.new_path,
                &intent.source_stamp,
                intent.destination_parent.ok_or(Error::ExternalChange)?,
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    fn sync_move_observed(&mut self, intent: &MoveIntent, applied: bool) -> Result<(), Error> {
        #[cfg(test)]
        return self
            .files
            .sync_move_with_hook(
                &intent.old_path,
                &intent.new_path,
                &intent.source_stamp,
                applied,
                |stage| {
                    if let Some(hook) = &mut self.hook {
                        hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                    }
                    Ok(())
                },
            )
            .map_err(Into::into);
        #[cfg(not(test))]
        self.files
            .sync_move(
                &intent.old_path,
                &intent.new_path,
                &intent.source_stamp,
                applied,
            )
            .map_err(Into::into)
    }
}

fn entry_stamp(entry: &ObservedEntry) -> Option<EntryStamp> {
    match entry {
        ObservedEntry::Absent => None,
        ObservedEntry::File(file) => Some(EntryStamp::File(file.stamp.clone())),
        ObservedEntry::Folder { stamp, .. } => Some(EntryStamp::Folder(stamp.clone())),
    }
}
