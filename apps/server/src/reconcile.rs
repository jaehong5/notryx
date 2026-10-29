use crate::{Document, Error, NodeId, Workspace, revision};
use notryx_domain::WorkspaceEpoch;
use notryx_server_storage::{
    FolderIdentity, ObservationPreparationError, ObservationPreparationFailure, ObservationReader,
    PreparedObservation, StorageError, StorageStage,
};
use std::sync::Arc;
use std::time::Duration;

pub(crate) enum ExternalDiagnostic {
    Clear,
    VerifyObject(String, Option<Error>),
}

pub(crate) struct ExternalDeleteProbe {
    epoch: WorkspaceEpoch,
    before: crate::NodeSnapshot,
    parent: crate::NodeSnapshot,
    path: String,
    watch_parent: FolderIdentity,
    reader: ObservationReader,
}

pub(crate) struct ExternalDeleteCandidate {
    epoch: WorkspaceEpoch,
    before: crate::NodeSnapshot,
    parent: crate::NodeSnapshot,
    path: String,
    watch_parent: FolderIdentity,
    previous: Option<Result<notryx_server_storage::FileStamp, StorageError>>,
    absence: Result<(), StorageError>,
}

impl ExternalDeleteProbe {
    pub(crate) fn observe_folder_absence(self) -> Result<bool, Error> {
        if self.before.kind != notryx_protocol::NodeKind::Folder {
            return Err(Error::ExternalChange);
        }
        match self
            .reader
            .sync_absent_guarded(&self.path, self.watch_parent)
        {
            Ok(()) => Ok(true),
            Err(StorageError::Changed) => self
                .reader
                .observe_watch_child_folder(&self.path, self.watch_parent)
                .map(|_| false)
                .map_err(Error::from),
            Err(error) => Err(Error::from(error)),
        }
    }

    pub(crate) fn observe(self) -> ExternalDeleteCandidate {
        self.observe_with(|| {})
    }

    #[cfg(test)]
    pub(crate) fn observe_with_hook(self, hook: impl FnOnce()) -> ExternalDeleteCandidate {
        self.observe_with(hook)
    }

    fn observe_with(self, hook: impl FnOnce()) -> ExternalDeleteCandidate {
        let previous = match (
            self.before.kind,
            self.before.content_hash.as_deref(),
            self.before.size,
        ) {
            (notryx_protocol::NodeKind::Folder, None, None) => None,
            (notryx_protocol::NodeKind::File, Some(hash), Some(size)) => {
                Some(self.reader.observe_object(hash).and_then(|stamp| {
                    if stamp.hash == hash && stamp.size == size {
                        Ok(stamp)
                    } else {
                        Err(StorageError::ChecksumMismatch)
                    }
                }))
            }
            _ => Some(Err(StorageError::InvalidHash)),
        };
        hook();
        let absence = self
            .reader
            .sync_absent_guarded(&self.path, self.watch_parent);
        ExternalDeleteCandidate {
            epoch: self.epoch,
            before: self.before,
            parent: self.parent,
            path: self.path,
            watch_parent: self.watch_parent,
            previous,
            absence,
        }
    }
}

pub(crate) struct ExternalProbe {
    epoch: WorkspaceEpoch,
    before: Document,
    path: String,
    reader: ObservationReader,
    generation: Arc<()>,
    watch_parent: Option<(NodeId, FolderIdentity)>,
}

pub(crate) struct ExternalCandidate {
    epoch: WorkspaceEpoch,
    before: Document,
    path: String,
    observed_hash: String,
    generation: Arc<()>,
    watch_parent: Option<(NodeId, FolderIdentity)>,
    prepared: Result<PreparedObservation, ObservationPreparationFailure>,
}

