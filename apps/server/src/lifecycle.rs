use crate::state::lifecycle::{LifecycleAction, LifecycleIntent};
use crate::{DeleteNode, Error, NodeSnapshot, Outcome, Phase, Receipt, RestoreNode, Workspace};
use notryx_domain::{MetaRevision, NodeId, OperationId};
use notryx_protocol::{NodeKind, TrashNode, TrashPage};
use notryx_server_storage::{EntryStamp, ObservedEntry, StorageError};
use uuid::Uuid;

struct LifecycleOperation {
    stream_id: Uuid,
    sequence: u64,
    operation_id: OperationId,
    fingerprint: String,
}

impl Workspace {
    pub async fn delete_node(&mut self, request: DeleteNode) -> Result<Receipt, Error> {
        self.delete_node_authorized(request, None).await
    }

    pub(crate) async fn delete_node_authorized(
        &mut self,
        request: DeleteNode,
        token: Option<&str>,
    ) -> Result<Receipt, Error> {
        if request.epoch != self.epoch() {
            return Err(Error::WorkspaceMismatch);
        }
        if request.operation_seq == 0
            || request.expected_node_etag.len() > 256
            || notryx_protocol::StrongEtag::parse(&request.expected_node_etag).is_err()
        {
            return Err(Error::InvalidRequest);
        }
        let operation = LifecycleOperation {
            stream_id: request.stream_id,
            sequence: request.operation_seq,
            operation_id: request.operation_id,
            fingerprint: crate::hash(
                &serde_json::to_vec(&(
                    "delete_node/v1",
                    request.epoch,
                    request.stream_id,
                    request.operation_seq,
                    request.operation_id,
                    request.node_id,
                    &request.expected_node_etag,
                ))
                .map_err(crate::state::serialization_error)?,
            ),
        };
        if let Some(previous) = self.begin_lifecycle(&operation, token).await? {
            return Ok(previous);
        }
        let Some(before) = self.state.node(request.node_id).await? else {
            return self
                .lifecycle_terminal(&operation, Outcome::NotFound, token)
                .await;
        };
        if before.parent_id.is_none() {
            return self
                .lifecycle_terminal(&operation, Outcome::RootImmutable, token)
                .await;
        }
        if before.deleted {
            return self
                .lifecycle_terminal(&operation, Outcome::NodeDeleted, token)
                .await;
        }
        let current = before.etag()?;
        if current != request.expected_node_etag {
            return self
                .lifecycle_terminal(
                    &operation,
                    Outcome::NodePreconditionFailed { current },
                    token,
                )
                .await;
        }
        if self.state.has_live_children(before.node_id).await? {
            return self
                .lifecycle_terminal(&operation, Outcome::FolderNotEmpty, token)
                .await;
        }
        let path = self.state.node_path(before.node_id).await?;
        let source_stamp = match self.files.observe_entry(&path)? {
            ObservedEntry::File(observed)
                if before.kind == NodeKind::File
                    && before.content_hash.as_deref() == Some(observed.stamp.hash.as_str())
                    && before.size == Some(observed.stamp.size) =>
            {
                self.put_object(&observed.bytes)?;
                EntryStamp::File(observed.stamp)
            }
            ObservedEntry::Folder { stamp, empty } if before.kind == NodeKind::Folder => {
                if !empty {
                    return self
                        .lifecycle_terminal(&operation, Outcome::FolderNotEmpty, token)
                        .await;
                }
                EntryStamp::Folder(stamp)
            }
            _ => return Err(Error::ExternalChange),
        };
        let parent = self
            .state
            .node(before.parent_id.ok_or(Error::InvalidFormat)?)
            .await?
            .ok_or(Error::InvalidFormat)?;
        let mut proposed = self.lifecycle_proposed(&before).await?;
        proposed.deleted = true;
        let intent = LifecycleIntent {
            operation_id: request.operation_id,
            before,
            proposed,
            parent_before: parent,
            path,
            action: LifecycleAction::Delete { source_stamp },
        };
        self.apply_lifecycle(&operation, &intent, token).await
    }

    pub async fn restore_node(&mut self, request: RestoreNode) -> Result<Receipt, Error> {
        self.restore_node_authorized(request, None).await
    }

