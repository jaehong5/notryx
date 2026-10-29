use crate::tree::{NoteParent, folder_stamp, identity};
use crate::{
    FileStamp, FileStore, FolderIdentity, FolderStamp, ObservedEntry, StorageError, StorageStage,
    errno, io, open_at, open_regular, read_observed,
};
use rustix::fs::{self, Dir, Mode, OFlags};
use serde::{Deserialize, Serialize};
use std::fs::File;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryStamp {
    File(FileStamp),
    Folder(FolderStamp),
}

impl EntryStamp {
    pub fn matches_after_move(&self, observed: &Self) -> bool {
        // Rename changes ctime. Identity, content, and mtime must still match the prepared entry.
        match (self, observed) {
            (Self::File(before), Self::File(after)) => {
                before.device == after.device
                    && before.inode == after.inode
                    && before.size == after.size
                    && before.hash == after.hash
                    && before.modified_seconds == after.modified_seconds
                    && before.modified_nanoseconds == after.modified_nanoseconds
            }
            (Self::Folder(before), Self::Folder(after)) => {
                before.device == after.device
                    && before.inode == after.inode
                    && before.modified_seconds == after.modified_seconds
                    && before.modified_nanoseconds == after.modified_nanoseconds
            }
            _ => false,
        }
    }
}

enum EntryHandle {
    File(File),
    Folder(File),
}

impl EntryHandle {
    fn open(parent: &NoteParent, expected: &EntryStamp) -> Result<Self, StorageError> {
        match expected {
            EntryStamp::File(_) => open_regular(&parent.directory, &parent.name).map(Self::File),
            EntryStamp::Folder(_) => open_at(
                &parent.directory,
                &parent.name,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
            )
            .map(Self::Folder),
        }
    }

    fn stamp(&self) -> Result<EntryStamp, StorageError> {
        match self {
            Self::File(file) => {
                read_observed(file).map(|observed| EntryStamp::File(observed.stamp))
            }
            Self::Folder(folder) => folder_stamp(folder).map(EntryStamp::Folder),
        }
    }

    fn sync(&self) -> Result<(), StorageError> {
        match self {
            Self::File(file) | Self::Folder(file) => file.sync_all().map_err(io),
        }
    }
}

fn ensure_destination(source: &NoteParent, destination: &NoteParent) -> Result<(), StorageError> {
    let same_parent = identity(&source.directory)? == identity(&destination.directory)?;
    let key = notryx_domain::mutation::normalized_name_key(&destination.name);
    for entry in Dir::read_from(&destination.directory).map_err(errno)? {
        let entry = entry.map_err(errno)?;
        let Some(name) = entry.file_name().to_str().ok() else {
            continue;
        };
        if same_parent && name == source.name {
            continue;
        }
        if notryx_domain::mutation::normalized_name_key(name) == key {
            return Err(StorageError::AlreadyExists);
        }
    }
    Ok(())
}

fn observed_stamp(entry: ObservedEntry) -> Result<EntryStamp, StorageError> {
    match entry {
        ObservedEntry::File(file) => Ok(EntryStamp::File(file.stamp)),
        ObservedEntry::Folder { stamp, .. } => Ok(EntryStamp::Folder(stamp)),
        ObservedEntry::Absent => Err(StorageError::Changed),
    }
}

impl FileStore {
    pub fn ensure_move_available(
        &self,
        old_path: &str,
        new_path: &str,
    ) -> Result<(), StorageError> {
        if old_path == new_path {
            return Err(StorageError::InvalidName);
        }
        let source = self.note_parent(old_path)?;
        let destination = self.note_parent(new_path)?;
        ensure_destination(&source, &destination)?;
        source.revalidate(self)?;
        destination.revalidate(self)
    }

    pub fn move_entry(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
    ) -> Result<EntryStamp, StorageError> {
        self.move_entry_inner(old_path, new_path, expected, None, &mut |_| Ok(()))
    }

    pub fn move_entry_guarded(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
        parent: FolderIdentity,
    ) -> Result<EntryStamp, StorageError> {
        self.move_entry_inner(old_path, new_path, expected, Some(parent), &mut |_| Ok(()))
    }

