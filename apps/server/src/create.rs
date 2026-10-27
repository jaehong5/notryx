use crate::state::create::CreateIntent;
use crate::{CreateNode, Error, NodeSnapshot, Outcome, Phase, Receipt, Workspace};
use notryx_domain::MetaRevision;
use notryx_protocol::NodeKind;
use notryx_server_storage::{FolderStamp, ObservedEntry, StorageError};
use uuid::Uuid;

impl Workspace {
    pub async fn create_node(&mut self, request: CreateNode) -> Result<Receipt, Error> {
        self.create_node_authorized(request, None).await
    }

    pub(crate) async fn create_node_authorized(
        &mut self,
        request: CreateNode,
        token: Option<&str>,
    ) -> Result<Receipt, Error> {
        if request.epoch != self.epoch() {
            return Err(Error::WorkspaceMismatch);
        }
        if request.operation_seq == 0
            || notryx_domain::mutation::validate_name(&request.name).is_err()
        {
            return Err(Error::InvalidRequest);
        }
        let identity = (
            "create_node/v1",
            request.epoch,
            request.stream_id,
            request.operation_seq,
            request.operation_id,
            request.node_id,
            request.parent_id,
            &request.name,
            request.kind,
        );
        let fingerprint = crate::hash(
            &match request.upload_id {
                None => serde_json::to_vec(&identity),
                Some(id) => serde_json::to_vec(&("create_node/upload/v1", identity, id)),
            }
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
        if request.upload_id.is_some() && request.kind != NodeKind::File {
            return Err(Error::InvalidRequest);
        }
        let upload = if let Some(id) = request.upload_id {
            Some(self.attachment_upload(id, token, request.stream_id).await?)
        } else {
            None
        };
        self.state
            .validate_admission(request.stream_id, request.operation_seq)
            .await?;
        if !self.ready {
            return Err(Error::RecoveryRequired);
        }
        if self.state.node(request.node_id).await?.is_some() {
            return self
                .create_terminal(&request, &fingerprint, Outcome::NodeIdConflict, token)
                .await;
        }
        let Some(parent) = self.state.node(request.parent_id).await? else {
            return self
                .create_terminal(&request, &fingerprint, Outcome::NotFound, token)
                .await;
        };
        if parent.deleted {
            return self
                .create_terminal(&request, &fingerprint, Outcome::NodeDeleted, token)
                .await;
        }
        if parent.kind != NodeKind::Folder {
            return self
                .create_terminal(&request, &fingerprint, Outcome::ParentNotFolder, token)
                .await;
        }
        let path = self.create_path(&parent, &request.name).await?;
        if self
            .state
            .child_exists(request.parent_id, &request.name)
            .await?
        {
            return self
                .create_terminal(&request, &fingerprint, Outcome::DestinationExists, token)
                .await;
        }
        match self.files.ensure_available(&path) {
            Ok(()) => {}
            Err(StorageError::AlreadyExists) => {
                return self
                    .create_terminal(&request, &fingerprint, Outcome::DestinationExists, token)
                    .await;
            }
            Err(error) => return Err(error.into()),
        }
        parent
            .children_generation
            .checked_add(1)
            .ok_or(Error::InvalidFormat)?;
        let (content_rev, content_hash, size) = if request.kind == NodeKind::File {
            (
                Some(crate::revision()),
                Some(match &upload {
                    Some(upload) => self.files.prepare_upload_object(upload).map_err(|error| {
                        if error == StorageError::NotFound {
                            Error::Storage
                        } else {
                            error.into()
                        }
                    })?,
                    None => self.put_object(b"")?,
                }),
                Some(upload.as_ref().map_or(0, |upload| upload.size)),
            )
        } else {
            (None, None, None)
        };
        let intent = CreateIntent {
            operation_id: request.operation_id,
            upload_id: request.upload_id,
            parent_before: parent,
            created: NodeSnapshot {
                node_id: request.node_id,
                parent_id: Some(request.parent_id),
                name: request.name.clone(),
                kind: request.kind,
                content_rev,
                meta_rev: MetaRevision::new(Uuid::new_v4().to_string()).expect("generated UUID"),
                content_hash,
                size,
                children_generation: 0,
                last_change_seq: self.state.next_change_seq().await?,
                deleted: false,
            },
            path,
        };
        self.point(Phase::BeforeAdmission)?;
        self.authorize_admission(token, request.stream_id).await?;
        if let Some(id) = request.upload_id {
            self.attachment_upload(id, token, request.stream_id).await?;
        }
        self.ready = false;
        self.state
            .admit_create(&request, &fingerprint, &Outcome::Pending, Some(&intent))
            .await?;
        self.point(Phase::AfterAdmission)?;
        self.publish_node(&intent.path, &intent.created)?;
        self.point(Phase::BeforeFinish)?;
        let receipt = self.state.finish_create(&intent, true).await?;
        self.point(Phase::AfterFinish)?;
        self.ready = true;
        if let Ok(time) = crate::upload::now() {
            let _ = self.cleanup_uploads(time).await;
        }
        Ok(receipt)
    }

    pub(crate) async fn create_path(
        &mut self,
        parent: &NodeSnapshot,
        name: &str,
    ) -> Result<String, Error> {
        let parent_path = self.state.node_path(parent.node_id).await?;
        let path = if parent_path.is_empty() {
            name.into()
        } else {
            format!("{parent_path}/{name}")
        };
        if path.len() > 1024 {
            return Err(Error::InvalidRequest);
        }
        Ok(path)
    }

    async fn create_terminal(
        &mut self,
        request: &CreateNode,
        fingerprint: &str,
        outcome: Outcome,
        token: Option<&str>,
    ) -> Result<Receipt, Error> {
        self.authorize_admission(token, request.stream_id).await?;
        self.ready = false;
        self.state
            .admit_create(request, fingerprint, &outcome, None)
            .await?;
        self.ready = true;
        Ok(Receipt {
            operation_id: request.operation_id,
            outcome,
        })
    }

    pub(crate) async fn validate_create_intent(
        &mut self,
        intent: &CreateIntent,
    ) -> Result<(), Error> {
        let node = &intent.created;
        if node.deleted
            || intent.parent_before.deleted
            || intent.parent_before.kind != NodeKind::Folder
            || self.state.node(intent.parent_before.node_id).await?
                != Some(intent.parent_before.clone())
            || self.state.node(node.node_id).await?.is_some()
            || node.parent_id != Some(intent.parent_before.node_id)
            || node.children_generation != 0
            || node.last_change_seq != self.state.next_change_seq().await?
            || notryx_domain::mutation::validate_name(&node.name).is_err()
            || self.create_path(&intent.parent_before, &node.name).await? != intent.path
        {
            return Err(Error::InvalidFormat);
        }
        match node.kind {
            NodeKind::File => {
                let hash = node.content_hash.as_deref().ok_or(Error::InvalidFormat)?;
                let size = node.size.ok_or(Error::InvalidFormat)?;
                if node.content_rev.is_none()
                    || size > notryx_server_storage::MAX_FILE_BYTES as u64
                    || (intent.upload_id.is_none() && (hash != crate::hash(b"") || size != 0))
                    || self.files.read_object(hash)?.len() as u64 != size
                {
                    return Err(Error::InvalidFormat);
                }
            }
            NodeKind::Folder => {
                if intent.upload_id.is_some()
                    || node.content_rev.is_some()
                    || node.content_hash.is_some()
                    || node.size.is_some()
                {
                    return Err(Error::InvalidFormat);
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn recover_create(&mut self, intent: &CreateIntent) -> Result<bool, Error> {
        self.validate_create_intent(intent).await?;
        if self.state.is_quarantined(intent.operation_id).await? {
            return Ok(false);
        }
        if let Some(hash) = &intent.created.content_hash {
            let bytes = self.files.read_object(hash)?;
            self.put_object(&bytes)?;
        }
        let observed = self.files.observe_entry(&intent.path)?;
        let applied = match observed {
            ObservedEntry::Absent => {
                self.sync_absent(&intent.path)?;
                false
            }
            ObservedEntry::File(observed) => {
                self.sync_observed(&intent.path, &observed.stamp)?;
                if intent.created.kind != NodeKind::File
                    || intent.created.content_hash.as_deref() != Some(observed.stamp.hash.as_str())
                {
                    let hash = self.put_object(&observed.bytes)?;
                    self.state
                        .quarantine(intent.operation_id, Some(&hash))
                        .await?;
                    return Ok(false);
                }
                true
            }
            ObservedEntry::Folder { stamp, empty } => {
                self.sync_folder(&intent.path, &stamp)?;
                if intent.created.kind != NodeKind::Folder || !empty {
                    self.state.quarantine(intent.operation_id, None).await?;
                    return Ok(false);
                }
                true
            }
        };
        self.point(Phase::BeforeRecoveryFinish)?;
        self.state.finish_create(intent, applied).await?;
        self.point(Phase::AfterRecoveryFinish)?;
        Ok(true)
    }

    pub(crate) fn publish_node(&mut self, path: &str, node: &NodeSnapshot) -> Result<(), Error> {
        #[cfg(test)]
        {
            let hook = &mut self.hook;
            let mut point = |stage| {
                if let Some(hook) = hook {
                    hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                }
                Ok(())
            };
            match node.kind {
                NodeKind::File => self
                    .files
                    .create_file_with_hook(
                        path,
                        node.content_hash.as_deref().ok_or(Error::InvalidFormat)?,
                        &mut point,
                    )
                    .map(|_| ())
                    .map_err(Into::into),
                NodeKind::Folder => self
                    .files
                    .create_folder_with_hook(path, &mut point)
                    .map(|_| ())
                    .map_err(Into::into),
            }
        }
        #[cfg(not(test))]
        match node.kind {
            NodeKind::File => self
                .files
                .create_file(
                    path,
                    node.content_hash.as_deref().ok_or(Error::InvalidFormat)?,
                )
                .map(|_| ())
                .map_err(Into::into),
            NodeKind::Folder => self
                .files
                .create_folder(path)
                .map(|_| ())
                .map_err(Into::into),
        }
    }

    pub(crate) fn sync_absent(&mut self, path: &str) -> Result<(), Error> {
        #[cfg(test)]
        return self
            .files
            .sync_absent_with_hook(path, |stage| {
                if let Some(hook) = &mut self.hook {
                    hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                }
                Ok(())
            })
            .map_err(Into::into);
        #[cfg(not(test))]
        self.files.sync_absent(path).map_err(Into::into)
    }

    pub(crate) fn sync_folder(&mut self, path: &str, stamp: &FolderStamp) -> Result<(), Error> {
        #[cfg(test)]
        return self
            .files
            .sync_folder_with_hook(path, stamp, |stage| {
                if let Some(hook) = &mut self.hook {
                    hook(Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                }
                Ok(())
            })
            .map_err(Into::into);
        #[cfg(not(test))]
        self.files.sync_folder(path, stamp).map_err(Into::into)
    }
}
