use super::{
    AnonymousFile, FileStamp, FileStore, ObservedFile, StorageError, StorageStage, copy_observed,
    errno, io, metadata_stamp, open_at, open_regular, read_observed, validate_name,
};
use rustix::fs::{self, Dir, Mode, OFlags};
use std::fs::File;
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FolderStamp {
    pub device: u64,
    pub inode: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FolderIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchUnsupportedReason {
    Symlink,
    HardLink,
    SpecialFile,
    InvalidName,
}

pub enum WatchChild<'a> {
    Candidate {
        name: &'a str,
        is_directory: bool,
    },
    Unsupported {
        name: &'a [u8],
        reason: WatchUnsupportedReason,
    },
}

pub struct WatchChildrenScan {
    pub has_unsupported: bool,
    pub coverage_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedEntry {
    Absent,
    File(ObservedFile),
    Folder { stamp: FolderStamp, empty: bool },
}

pub(super) struct NoteParent {
    pub directory: File,
    pub name: String,
    pub(super) path: String,
}

impl NoteParent {
    pub(super) fn verify_identity(
        &self,
        expected: Option<FolderIdentity>,
    ) -> Result<(), StorageError> {
        if let Some(expected) = expected
            && folder_identity(&self.directory)? != expected
        {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    pub fn revalidate(&self, store: &FileStore) -> Result<(), StorageError> {
        let current = store.open_note_folder(&self.path)?;
        if identity(&current)? != identity(&self.directory)? {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    pub(super) fn unique_name_stamp(&self, store: &FileStore) -> Result<FolderStamp, StorageError> {
        let before = folder_stamp(&self.directory)?;
        let expected = notryx_domain::mutation::normalized_name_key(&self.name);
        let mut count = 0;
        for entry in Dir::read_from(&self.directory).map_err(errno)? {
            let entry = entry.map_err(errno)?;
            if let Ok(name) = entry.file_name().to_str()
                && notryx_domain::mutation::normalized_name_key(name) == expected
            {
                count += 1;
            }
        }
        if count != 1 {
            return Err(StorageError::Changed);
        }
        if folder_stamp(&self.directory)? != before {
            return Err(StorageError::Changed);
        }
        self.revalidate(store)?;
        Ok(before)
    }

    fn ensure_available(&self) -> Result<(), StorageError> {
        let expected = notryx_domain::mutation::normalized_name_key(&self.name);
        for entry in Dir::read_from(&self.directory).map_err(errno)? {
            let entry = entry.map_err(errno)?;
            if let Ok(name) = entry.file_name().to_str()
                && notryx_domain::mutation::normalized_name_key(name) == expected
            {
                return Err(StorageError::AlreadyExists);
            }
        }
        Ok(())
    }

    pub(super) fn absent(&self) -> Result<bool, StorageError> {
        match open_at(
            &self.directory,
            &self.name,
            OFlags::PATH | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(_) => Ok(false),
            Err(StorageError::NotFound) => Ok(true),
            Err(error) => Err(error),
        }
    }
}

pub(super) fn identity(file: &File) -> Result<(u64, u64), StorageError> {
    let metadata = file.metadata().map_err(io)?;
    Ok((metadata.dev(), metadata.ino()))
}

pub(super) fn folder_stamp(file: &File) -> Result<FolderStamp, StorageError> {
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_dir() {
        return Err(StorageError::UnsafeFile);
    }
    Ok(FolderStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

fn empty_folder(file: &File) -> Result<bool, StorageError> {
    for entry in Dir::read_from(file).map_err(errno)? {
        let entry = entry.map_err(errno)?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            return Ok(false);
        }
    }
    Ok(true)
}

fn child_names(directory: &File) -> Result<Vec<String>, StorageError> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory).map_err(errno)? {
        let entry = entry.map_err(errno)?;
        if matches!(entry.file_name().to_bytes(), b"." | b"..") {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| StorageError::InvalidName)?;
        let key = notryx_domain::mutation::normalized_name_key(name);
        if matches!(key.as_str(), ".git" | ".obsidian" | "node_modules") || key.starts_with(".mdw-")
        {
            continue;
        }
        validate_name(name)?;
        names.push(name.to_owned());
    }
    names.sort_unstable();
    Ok(names)
}

fn validate_watch_folder_path(path: &str) -> Result<(), StorageError> {
    if path.len() > 1024 {
        return Err(StorageError::InvalidName);
    }
    if !path.is_empty() {
        for segment in path.split('/') {
            let key = notryx_domain::mutation::normalized_name_key(segment);
            if matches!(key.as_str(), ".git" | ".obsidian" | "node_modules")
                || key.starts_with(".mdw-")
            {
                return Err(StorageError::InvalidName);
            }
            validate_name(segment)?;
        }
    }
    Ok(())
}

fn folder_identity(file: &File) -> Result<FolderIdentity, StorageError> {
    let stamp = folder_stamp(file)?;
    Ok(FolderIdentity {
        device: stamp.device,
        inode: stamp.inode,
    })
}

impl FileStore {
    pub fn verify_watch_child_folder(
        &self,
        path: &str,
        expected_parent: FolderIdentity,
        expected_child: FolderIdentity,
    ) -> Result<(), StorageError> {
        self.verify_workspace_marker()?;
        let parent = self.note_parent(path)?;
        parent.verify_identity(Some(expected_parent))?;
        parent.unique_name_stamp(self)?;
        let (_, child) = self.open_watch_folder(path)?;
        if child != expected_child {
            return Err(StorageError::Changed);
        }
        parent.revalidate(self)?;
        self.verify_workspace_marker()?;
        Ok(())
    }

    pub fn open_watch_folder(&self, path: &str) -> Result<(File, FolderIdentity), StorageError> {
        validate_watch_folder_path(path)?;
        self.verify_workspace_marker()?;
        let folder = self.open_note_folder(path)?;
        let expected = folder_identity(&folder)?;
        if !self.watch_folder_matches(path, expected)? {
            return Err(StorageError::Changed);
        }
        Ok((folder, expected))
    }

    pub fn watch_folder_matches(
        &self,
        path: &str,
        expected: FolderIdentity,
    ) -> Result<bool, StorageError> {
        validate_watch_folder_path(path)?;
        self.verify_workspace_marker()?;
        let current = self.open_note_folder(path)?;
        let matches = folder_identity(&current)? == expected;
        self.verify_workspace_marker()?;
        Ok(matches)
    }

    pub fn list_children(&self, path: &str) -> Result<Vec<String>, StorageError> {
        self.list_children_inner(path, &mut |_| Ok(()))
    }

    fn list_children_inner(
        &self,
        path: &str,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<Vec<String>, StorageError> {
        if path.len() > 1024 {
            return Err(StorageError::InvalidName);
        }
        if !path.is_empty() {
            for segment in path.split('/') {
                validate_name(segment)?;
            }
        }
        let directory = self.open_note_folder(path)?;
        let before = folder_stamp(&directory)?;
        let names = child_names(&directory)?;
        hook(StorageStage::AfterDirectoryRead)?;
        // Entry edits can leave directory timestamps unchanged, so compare names as well.
        if folder_stamp(&directory)? != before
            || child_names(&directory)? != names
            || folder_stamp(&self.open_note_folder(path)?)? != before
        {
            return Err(StorageError::Changed);
        }
        Ok(names)
    }

    fn open_note_folder(&self, path: &str) -> Result<File, StorageError> {
        if path.is_empty() {
            self.notes.try_clone().map_err(io)
        } else {
            open_at(
                &self.notes,
                path,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
            )
        }
    }

    pub(super) fn note_parent(&self, path: &str) -> Result<NoteParent, StorageError> {
        if path.len() > 1024 {
            return Err(StorageError::InvalidName);
        }
        for segment in path.split('/') {
            validate_name(segment)?;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        Ok(NoteParent {
            directory: self.open_note_folder(parent)?,
            name: name.to_owned(),
            path: parent.to_owned(),
        })
    }

    pub fn ensure_available(&self, path: &str) -> Result<(), StorageError> {
        let parent = self.note_parent(path)?;
        parent.ensure_available()?;
        parent.revalidate(self)
    }

    pub fn create_file(&self, path: &str, hash: &str) -> Result<FileStamp, StorageError> {
        self.create_file_inner(path, hash, None, &mut |_| Ok(()))
    }

    pub fn create_file_guarded(
        &self,
        path: &str,
        hash: &str,
        parent: FolderIdentity,
    ) -> Result<FileStamp, StorageError> {
        self.create_file_inner(path, hash, Some(parent), &mut |_| Ok(()))
    }

    fn create_file_inner(
        &self,
        path: &str,
        hash: &str,
        expected_parent: Option<FolderIdentity>,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FileStamp, StorageError> {
        let parent = self.note_parent(path)?;
        parent.verify_identity(expected_parent)?;
        parent.ensure_available()?;
        let (object, _) = self.verified_object(hash)?;
        let mut temporary = AnonymousFile::new(&parent.directory)?;
        if copy_observed(&object, &mut temporary.file)?.hash != hash {
            return Err(StorageError::ChecksumMismatch);
        }
        hook(StorageStage::BeforeTemporaryFileSync)?;
        temporary.file.sync_all().map_err(io)?;
        let durable = metadata_stamp(&temporary.file.metadata().map_err(io)?, hash.into());
        hook(StorageStage::AfterTemporaryFileSync)?;
        hook(StorageStage::BeforeTargetRevalidation)?;
        parent.revalidate(self)?;
        parent.ensure_available()?;
        hook(StorageStage::AfterTargetRevalidation)?;
        hook(StorageStage::BeforeRename)?;
        parent.revalidate(self)?;
        parent.ensure_available()?;
        temporary.publish(&parent.directory, &parent.name)?;
        hook(StorageStage::AfterRename)?;
        hook(StorageStage::BeforeParentDirectorySync)?;
        parent.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterParentDirectorySync)?;
        let current = self.observe_stamp(path)?;
        if current.hash != hash
            || (current.device, current.inode) != (durable.device, durable.inode)
        {
            return Err(StorageError::Changed);
        }
        Ok(current)
    }

    pub fn create_folder(&self, path: &str) -> Result<FolderStamp, StorageError> {
        self.create_folder_inner(path, None, &mut |_| Ok(()))
    }

    pub fn create_folder_guarded(
        &self,
        path: &str,
        parent: FolderIdentity,
    ) -> Result<FolderStamp, StorageError> {
        self.create_folder_inner(path, Some(parent), &mut |_| Ok(()))
    }

    fn create_folder_inner(
        &self,
        path: &str,
        expected_parent: Option<FolderIdentity>,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FolderStamp, StorageError> {
        let parent = self.note_parent(path)?;
        parent.verify_identity(expected_parent)?;
        parent.ensure_available()?;
        hook(StorageStage::BeforeTargetRevalidation)?;
        parent.revalidate(self)?;
        parent.ensure_available()?;
        hook(StorageStage::AfterTargetRevalidation)?;
        hook(StorageStage::BeforeRename)?;
        parent.revalidate(self)?;
        parent.ensure_available()?;
        fs::mkdirat(&parent.directory, &parent.name, Mode::RWXU).map_err(errno)?;
        let folder = open_at(
            &parent.directory,
            &parent.name,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        let durable = folder_stamp(&folder)?;
        hook(StorageStage::AfterRename)?;
        hook(StorageStage::BeforeCreatedFolderSync)?;
        folder.sync_all().map_err(io)?;
        hook(StorageStage::AfterCreatedFolderSync)?;
        hook(StorageStage::BeforeParentDirectorySync)?;
        parent.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterParentDirectorySync)?;
        let ObservedEntry::Folder { stamp, empty: true } = self.observe_entry(path)? else {
            return Err(StorageError::Changed);
        };
        if stamp != durable {
            return Err(StorageError::Changed);
        }
        Ok(stamp)
    }

    pub fn observe_entry(&self, path: &str) -> Result<ObservedEntry, StorageError> {
        let parent = self.note_parent(path)?;
        let probe = match open_at(
            &parent.directory,
            &parent.name,
            OFlags::PATH | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(file) => file,
            Err(StorageError::NotFound) => {
                parent.revalidate(self)?;
                return Ok(ObservedEntry::Absent);
            }
            Err(error) => return Err(error),
        };
        let metadata = probe.metadata().map_err(io)?;
        let result = if metadata.is_file() {
            let observed = read_observed(&open_regular(&parent.directory, &parent.name)?)?;
            if (observed.stamp.device, observed.stamp.inode) != (metadata.dev(), metadata.ino()) {
                return Err(StorageError::Changed);
            }
            ObservedEntry::File(observed)
        } else if metadata.is_dir() {
            let folder = open_at(
                &parent.directory,
                &parent.name,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
            )?;
            let stamp = folder_stamp(&folder)?;
            let empty = empty_folder(&folder)?;
            if (stamp.device, stamp.inode) != (metadata.dev(), metadata.ino())
                || folder_stamp(&folder)? != stamp
            {
                return Err(StorageError::Changed);
            }
            ObservedEntry::Folder { stamp, empty }
        } else {
            return Err(StorageError::UnsafeFile);
        };
        parent.revalidate(self)?;
        Ok(result)
    }

    pub fn sync_absent(&self, path: &str) -> Result<(), StorageError> {
        self.sync_absent_inner(path, None, &mut |_| Ok(()))
    }

    pub fn verify_absent_guarded(
        &self,
        path: &str,
        expected: FolderIdentity,
    ) -> Result<(), StorageError> {
        self.verify_workspace_marker()?;
        let parent = self.note_parent(path)?;
        parent.verify_identity(Some(expected))?;
        if !parent.absent()? {
            return Err(StorageError::Changed);
        }
        parent.revalidate(self)?;
        if !parent.absent()? {
            return Err(StorageError::Changed);
        }
        self.verify_workspace_marker()?;
        Ok(())
    }

    fn sync_absent_inner(
        &self,
        path: &str,
        expected: Option<FolderIdentity>,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let parent = self.note_parent(path)?;
        parent.verify_identity(expected)?;
        if !parent.absent()? {
            return Err(StorageError::Changed);
        }
        hook(StorageStage::BeforeRecoveryDirectorySync)?;
        parent.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterRecoveryDirectorySync)?;
        parent.revalidate(self)?;
        if !parent.absent()? {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    pub fn sync_folder(&self, path: &str, expected: &FolderStamp) -> Result<(), StorageError> {
        self.sync_folder_inner(path, expected, &mut |_| Ok(()))
    }

    fn sync_folder_inner(
        &self,
        path: &str,
        expected: &FolderStamp,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let parent = self.note_parent(path)?;
        let folder = open_at(
            &parent.directory,
            &parent.name,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        if folder_stamp(&folder)? != *expected {
            return Err(StorageError::Changed);
        }
        hook(StorageStage::BeforeRecoveryFolderSync)?;
        folder.sync_all().map_err(io)?;
        hook(StorageStage::AfterRecoveryFolderSync)?;
        hook(StorageStage::BeforeRecoveryDirectorySync)?;
        parent.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterRecoveryDirectorySync)?;
        parent.revalidate(self)?;
        match self.observe_entry(path)? {
            ObservedEntry::Folder { stamp, .. } if stamp == *expected => Ok(()),
            _ => Err(StorageError::Changed),
        }
    }

    #[cfg(feature = "test-hooks")]
    pub fn list_children_with_hook(
        &self,
        path: &str,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<Vec<String>, StorageError> {
        self.list_children_inner(path, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn create_file_with_hook(
        &self,
        path: &str,
        hash: &str,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FileStamp, StorageError> {
        self.create_file_inner(path, hash, None, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn create_file_guarded_with_hook(
        &self,
        path: &str,
        hash: &str,
        parent: FolderIdentity,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FileStamp, StorageError> {
        self.create_file_inner(path, hash, Some(parent), &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn create_folder_with_hook(
        &self,
        path: &str,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FolderStamp, StorageError> {
        self.create_folder_inner(path, None, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn create_folder_guarded_with_hook(
        &self,
        path: &str,
        parent: FolderIdentity,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FolderStamp, StorageError> {
        self.create_folder_inner(path, Some(parent), &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn sync_absent_with_hook(
        &self,
        path: &str,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.sync_absent_inner(path, None, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn sync_folder_with_hook(
        &self,
        path: &str,
        expected: &FolderStamp,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.sync_folder_inner(path, expected, &mut hook)
    }
}

impl super::ObservationReader {
    pub fn observe_watch_child_folder(
        &self,
        path: &str,
        expected_parent: FolderIdentity,
    ) -> Result<FolderIdentity, StorageError> {
        self.files.verify_workspace_marker()?;
        let parent = self.files.note_parent(path)?;
        parent.verify_identity(Some(expected_parent))?;
        parent.unique_name_stamp(&self.files)?;
        let folder = open_at(
            &parent.directory,
            &parent.name,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        let stamp = folder_stamp(&folder)?;
        folder.sync_all().map_err(io)?;
        parent.directory.sync_all().map_err(io)?;
        if folder_stamp(&folder)? != stamp {
            return Err(StorageError::Changed);
        }
        let child = FolderIdentity {
            device: stamp.device,
            inode: stamp.inode,
        };
        self.files
            .verify_watch_child_folder(path, expected_parent, child)?;
        parent.revalidate(&self.files)?;
        Ok(child)
    }

    pub fn sync_absent_guarded(
        &self,
        path: &str,
        expected: FolderIdentity,
    ) -> Result<(), StorageError> {
        self.files.verify_workspace_marker()?;
        self.files
            .sync_absent_inner(path, Some(expected), &mut |_| Ok(()))?;
        self.files.verify_workspace_marker()?;
        Ok(())
    }
    pub fn stream_watch_children(
        &self,
        path: &str,
        expected: FolderIdentity,
        mut visit: impl FnMut(WatchChild<'_>) -> Result<(), StorageError>,
    ) -> Result<WatchChildrenScan, StorageError> {
        let (directory, identity) = self.files.open_watch_folder(path)?;
        if identity != expected {
            return Err(StorageError::Changed);
        }
        let before = folder_stamp(&directory)?;
        let mut has_unsupported = false;
        let mut coverage_complete = true;
        for entry in Dir::read_from(&directory).map_err(errno)? {
            let entry = entry.map_err(errno)?;
            let bytes = entry.file_name().to_bytes();
            if matches!(bytes, b"." | b"..") {
                continue;
            }
            let name = match std::str::from_utf8(bytes) {
                Ok(name) => name,
                Err(_) => {
                    has_unsupported = true;
                    visit(WatchChild::Unsupported {
                        name: bytes,
                        reason: WatchUnsupportedReason::InvalidName,
                    })?;
                    continue;
                }
            };
            let key = notryx_domain::mutation::normalized_name_key(name);
            if matches!(key.as_str(), ".git" | ".obsidian" | "node_modules")
                || key.starts_with(".mdw-")
            {
                continue;
            }
            if validate_name(name).is_err() {
                has_unsupported = true;
                visit(WatchChild::Unsupported {
                    name: bytes,
                    reason: WatchUnsupportedReason::InvalidName,
                })?;
                continue;
            }
            let stat =
                match fs::statat(&directory, entry.file_name(), fs::AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat,
                    Err(_) => {
                        coverage_complete = false;
                        continue;
                    }
                };
            let kind = fs::FileType::from_raw_mode(stat.st_mode);
            let is_directory = kind == fs::FileType::Directory;
            if !is_directory && (kind != fs::FileType::RegularFile || stat.st_nlink != 1) {
                has_unsupported = true;
                let reason = if kind == fs::FileType::Symlink {
                    WatchUnsupportedReason::Symlink
                } else if kind == fs::FileType::RegularFile {
                    WatchUnsupportedReason::HardLink
                } else {
                    WatchUnsupportedReason::SpecialFile
                };
                visit(WatchChild::Unsupported {
                    name: bytes,
                    reason,
                })?;
                continue;
            }
            match visit(WatchChild::Candidate { name, is_directory }) {
                Ok(()) => {}
                Err(StorageError::InvalidName) => {
                    has_unsupported = true;
                    visit(WatchChild::Unsupported {
                        name: bytes,
                        reason: WatchUnsupportedReason::InvalidName,
                    })?;
                }
                Err(error) => return Err(error),
            }
        }
        if folder_stamp(&directory)? != before
            || !self.files.watch_folder_matches(path, expected)?
        {
            return Err(StorageError::Changed);
        }
        Ok(WatchChildrenScan {
            has_unsupported,
            coverage_complete,
        })
    }
}
