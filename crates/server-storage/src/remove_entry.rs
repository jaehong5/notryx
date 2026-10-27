use crate::{EntryStamp, FileStore, ObservedEntry, StorageError, StorageStage, errno, io};
use rustix::fs::{self, AtFlags};
use rustix::io::Errno;

impl FileStore {
    /// The coordinator must persist the before object before admitting a file deletion.
    pub fn remove_entry(&self, path: &str, expected: &EntryStamp) -> Result<(), StorageError> {
        self.remove_entry_inner(path, expected, &mut |_| Ok(()))
    }

    fn validate_removal(&self, path: &str, expected: &EntryStamp) -> Result<(), StorageError> {
        match self.observe_entry(path)? {
            ObservedEntry::Absent => Err(StorageError::NotFound),
            ObservedEntry::File(observed) => match expected {
                EntryStamp::File(expected) if observed.stamp == *expected => Ok(()),
                _ => Err(StorageError::Changed),
            },
            ObservedEntry::Folder { empty: false, .. } => Err(StorageError::NotEmpty),
            ObservedEntry::Folder { stamp, empty: true } => match expected {
                EntryStamp::Folder(expected) if stamp == *expected => Ok(()),
                _ => Err(StorageError::Changed),
            },
        }
    }

    fn remove_entry_inner(
        &self,
        path: &str,
        expected: &EntryStamp,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let parent = self.note_parent(path)?;
        self.validate_removal(path, expected)?;
        hook(StorageStage::BeforeTargetRevalidation)?;
        parent.revalidate(self)?;
        self.validate_removal(path, expected)?;
        hook(StorageStage::AfterTargetRevalidation)?;
        hook(StorageStage::BeforeUnlink)?;
        parent.revalidate(self)?;
        self.validate_removal(path, expected)?;
        let flags = match expected {
            EntryStamp::File(_) => AtFlags::empty(),
            EntryStamp::Folder(_) => AtFlags::REMOVEDIR,
        };
        // REMOVEDIR checks emptiness atomically; no child entry is removed by this operation.
        fs::unlinkat(&parent.directory, parent.name.as_str(), flags).map_err(|error| {
            if flags == AtFlags::REMOVEDIR && error == Errno::EXIST {
                StorageError::NotEmpty
            } else {
                errno(error)
            }
        })?;
        hook(StorageStage::AfterUnlink)?;
        hook(StorageStage::BeforeParentDirectorySync)?;
        parent.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterParentDirectorySync)?;
        parent.revalidate(self)?;
        if !parent.absent()? {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    #[cfg(feature = "test-hooks")]
    pub fn remove_entry_with_hook(
        &self,
        path: &str,
        expected: &EntryStamp,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.remove_entry_inner(path, expected, &mut hook)
    }
}