    pub(crate) async fn restore_node_authorized(
        &mut self,
        request: RestoreNode,
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
        let operation = LifecycleOperation {
            stream_id: request.stream_id,
            sequence: request.operation_seq,
            operation_id: request.operation_id,
            fingerprint: crate::hash(
                &serde_json::to_vec(&(
                    "restore_node/v1",
                    request.epoch,
                    request.stream_id,
                    request.operation_seq,
                    request.operation_id,
                    request.node_id,
                    &request.expected_meta_rev,
                    request.parent_id,
                    &request.name,
                ))
                .map_err(crate::state::serialization_error)?,
            ),
        };
        if let Some(previous) = self.begin_lifecycle(&operation, token).await? {
            return Ok(previous);
        }
        let Some(before) = self.state.node(request.node_id).await? else {
            return self
                .lifecycle_terminal(&operation, Outcome::NotFound, token)
                .await;
        };
        if !before.deleted {
            return self
                .lifecycle_terminal(&operation, Outcome::NotDeleted, token)
                .await;
        }
        if before.meta_rev != request.expected_meta_rev {
            return self
                .lifecycle_terminal(
                    &operation,
                    Outcome::MetadataPreconditionFailed {
                        current: before.meta_rev,
                    },
                    token,
                )
                .await;
        }
        let Some(parent) = self.state.node(request.parent_id).await? else {
            return self
                .lifecycle_terminal(&operation, Outcome::NotFound, token)
                .await;
        };
        if parent.deleted {
            return self
                .lifecycle_terminal(&operation, Outcome::NodeDeleted, token)
                .await;
        }
        if parent.kind != NodeKind::Folder {
            return self
                .lifecycle_terminal(&operation, Outcome::ParentNotFolder, token)
                .await;
        }
        let path = self.create_path(&parent, &request.name).await?;
        if self
            .state
            .child_exists(parent.node_id, &request.name)
            .await?
        {
            return self
                .lifecycle_terminal(&operation, Outcome::DestinationExists, token)
                .await;
        }
        match self.files.ensure_available(&path) {
            Ok(()) => {}
            Err(StorageError::AlreadyExists) => {
                return self
                    .lifecycle_terminal(&operation, Outcome::DestinationExists, token)
                    .await;
            }
            Err(error) => return Err(error.into()),
        }
        if let Some(hash) = &before.content_hash {
            match self.files.read_object(hash) {
                Ok(bytes) if Some(bytes.len() as u64) == before.size => {
                    self.put_object(&bytes)?;
                }
                Err(StorageError::NotFound) => {
                    return self
                        .lifecycle_terminal(&operation, Outcome::HistoryExpired, token)
                        .await;
                }
                Ok(_) => return Err(Error::InvalidFormat),
                Err(error) => return Err(error.into()),
            }
        }
        let mut proposed = self.lifecycle_proposed(&before).await?;
        proposed.deleted = false;
        proposed.parent_id = Some(parent.node_id);
        proposed.name = request.name;
        let intent = LifecycleIntent {
            operation_id: request.operation_id,
            before,
            proposed,
            parent_before: parent,
            path,
            action: LifecycleAction::Restore,
        };
        self.apply_lifecycle(&operation, &intent, token).await
    }

    async fn begin_lifecycle(
        &mut self,
        operation: &LifecycleOperation,
        token: Option<&str>,
    ) -> Result<Option<Receipt>, Error> {
        if let Some(previous) = self
            .state
            .previous_identity(
                operation.stream_id,
                operation.sequence,
                operation.operation_id,
                Some(&operation.fingerprint),
            )
            .await?
        {
            return Ok(Some(previous));
        }
        self.authorize_admission(token, operation.stream_id).await?;
        self.state
            .validate_admission(operation.stream_id, operation.sequence)
            .await?;
        if !self.ready {
            return Err(Error::RecoveryRequired);
        }
        Ok(None)
    }

    async fn lifecycle_proposed(&mut self, before: &NodeSnapshot) -> Result<NodeSnapshot, Error> {
        let mut proposed = before.clone();
        proposed.meta_rev = MetaRevision::new(Uuid::new_v4().to_string()).expect("generated UUID");
        if before.kind == NodeKind::File {
            proposed.content_rev = Some(crate::revision());
        }
        proposed.last_change_seq = self.state.next_change_seq().await?;
        Ok(proposed)
    }