impl ExternalProbe {
    pub(crate) fn observe(self) -> Result<ExternalCandidate, Error> {
        self.observe_inner(|| {}, &mut |_| Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn observe_with_hook(self, hook: impl FnOnce()) -> Result<ExternalCandidate, Error> {
        self.observe_inner(hook, &mut |_| Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn observe_with_storage_hook(
        self,
        mut hook: impl FnMut(StorageStage) -> Result<(), Error>,
    ) -> Result<ExternalCandidate, Error> {
        self.observe_inner(|| {}, &mut hook)
    }

    fn observe_inner(
        self,
        hook: impl FnOnce(),
        storage_hook: &mut impl FnMut(StorageStage) -> Result<(), Error>,
    ) -> Result<ExternalCandidate, Error> {
        let first = self.reader.observe(&self.path)?.stamp;
        hook();
        std::thread::sleep(Duration::from_millis(50));
        let observed = self.reader.observe(&self.path)?;
        if observed.stamp != first {
            return Err(Error::ExternalChange);
        }
        let observed_hash = observed.stamp.hash.clone();
        #[cfg(test)]
        let prepared = self.reader.prepare_with_hook(
            &self.path,
            observed,
            &self.before.content_hash,
            self.before.size,
            |stage| storage_hook(stage).map_err(|_| StorageError::Io),
        );
        #[cfg(not(test))]
        let prepared = {
            let _ = storage_hook;
            self.reader.prepare(
                &self.path,
                observed,
                &self.before.content_hash,
                self.before.size,
            )
        };
        Ok(ExternalCandidate {
            epoch: self.epoch,
            before: self.before,
            path: self.path,
            observed_hash,
            generation: self.generation,
            watch_parent: self.watch_parent,
            prepared,
        })
    }
}

impl Workspace {
    pub(crate) async fn prepare_external_delete(
        &mut self,
        node_id: NodeId,
        parent_id: NodeId,
        identity: FolderIdentity,
    ) -> Result<ExternalDeleteProbe, Error> {
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        self.verify_mutation_root()?;
        let before = self.state.node(node_id).await?.ok_or(Error::NotFound)?;
        if before.deleted || before.parent_id != Some(parent_id) {
            return Err(Error::ExternalChange);
        }
        let parent = self
            .state
            .node(parent_id)
            .await?
            .ok_or(Error::ExternalChange)?;
        if parent.deleted || parent.kind != notryx_protocol::NodeKind::Folder {
            return Err(Error::ExternalChange);
        }
        self.verify_watched_create_parent(parent_id, identity)
            .await?;
        let path = self.state.node_path(node_id).await?;
        let reader = self.files.observation_reader()?;
        Ok(ExternalDeleteProbe {
            epoch: self.epoch(),
            before,
            parent,
            path,
            watch_parent: identity,
            reader,
        })
    }

    pub(crate) async fn diagnose_external_delete(
        &mut self,
        candidate: &ExternalDeleteCandidate,
    ) -> Result<ExternalDiagnostic, Error> {
        if candidate.epoch != self.epoch() {
            return Err(Error::ExternalChange);
        }
        let Some(previous) = &candidate.previous else {
            return if candidate.before.kind == notryx_protocol::NodeKind::Folder {
                Ok(ExternalDiagnostic::Clear)
            } else {
                Err(Error::InvalidFormat)
            };
        };
        if let Err(error) = previous {
            let hash = candidate
                .before
                .content_hash
                .as_deref()
                .ok_or(Error::InvalidFormat)?;
            let referenced = match self.state.retained_object_is_referenced(hash).await {
                Ok(value) => value,
                Err(error) => {
                    self.ready = false;
                    return Err(error);
                }
            };
            if referenced {
                self.ready = false;
                return Err(match error {
                    StorageError::ChecksumMismatch | StorageError::InvalidHash => {
                        Error::InvalidFormat
                    }
                    _ => Error::Storage,
                });
            }
            return Err(Error::ExternalChange);
        }
        let hash = candidate
            .before
            .content_hash
            .as_deref()
            .ok_or(Error::InvalidFormat)?;
        let stamp = previous.as_ref().map_err(|_| Error::Storage)?;
        match self.files.object_matches_observation(hash, stamp) {
            Ok(true) => Ok(ExternalDiagnostic::Clear),
            Ok(false)
            | Err(StorageError::NotFound | StorageError::UnsafeFile | StorageError::Changed) => {
                Ok(ExternalDiagnostic::VerifyObject(hash.to_owned(), None))
            }
            Err(_) => {
                self.ready = false;
                Err(Error::Storage)
            }
        }
    }

    pub(crate) async fn commit_external_delete(
        &mut self,
        candidate: ExternalDeleteCandidate,
        watch_generation: &std::sync::atomic::AtomicU64,
        expected_watch_generation: u64,
        #[cfg(test)] pause: Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
        #[cfg(test)] transaction_pause: Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    ) -> Result<(), Error> {
        if matches!(
            self.diagnose_external_delete(&candidate).await?,
            ExternalDiagnostic::VerifyObject(..)
        ) {
            return Err(Error::ExternalChange);
        }
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        candidate.absence.map_err(Error::from)?;
        if candidate.epoch != self.epoch()
            || self.state.node(candidate.before.node_id).await?.as_ref() != Some(&candidate.before)
            || self.state.node(candidate.parent.node_id).await?.as_ref() != Some(&candidate.parent)
            || self.state.node_path(candidate.before.node_id).await? != candidate.path
        {
            return Err(Error::ExternalChange);
        }
        if candidate.before.kind == notryx_protocol::NodeKind::Folder
            && self
                .state
                .has_live_children(candidate.before.node_id)
                .await?
        {
            return Err(Error::ExternalChange);
        }
        self.verify_mutation_root()?;
        self.files
            .verify_absent_guarded(&candidate.path, candidate.watch_parent)?;
        let mut proposed = candidate.before.clone();
        proposed.deleted = true;
        if proposed.kind == notryx_protocol::NodeKind::File {
            proposed.content_rev = Some(revision());
        }
        proposed.meta_rev = notryx_domain::MetaRevision::new(uuid::Uuid::new_v4().to_string())
            .expect("generated UUID");
        proposed.last_change_seq = self.state.next_change_seq().await?;
        #[cfg(test)]
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.await;
        }
        if expected_watch_generation & 1 != 0
            || watch_generation.load(std::sync::atomic::Ordering::Acquire)
                != expected_watch_generation
        {
            return Err(Error::ExternalChange);
        }
        self.ready = false;
        let result = self
            .state
            .finish_external_delete(
                &candidate.before,
                &proposed,
                &candidate.parent,
                watch_generation,
                expected_watch_generation,
                #[cfg(test)]
                transaction_pause,
            )
            .await;
        if result.is_ok() || matches!(&result, Err(Error::ExternalChange)) {
            self.ready = true;
        }
        result
    }
    pub(crate) async fn prepare_external_file(
        &mut self,
        node_id: NodeId,
    ) -> Result<ExternalProbe, Error> {
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        self.verify_mutation_root()?;
        let before = self.state.document(node_id).await?.ok_or(Error::NotFound)?;
        let path = self.state.node_path(node_id).await?;
        let reader = self.files.observation_reader()?;
        self.external_generations
            .retain(|_, generation| generation.strong_count() > 0);
        let generation = Arc::new(());
        self.external_generations
            .insert(node_id, Arc::downgrade(&generation));
        Ok(ExternalProbe {
            epoch: self.epoch(),
            before,
            path,
            reader,
            generation,
            watch_parent: None,
        })
    }

    pub(crate) async fn prepare_watched_external_file(
        &mut self,
        node_id: NodeId,
        parent_id: NodeId,
        identity: FolderIdentity,
    ) -> Result<ExternalProbe, Error> {
        let mut probe = self.prepare_external_file(node_id).await?;
        self.verify_watched_parent(&probe.before, parent_id, identity)
            .await?;
        probe.watch_parent = Some((parent_id, identity));
        Ok(probe)
    }

    async fn verify_watched_parent(
        &mut self,
        before: &Document,
        parent_id: NodeId,
        identity: FolderIdentity,
    ) -> Result<(), Error> {
        if before.parent_id != parent_id {
            return Err(Error::ExternalChange);
        }
        let path = self.state.node_path(parent_id).await?;
        if !self
            .files
            .watch_folder_matches(&path, identity)
            .map_err(|_| Error::ExternalChange)?
        {
            return Err(Error::ExternalChange);
        }
        Ok(())
    }

    pub(crate) async fn diagnose_external_file(
        &mut self,
        candidate: &ExternalCandidate,
    ) -> Result<ExternalDiagnostic, Error> {
        if !self.ready {
            return Err(Error::RecoveryRequired);
        }
        if candidate.epoch != self.epoch() {
            return Err(Error::ExternalChange);
        }
        let referenced_fault = match candidate
            .prepared
            .as_ref()
            .err()
            .map(|failure| failure.error)
        {
            Some(ObservationPreparationError::Previous(error)) => {
                Some((&candidate.before.content_hash, error))
            }
            Some(ObservationPreparationError::Proposed(
                error @ (StorageError::ChecksumMismatch | StorageError::InvalidHash),
            )) => Some((&candidate.observed_hash, error)),
            _ => None,
        };
        if let Some((hash, error)) = referenced_fault {
            let referenced = match self.state.retained_object_is_referenced(hash).await {
                Ok(value) => value,
                Err(error) => {
                    self.ready = false;
                    return Err(error);
                }
            };
            if referenced {
                self.ready = false;
                return Err(match error {
                    StorageError::ChecksumMismatch | StorageError::InvalidHash => {
                        Error::InvalidFormat
                    }
                    _ => Error::Storage,
                });
            }
        }
        self.diagnose_unavailable_proposed_object(&candidate.observed_hash, &candidate.prepared)
            .await
    }

    async fn diagnose_unavailable_proposed_object(
        &mut self,
        hash: &str,
        prepared: &Result<PreparedObservation, ObservationPreparationFailure>,
    ) -> Result<ExternalDiagnostic, Error> {
        let unavailable = match prepared {
            Ok(prepared) => prepared.object_was_missing(),
            Err(failure) => {
                failure.proposed_object_missing
                    || matches!(
                        failure.error,
                        ObservationPreparationError::Proposed(StorageError::UnsafeFile)
                    )
            }
        };
        if !unavailable {
            return Ok(ExternalDiagnostic::Clear);
        }
        let referenced = match self.state.retained_object_is_referenced(hash).await {
            Ok(value) => value,
            Err(error) => {
                self.ready = false;
                return Err(error);
            }
        };
        if !referenced {
            return Ok(ExternalDiagnostic::Clear);
        }
        match self.files.object_file_present(hash) {
            Ok(true) => {
                let preparation_error =
                    prepared.as_ref().err().map(|failure| match failure.error {
                        ObservationPreparationError::Previous(error)
                        | ObservationPreparationError::Proposed(error)
                        | ObservationPreparationError::Source(error) => error.into(),
                    });
                Ok(ExternalDiagnostic::VerifyObject(
                    hash.to_owned(),
                    preparation_error,
                ))
            }
            Ok(false) | Err(_) => {
                self.ready = false;
                Err(Error::Storage)
            }
        }
    }

    pub(crate) async fn commit_external_file(
        &mut self,
        candidate: ExternalCandidate,
    ) -> Result<Option<Document>, Error> {
        if matches!(
            self.diagnose_external_file(&candidate).await?,
            ExternalDiagnostic::VerifyObject(..)
        ) {
            return Err(Error::ExternalChange);
        }
        let latest = self
            .external_generations
            .get(&candidate.before.node_id)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|generation| Arc::ptr_eq(&generation, &candidate.generation));
        if !latest {
            return Err(Error::ExternalChange);
        }
        if self
            .state
            .document(candidate.before.node_id)
            .await?
            .as_ref()
            != Some(&candidate.before)
            || self.state.node_path(candidate.before.node_id).await? != candidate.path
        {
            return Err(Error::ExternalChange);
        }
        self.verify_mutation_root()?;
        if let Some((parent_id, identity)) = candidate.watch_parent {
            self.verify_watched_parent(&candidate.before, parent_id, identity)
                .await?;
        }
        let prepared = match candidate.prepared {
            Ok(prepared) => prepared,
            Err(ObservationPreparationFailure {
                error: ObservationPreparationError::Previous(error),
                ..
            }) => {
                self.ready = false;
                return Err(match error {
                    StorageError::ChecksumMismatch | StorageError::InvalidHash => {
                        Error::InvalidFormat
                    }
                    _ => Error::Storage,
                });
            }
            Err(ObservationPreparationFailure {
                error: ObservationPreparationError::Proposed(error),
                ..
            }) => {
                if matches!(
                    error,
                    StorageError::ChecksumMismatch | StorageError::InvalidHash
                ) {
                    self.ready = false;
                }
                return Err(error.into());
            }
            Err(ObservationPreparationFailure {
                error: ObservationPreparationError::Source(error),
                ..
            }) => return Err(error.into()),
        };
        #[cfg(test)]
        let published =
            self.files
                .publish_observation_with_hook(&candidate.path, prepared, |stage| {
                    if let Some(hook) = &mut self.hook {
                        hook(crate::Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                    }
                    Ok(())
                })?;
        #[cfg(not(test))]
        let published = self.files.publish_observation(&candidate.path, prepared)?;
        if published.stamp().hash == candidate.before.content_hash
            && published.stamp().size == candidate.before.size
        {
            return Ok(None);
        }

        self.verify_mutation_root()?;
        if let Some((parent_id, identity)) = candidate.watch_parent {
            self.verify_watched_parent(&candidate.before, parent_id, identity)
                .await?;
        }
        let mut proposed = candidate.before.clone();
        proposed.content_rev = revision();
        proposed.content_hash = published.stamp().hash.clone();
        proposed.size = published.stamp().size;
        proposed.last_change_seq = self.state.next_change_seq().await?;
        self.ready = false;
        #[cfg(test)]
        self.state
            .finish_external_file(&candidate.before, &proposed, &mut self.hook)
            .await?;
        #[cfg(not(test))]
        self.state
            .finish_external_file(&candidate.before, &proposed)
            .await?;
        self.ready = true;
        Ok(Some(proposed))
    }
}

pub(crate) struct ExternalCreateProbe {
    epoch: WorkspaceEpoch,
    parent: crate::NodeSnapshot,
    name: String,
    path: String,
    reader: ObservationReader,
    generation: Arc<()>,
    watch_parent: Option<FolderIdentity>,
}

pub(crate) struct ExternalCreateCandidate {
    epoch: WorkspaceEpoch,
    parent: crate::NodeSnapshot,
    name: String,
    path: String,
    observed_hash: String,
    prepared: Result<PreparedObservation, ObservationPreparationFailure>,
    generation: Arc<()>,
    watch_parent: Option<FolderIdentity>,
}

pub(crate) struct ExternalFolderCandidate {
    epoch: WorkspaceEpoch,
    parent: crate::NodeSnapshot,
    name: String,
    path: String,
    generation: Arc<()>,
    watch_parent: FolderIdentity,
    child: FolderIdentity,
}

impl ExternalCreateProbe {
    pub(crate) fn observe_folder(self) -> Result<ExternalFolderCandidate, Error> {
        let watch_parent = self.watch_parent.ok_or(Error::ExternalChange)?;
        let child = self
            .reader
            .observe_watch_child_folder(&self.path, watch_parent)?;
        Ok(ExternalFolderCandidate {
            epoch: self.epoch,
            parent: self.parent,
            name: self.name,
            path: self.path,
            generation: self.generation,
            watch_parent,
            child,
        })
    }

    pub(crate) fn observe(self) -> Result<ExternalCreateCandidate, Error> {
        self.observe_inner(&mut |_| Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn observe_with_storage_hook(
        self,
        mut hook: impl FnMut(StorageStage) -> Result<(), Error>,
    ) -> Result<ExternalCreateCandidate, Error> {
        self.observe_inner(&mut hook)
    }

    fn observe_inner(
        self,
        storage_hook: &mut impl FnMut(StorageStage) -> Result<(), Error>,
    ) -> Result<ExternalCreateCandidate, Error> {
        let first = self.reader.observe(&self.path)?.stamp;
        std::thread::sleep(Duration::from_millis(50));
        let observed = self.reader.observe(&self.path)?;
        if observed.stamp != first {
            return Err(Error::ExternalChange);
        }
        let observed_hash = observed.stamp.hash.clone();
        #[cfg(test)]
        let prepared = self
            .reader
            .prepare_new_with_hook(&self.path, observed, |stage| {
                storage_hook(stage).map_err(|_| StorageError::Io)
            });
        #[cfg(not(test))]
        let prepared = {
            let _ = storage_hook;
            self.reader.prepare_new(&self.path, observed)
        };
        Ok(ExternalCreateCandidate {
            epoch: self.epoch,
            parent: self.parent,
            name: self.name,
            path: self.path,
            observed_hash,
            prepared,
            generation: self.generation,
            watch_parent: self.watch_parent,
        })
    }
}

impl Workspace {
    pub(crate) async fn prepare_watched_external_folder(
        &mut self,
        parent_id: NodeId,
        name: &str,
        identity: FolderIdentity,
    ) -> Result<Option<ExternalCreateProbe>, Error> {
        self.verify_watched_create_parent(parent_id, identity)
            .await?;
        if self.state.child_exists(parent_id, name).await? {
            return if self.state.known_folder_child(parent_id, name).await? {
                Ok(None)
            } else {
                Err(Error::ExternalChange)
            };
        }
        self.prepare_watched_external_create(parent_id, name, identity)
            .await
            .map(Some)
    }

    pub(crate) async fn commit_external_folder(
        &mut self,
        candidate: ExternalFolderCandidate,
    ) -> Result<crate::NodeSnapshot, Error> {
        if !self.ready() || candidate.epoch != self.epoch() {
            return Err(Error::ExternalChange);
        }
        self.verify_mutation_root()?;
        let key = (
            candidate.parent.node_id,
            notryx_domain::mutation::normalized_name_key(&candidate.name),
        );
        let latest = self
            .external_create_generations
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|generation| Arc::ptr_eq(&generation, &candidate.generation));
        if !latest
            || self.state.node(candidate.parent.node_id).await?.as_ref() != Some(&candidate.parent)
            || self.create_path(&candidate.parent, &candidate.name).await? != candidate.path
            || self
                .state
                .child_exists(candidate.parent.node_id, &candidate.name)
                .await?
        {
            return Err(Error::ExternalChange);
        }
        self.files.verify_watch_child_folder(
            &candidate.path,
            candidate.watch_parent,
            candidate.child,
        )?;
        let node = crate::NodeSnapshot {
            node_id: crate::node_id(),
            parent_id: Some(candidate.parent.node_id),
            name: candidate.name,
            kind: notryx_protocol::NodeKind::Folder,
            content_rev: None,
            meta_rev: notryx_domain::MetaRevision::new(uuid::Uuid::new_v4().to_string())
                .expect("generated UUID"),
            content_hash: None,
            size: None,
            children_generation: 0,
            last_change_seq: self.state.next_change_seq().await?,
            deleted: false,
        };
        self.files.verify_watch_child_folder(
            &candidate.path,
            candidate.watch_parent,
            candidate.child,
        )?;
        self.ready = false;
        self.state
            .finish_external_create(&candidate.parent, &node)
            .await?;
        self.ready = true;
        Ok(node)
    }

    pub(crate) async fn prepare_external_create(
        &mut self,
        parent_id: NodeId,
        name: &str,
    ) -> Result<ExternalCreateProbe, Error> {
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        notryx_domain::mutation::validate_name(name).map_err(|_| Error::InvalidRequest)?;
        self.verify_mutation_root()?;
        let parent = self.state.node(parent_id).await?.ok_or(Error::NotFound)?;
        if parent.deleted || parent.kind != notryx_protocol::NodeKind::Folder {
            return Err(Error::ExternalChange);
        }
        if self.state.child_exists(parent_id, name).await? {
            return Err(Error::ExternalChange);
        }
        let path = self.create_path(&parent, name).await?;
        let reader = self.files.observation_reader()?;
        self.external_create_generations
            .retain(|_, generation| generation.strong_count() > 0);
        let generation = Arc::new(());
        self.external_create_generations.insert(
            (
                parent_id,
                notryx_domain::mutation::normalized_name_key(name),
            ),
            Arc::downgrade(&generation),
        );
        Ok(ExternalCreateProbe {
            epoch: self.epoch(),
            parent,
            name: name.into(),
            path,
            reader,
            generation,
            watch_parent: None,
        })
    }

    pub(crate) async fn prepare_watched_external_create(
        &mut self,
        parent_id: NodeId,
        name: &str,
        identity: FolderIdentity,
    ) -> Result<ExternalCreateProbe, Error> {
        let mut probe = self.prepare_external_create(parent_id, name).await?;
        self.verify_watched_create_parent(parent_id, identity)
            .await?;
        probe.watch_parent = Some(identity);
        Ok(probe)
    }

    async fn verify_watched_create_parent(
        &mut self,
        parent_id: NodeId,
        identity: FolderIdentity,
    ) -> Result<(), Error> {
        let path = self.state.node_path(parent_id).await?;
        if !self
            .files
            .watch_folder_matches(&path, identity)
            .map_err(|_| Error::ExternalChange)?
        {
            return Err(Error::ExternalChange);
        }
        Ok(())
    }

    pub(crate) async fn diagnose_external_create(
        &mut self,
        candidate: &ExternalCreateCandidate,
    ) -> Result<ExternalDiagnostic, Error> {
        if !self.ready {
            return Err(Error::RecoveryRequired);
        }
        if candidate.epoch != self.epoch() {
            return Err(Error::ExternalChange);
        }
        if let Err(ObservationPreparationFailure {
            error:
                ObservationPreparationError::Proposed(
                    error @ (StorageError::ChecksumMismatch | StorageError::InvalidHash),
                ),
            ..
        }) = &candidate.prepared
        {
            let referenced = match self
                .state
                .retained_object_is_referenced(&candidate.observed_hash)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    self.ready = false;
                    return Err(error);
                }
            };
            if referenced {
                self.ready = false;
                return Err((*error).into());
            }
        }
        self.diagnose_unavailable_proposed_object(&candidate.observed_hash, &candidate.prepared)
            .await
    }

    pub(crate) async fn commit_external_create(
        &mut self,
        candidate: ExternalCreateCandidate,
    ) -> Result<crate::NodeSnapshot, Error> {
        if matches!(
            self.diagnose_external_create(&candidate).await?,
            ExternalDiagnostic::VerifyObject(..)
        ) {
            return Err(Error::ExternalChange);
        }
        if !self.ready() {
            return Err(Error::RecoveryRequired);
        }
        self.verify_mutation_root()?;
        let key = (
            candidate.parent.node_id,
            notryx_domain::mutation::normalized_name_key(&candidate.name),
        );
        let latest = self
            .external_create_generations
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|generation| Arc::ptr_eq(&generation, &candidate.generation));
        if !latest
            || self.state.node(candidate.parent.node_id).await?.as_ref() != Some(&candidate.parent)
            || self.create_path(&candidate.parent, &candidate.name).await? != candidate.path
            || self
                .state
                .child_exists(candidate.parent.node_id, &candidate.name)
                .await?
        {
            return Err(Error::ExternalChange);
        }
        if let Some(identity) = candidate.watch_parent {
            self.verify_watched_create_parent(candidate.parent.node_id, identity)
                .await?;
        }
        let prepared = match candidate.prepared {
            Ok(prepared) => prepared,
            Err(ObservationPreparationFailure { error, .. }) => {
                let error = match error {
                    ObservationPreparationError::Proposed(error)
                    | ObservationPreparationError::Source(error)
                    | ObservationPreparationError::Previous(error) => error,
                };
                return Err(error.into());
            }
        };
        #[cfg(test)]
        let published =
            self.files
                .publish_observation_with_hook(&candidate.path, prepared, |stage| {
                    if let Some(hook) = &mut self.hook {
                        hook(crate::Phase::Storage(stage)).map_err(|_| StorageError::Io)?;
                    }
                    Ok(())
                })?;
        #[cfg(not(test))]
        let published = self.files.publish_observation(&candidate.path, prepared)?;
        self.verify_mutation_root()?;
        if let Some(identity) = candidate.watch_parent {
            self.verify_watched_create_parent(candidate.parent.node_id, identity)
                .await?;
        }
        let node = crate::NodeSnapshot {
            node_id: crate::node_id(),
            parent_id: Some(candidate.parent.node_id),
            name: candidate.name,
            kind: notryx_protocol::NodeKind::File,
            content_rev: Some(crate::revision()),
            meta_rev: notryx_domain::MetaRevision::new(uuid::Uuid::new_v4().to_string())
                .expect("generated UUID"),
            content_hash: Some(published.stamp().hash.clone()),
            size: Some(published.stamp().size),
            children_generation: 0,
            last_change_seq: self.state.next_change_seq().await?,
            deleted: false,
        };
        self.ready = false;
        self.state
            .finish_external_create(&candidate.parent, &node)
            .await?;
        self.ready = true;
        Ok(node)
    }
}