    fn move_entry_inner(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
        expected_parent: Option<FolderIdentity>,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<EntryStamp, StorageError> {
        if old_path == new_path {
            return Err(StorageError::InvalidName);
        }
        let source = self.note_parent(old_path)?;
        let destination = self.note_parent(new_path)?;
        destination.verify_identity(expected_parent)?;
        ensure_destination(&source, &destination)?;
        let entry = EntryHandle::open(&source, expected)?;
        if entry.stamp()? != *expected {
            return Err(StorageError::Changed);
        }
        hook(StorageStage::BeforeTargetRevalidation)?;
        source.revalidate(self)?;
        destination.revalidate(self)?;
        ensure_destination(&source, &destination)?;
        if observed_stamp(self.observe_entry(old_path)?)? != *expected {
            return Err(StorageError::Changed);
        }
        hook(StorageStage::AfterTargetRevalidation)?;
        hook(StorageStage::BeforeRename)?;
        source.revalidate(self)?;
        destination.revalidate(self)?;
        ensure_destination(&source, &destination)?;
        if observed_stamp(self.observe_entry(old_path)?)? != *expected {
            return Err(StorageError::Changed);
        }
        // The final pathname checks cannot exclude a non-cooperating external writer's last race.
        fs::renameat_with(
            &source.directory,
            source.name.as_str(),
            &destination.directory,
            destination.name.as_str(),
            fs::RenameFlags::NOREPLACE,
        )
        .map_err(errno)?;
        hook(StorageStage::AfterRename)?;
        hook(StorageStage::BeforeMoveEntrySync)?;
        entry.sync()?;
        hook(StorageStage::AfterMoveEntrySync)?;
        hook(StorageStage::BeforeMoveSourceDirectorySync)?;
        source.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterMoveSourceDirectorySync)?;
        if identity(&source.directory)? != identity(&destination.directory)? {
            hook(StorageStage::BeforeMoveDestinationDirectorySync)?;
            destination.directory.sync_all().map_err(io)?;
            hook(StorageStage::AfterMoveDestinationDirectorySync)?;
        }
        source.revalidate(self)?;
        destination.revalidate(self)?;
        if !source.absent()? {
            return Err(StorageError::Changed);
        }
        let moved = observed_stamp(self.observe_entry(new_path)?)?;
        if !expected.matches_after_move(&moved) || entry.stamp()? != moved {
            return Err(StorageError::Changed);
        }
        Ok(moved)
    }

    pub fn sync_move(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
        applied: bool,
    ) -> Result<(), StorageError> {
        self.sync_move_inner(old_path, new_path, expected, applied, &mut |_| Ok(()))
    }

    fn sync_move_inner(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
        applied: bool,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        if old_path == new_path {
            return Err(StorageError::InvalidName);
        }
        let source = self.note_parent(old_path)?;
        let destination = self.note_parent(new_path)?;
        let (present, absent, present_path) = if applied {
            (&destination, &source, new_path)
        } else {
            (&source, &destination, old_path)
        };
        if !absent.absent()? {
            return Err(StorageError::Changed);
        }
        let entry = EntryHandle::open(present, expected)?;
        let before_sync = entry.stamp()?;
        let matches = if applied {
            expected.matches_after_move(&before_sync)
        } else {
            before_sync == *expected
        };
        if !matches {
            return Err(StorageError::Changed);
        }
        source.revalidate(self)?;
        destination.revalidate(self)?;
        let (before, after) = match entry {
            EntryHandle::File(_) => (
                StorageStage::BeforeRecoveryFileSync,
                StorageStage::AfterRecoveryFileSync,
            ),
            EntryHandle::Folder(_) => (
                StorageStage::BeforeRecoveryFolderSync,
                StorageStage::AfterRecoveryFolderSync,
            ),
        };
        hook(before)?;
        entry.sync()?;
        hook(after)?;
        hook(StorageStage::BeforeRecoverySourceDirectorySync)?;
        source.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterRecoverySourceDirectorySync)?;
        if identity(&source.directory)? != identity(&destination.directory)? {
            hook(StorageStage::BeforeRecoveryDestinationDirectorySync)?;
            destination.directory.sync_all().map_err(io)?;
            hook(StorageStage::AfterRecoveryDestinationDirectorySync)?;
        }
        source.revalidate(self)?;
        destination.revalidate(self)?;
        if !absent.absent()? || observed_stamp(self.observe_entry(present_path)?)? != before_sync {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    #[cfg(feature = "test-hooks")]
    pub fn move_entry_with_hook(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<EntryStamp, StorageError> {
        self.move_entry_inner(old_path, new_path, expected, None, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn move_entry_guarded_with_hook(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
        parent: FolderIdentity,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<EntryStamp, StorageError> {
        self.move_entry_inner(old_path, new_path, expected, Some(parent), &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn sync_move_with_hook(
        &self,
        old_path: &str,
        new_path: &str,
        expected: &EntryStamp,
        applied: bool,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.sync_move_inner(old_path, new_path, expected, applied, &mut hook)
    }
}