    async fn lifecycle_terminal(
        &mut self,
        operation: &LifecycleOperation,
        outcome: Outcome,
        token: Option<&str>,
    ) -> Result<Receipt, Error> {
        self.authorize_admission(token, operation.stream_id).await?;
        self.ready = false;
        self.state
            .admit_lifecycle(
                operation.stream_id,
                operation.sequence,
                operation.operation_id,
                &operation.fingerprint,
                &outcome,
                None,
            )
            .await?;
        self.ready = true;
        Ok(Receipt {
            operation_id: operation.operation_id,
            outcome,
        })
    }

    async fn apply_lifecycle(
        &mut self,
        operation: &LifecycleOperation,
        intent: &LifecycleIntent,
        token: Option<&str>,
    ) -> Result<Receipt, Error> {
        intent
            .parent_before
            .children_generation
            .checked_add(1)
            .ok_or(Error::InvalidFormat)?;
        self.point(Phase::BeforeAdmission)?;
        self.authorize_admission(token, operation.stream_id).await?;
        self.ready = false;
        self.state
            .admit_lifecycle(
                operation.stream_id,
                operation.sequence,
                operation.operation_id,
                &operation.fingerprint,
                &Outcome::Pending,
                Some(intent),
            )
            .await?;
        self.point(Phase::AfterAdmission)?;
        match &intent.action {
            LifecycleAction::Delete { source_stamp } => {
                self.publish_deleted(&intent.path, source_stamp)?
            }
            LifecycleAction::Restore => self.publish_node(&intent.path, &intent.proposed)?,
        }
        self.point(Phase::BeforeFinish)?;
        let receipt = self.state.finish_lifecycle(intent, true).await?;
        self.point(Phase::AfterFinish)?;
        self.ready = true;
        Ok(receipt)
    }

    pub(crate) async fn validate_lifecycle_intent(
        &mut self,
        intent: &LifecycleIntent,
    ) -> Result<(), Error> {
        let before = &intent.before;
        let node = &intent.proposed;
        let parent = &intent.parent_before;
        if self.state.node(before.node_id).await? != Some(before.clone())
            || self.state.node(parent.node_id).await? != Some(parent.clone())
            || before.parent_id.is_none()
            || parent.deleted
            || parent.kind != NodeKind::Folder
            || node.node_id != before.node_id
            || node.kind != before.kind
            || node.content_hash != before.content_hash
            || node.size != before.size
            || node.children_generation != before.children_generation
            || node.meta_rev == before.meta_rev
            || (node.kind == NodeKind::File
                && (node.content_rev.is_none() || node.content_rev == before.content_rev))
            || (node.kind == NodeKind::Folder && node.content_rev.is_some())
            || node.deleted == before.deleted
            || node.last_change_seq != self.state.next_change_seq().await?
            || notryx_domain::mutation::validate_name(&node.name).is_err()
            || self.create_path(parent, &node.name).await? != intent.path
            || node.parent_id != Some(parent.node_id)
        {
            return Err(Error::InvalidFormat);
        }
        parent
            .children_generation
            .checked_add(1)
            .ok_or(Error::InvalidFormat)?;
        match &intent.action {
            LifecycleAction::Delete { source_stamp } => {
                if before.deleted
                    || !node.deleted
                    || node.parent_id != before.parent_id
                    || node.name != before.name
                    || self.state.node_path(before.node_id).await? != intent.path
                    || self.state.has_live_children(node.node_id).await?
                {
                    return Err(Error::InvalidFormat);
                }
                match source_stamp {
                    EntryStamp::File(stamp)
                        if node.kind == NodeKind::File
                            && node.content_hash.as_deref() == Some(stamp.hash.as_str())
                            && node.size == Some(stamp.size) => {}
                    EntryStamp::Folder(_) if node.kind == NodeKind::Folder => {}
                    _ => return Err(Error::InvalidFormat),
                }
            }
            LifecycleAction::Restore => {
                if !before.deleted
                    || node.deleted
                    || self.state.has_live_children(node.node_id).await?
                    || self.state.child_exists(parent.node_id, &node.name).await?
                {
                    return Err(Error::InvalidFormat);
                }
            }
        }
        if let Some(hash) = &node.content_hash
            && Some(self.files.read_object(hash)?.len() as u64) != node.size
        {
            return Err(Error::InvalidFormat);
        }
        if let Some(hash) = self.state.quarantine_hash(intent.operation_id).await? {
            self.files.read_object(&hash)?;
        }
        Ok(())
    }

