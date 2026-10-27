use super::{
    FileStamp, FileStore, ObservedFile, StorageError, StorageStage, TemporaryFile, copy_observed,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedEntry {
    Absent,
    File(ObservedFile),
    Folder { stamp: FolderStamp, empty: bool },
}

pub(super) struct NoteParent {
    pub directory: File,
    pub name: String,
    path: String,
}

impl NoteParent {
    pub fn revalidate(&self, store: &FileStore) -> Result<(), StorageError> {
        let current = store.open_note_folder(&self.path)?;
        if identity(&current)? != identity(&self.directory)? {
            return Err(StorageError::Changed);
        }
        Ok(())
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

struct TemporaryFolder<'a> {
    parent: &'a File,
    name: String,
    folder: File,
    armed: bool,
}

impl<'a> TemporaryFolder<'a> {
    fn new(parent: &'a File) -> Result<Self, StorageError> {
        let name = format!(".mdw-tmp-{}", uuid::Uuid::new_v4());
        fs::mkdirat(parent, name.as_str(), Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(errno)?;
        let folder = open_at(
            parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        Ok(Self {
            parent,
            name,
            folder,
            armed: true,
        })
    }

    fn validate(&self, expected: &FolderStamp) -> Result<(), StorageError> {
        let current = open_at(
            self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        if folder_stamp(&current)? != *expected || !empty_folder(&current)? {
            return Err(StorageError::Changed);
        }
        Ok(())
    }
}

impl Drop for TemporaryFolder<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let current = open_at(
            self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        );
        if let (Ok(original), Ok(current)) = (identity(&self.folder), current)
            && identity(&current) == Ok(original)
        {
            let _ = fs::unlinkat(self.parent, self.name.as_str(), fs::AtFlags::REMOVEDIR);
        }
    }
}

impl FileStore {
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
        self.create_file_inner(path, hash, &mut |_| Ok(()))
    }

    fn create_file_inner(
        &self,
        path: &str,
        hash: &str,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FileStamp, StorageError> {
        let parent = self.note_parent(path)?;
        parent.ensure_available()?;
        let (object, _) = self.verified_object(hash)?;
        let mut temporary = TemporaryFile::new(&parent.directory)?;
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
        temporary.validate(&durable)?;
        parent.revalidate(self)?;
        parent.ensure_available()?;
        fs::renameat_with(
            &parent.directory,
            temporary.name.as_str(),
            &parent.directory,
            parent.name.as_str(),
            fs::RenameFlags::NOREPLACE,
        )
        .map_err(errno)?;
        temporary.armed = false;
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
        self.create_folder_inner(path, &mut |_| Ok(()))
    }

    fn create_folder_inner(
        &self,
        path: &str,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FolderStamp, StorageError> {
        let parent = self.note_parent(path)?;
        parent.ensure_available()?;
        let mut temporary = TemporaryFolder::new(&parent.directory)?;
        hook(StorageStage::BeforeTemporaryFolderSync)?;
        temporary.folder.sync_all().map_err(io)?;
        let durable = folder_stamp(&temporary.folder)?;
        hook(StorageStage::AfterTemporaryFolderSync)?;
        hook(StorageStage::BeforeTargetRevalidation)?;
        parent.revalidate(self)?;
        parent.ensure_available()?;
        hook(StorageStage::AfterTargetRevalidation)?;
        hook(StorageStage::BeforeRename)?;
        temporary.validate(&durable)?;
        parent.revalidate(self)?;
        parent.ensure_available()?;
        fs::renameat_with(
            &parent.directory,
            temporary.name.as_str(),
            &parent.directory,
            parent.name.as_str(),
            fs::RenameFlags::NOREPLACE,
        )
        .map_err(errno)?;
        temporary.armed = false;
        hook(StorageStage::AfterRename)?;
        hook(StorageStage::BeforeParentDirectorySync)?;
        parent.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterParentDirectorySync)?;
        let ObservedEntry::Folder { stamp, empty: true } = self.observe_entry(path)? else {
            return Err(StorageError::Changed);
        };
        if (stamp.device, stamp.inode) != (durable.device, durable.inode) {
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
        self.sync_absent_inner(path, &mut |_| Ok(()))
    }

    fn sync_absent_inner(
        &self,
        path: &str,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let parent = self.note_parent(path)?;
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
        self.create_file_inner(path, hash, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn create_folder_with_hook(
        &self,
        path: &str,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FolderStamp, StorageError> {
        self.create_folder_inner(path, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn sync_absent_with_hook(
        &self,
        path: &str,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.sync_absent_inner(path, &mut hook)
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