    pub(crate) async fn recover_lifecycle(
        &mut self,
        intent: &LifecycleIntent,
    ) -> Result<bool, Error> {
        self.validate_lifecycle_intent(intent).await?;
        if self.state.is_quarantined(intent.operation_id).await? {
            return Ok(false);
        }
        if let Some(hash) = &intent.proposed.content_hash {
            let bytes = self.files.read_object(hash)?;
            self.put_object(&bytes)?;
        }
        let observed = self.files.observe_entry(&intent.path)?;
        let deleting = matches!(intent.action, LifecycleAction::Delete { .. });
        let applied = match &observed {
            ObservedEntry::Absent => {
                self.sync_absent(&intent.path)?;
                Some(deleting)
            }
            ObservedEntry::File(file) => {
                self.sync_observed(&intent.path, &file.stamp)?;
                let matches = match &intent.action {
                    LifecycleAction::Delete { source_stamp } => {
                        *source_stamp == EntryStamp::File(file.stamp.clone())
                    }
                    LifecycleAction::Restore => {
                        intent.proposed.kind == NodeKind::File
                            && intent.proposed.content_hash.as_deref()
                                == Some(file.stamp.hash.as_str())
                            && intent.proposed.size == Some(file.stamp.size)
                    }
                };
                matches.then_some(!deleting)
            }
            ObservedEntry::Folder { stamp, empty } => {
                self.sync_folder(&intent.path, stamp)?;
                let matches = match &intent.action {
                    LifecycleAction::Delete { source_stamp } => {
                        *empty && *source_stamp == EntryStamp::Folder(stamp.clone())
                    }
                    LifecycleAction::Restore => *empty && intent.proposed.kind == NodeKind::Folder,
                };
                matches.then_some(!deleting)
            }
        };
        if let Some(applied) = applied {
            self.point(Phase::BeforeRecoveryFinish)?;
            self.state.finish_lifecycle(intent, applied).await?;
            self.point(Phase::AfterRecoveryFinish)?;
            return Ok(true);
        }
        let hash = match observed {
            ObservedEntry::File(file) => Some(self.put_object(&file.bytes)?),
            _ => None,
        };
        self.state
            .quarantine(intent.operation_id, hash.as_deref())
            .await?;
        Ok(false)
    }

    pub async fn trash(
        &mut self,
        after_id: Option<NodeId>,
        limit: u16,
    ) -> Result<TrashPage, Error> {
        let nodes = self.state.trash_nodes(after_id, limit).await?;
        let mut page = TrashPage {
            workspace_epoch: self.epoch(),
            nodes: Vec::new(),
            next_after_id: None,
        };
        for (index, node) in nodes.iter().take(usize::from(limit)).enumerate() {
            let restorable = match &node.content_hash {
                None => true,
                Some(hash) => match self.files.read_object(hash) {
                    Ok(bytes) => Some(bytes.len() as u64) == node.size,
                    Err(StorageError::NotFound) => false,
                    Err(error) => return Err(error.into()),
                },
            };
            page.nodes.push(TrashNode {
                node_id: node.node_id,
                parent_id: node.parent_id.ok_or(Error::InvalidFormat)?,
                name: node.name.clone(),
                kind: node.kind,
                meta_rev: node.meta_rev.clone(),
                restorable,
            });
            page.next_after_id = (index + 1 < nodes.len()).then_some(node.node_id);
            if serde_json::to_vec(&page)
                .map_err(crate::state::serialization_error)?
                .len()
                > 1024 * 1024
            {
                page.nodes.pop();
                page.next_after_id = Some(page.nodes.last().ok_or(Error::InvalidFormat)?.node_id);
                break;
            }
        }
        Ok(page)
    }

    fn publish_deleted(&mut self, path: &str, stamp: &EntryStamp) -> Result<(), Error> {
        #[cfg(test)]
        return self
            .files
            .remove_entry_with_hook(path, stamp, |stage| {
                if let Some(hook) = &mut self.hook {
                    hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                }
                Ok(())
            })
            .map_err(Into::into);
        #[cfg(not(test))]
        self.files.remove_entry(path, stamp).map_err(Into::into)
    }
}
