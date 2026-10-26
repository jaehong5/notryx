#![cfg(target_os = "linux")]

use notryx_domain::WorkspaceId;
use rustix::fs::{self, Dir, FlockOperation, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    Io,
    NotFound,
    UnsupportedPlatform,
    UnsafeFile,
    InvalidName,
    InvalidHash,
    TooLarge,
    Changed,
    Locked,
    NotEmpty,
    InvalidMarker,
    ChecksumMismatch,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for StorageError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedFile {
    pub bytes: Vec<u8>,
    pub stamp: FileStamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageStage {
    BeforeObjectFileSync,
    AfterObjectFileSync,
    BeforeObjectPublish,
    AfterObjectPublish,
    BeforeObjectDirectorySync,
    AfterObjectDirectorySync,
    BeforeTemporaryFileSync,
    AfterTemporaryFileSync,
    BeforeTargetRevalidation,
    AfterTargetRevalidation,
    BeforeRename,
    AfterRename,
    BeforeParentDirectorySync,
    AfterParentDirectorySync,
    BeforeRecoveryFileSync,
    AfterRecoveryFileSync,
    BeforeRecoveryDirectorySync,
    AfterRecoveryDirectorySync,
}

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    format_version: u32,
    workspace_id: WorkspaceId,
}

pub struct FileStore {
    notes: File,
    state: File,
    objects: File,
    _notes_lock: File,
    _state_lock: File,
    workspace_id: WorkspaceId,
}

fn errno(error: Errno) -> StorageError {
    match error {
        Errno::NOENT => StorageError::NotFound,
        Errno::NOSYS | Errno::INVAL => StorageError::UnsupportedPlatform,
        Errno::LOOP | Errno::XDEV | Errno::NOTDIR => StorageError::UnsafeFile,
        _ => StorageError::Io,
    }
}

fn io(_: std::io::Error) -> StorageError {
    StorageError::Io
}

fn open_at(
    root: &impl AsFd,
    name: impl AsRef<Path>,
    flags: OFlags,
    mode: Mode,
) -> Result<File, StorageError> {
    fs::openat2(root, name.as_ref(), flags | OFlags::CLOEXEC, mode, RESOLVE)
        .map(File::from)
        .map_err(errno)
}

fn open_root(path: &Path) -> Result<File, StorageError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io)?.join(path)
    };
    let root = File::open("/").map_err(io)?;
    open_at(
        &root,
        absolute
            .strip_prefix("/")
            .map_err(|_| StorageError::UnsafeFile)?,
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    )
}

fn ensure_entries(root: &File, allowed: &[&[u8]]) -> Result<(), StorageError> {
    for entry in Dir::read_from(root).map_err(errno)? {
        let entry = entry.map_err(errno)?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." && !allowed.contains(&name) {
            return Err(StorageError::NotEmpty);
        }
    }
    Ok(())
}

fn checked_regular(file: &File) -> Result<std::fs::Metadata, StorageError> {
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(StorageError::UnsafeFile);
    }
    Ok(metadata)
}

fn open_regular(root: &File, name: &str) -> Result<File, StorageError> {
    let probe = open_at(root, name, OFlags::PATH | OFlags::NOFOLLOW, Mode::empty())?;
    let before = checked_regular(&probe)?;
    let file = open_at(root, name, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())?;
    let after = checked_regular(&file)?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(StorageError::Changed);
    }
    Ok(file)
}

fn create_regular(root: &File, name: &str) -> Result<File, StorageError> {
    open_at(
        root,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        Mode::RUSR | Mode::WUSR,
    )
}

fn lock_file(root: &File, name: &str, create: bool) -> Result<File, StorageError> {
    let file = if create {
        create_regular(root, name)?
    } else {
        open_regular(root, name)?
    };
    fs::flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error == Errno::WOULDBLOCK {
            StorageError::Locked
        } else {
            errno(error)
        }
    })?;
    if create {
        file.sync_all().map_err(io)?;
    }
    Ok(file)
}

fn distinct_roots(notes: &File, state: &File) -> Result<(), StorageError> {
    let notes = notes.metadata().map_err(io)?;
    let state = state.metadata().map_err(io)?;
    if (notes.dev(), notes.ino()) == (state.dev(), state.ino()) {
        return Err(StorageError::UnsafeFile);
    }
    Ok(())
}

fn private_state(state: &File) -> Result<(), StorageError> {
    if state.metadata().map_err(io)?.mode() & 0o777 != 0o700 {
        return Err(StorageError::UnsafeFile);
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), StorageError> {
    notryx_domain::mutation::validate_name(name).map_err(|_| StorageError::InvalidName)
}

fn validate_hash(hash: &str) -> Result<(), StorageError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StorageError::InvalidHash);
    }
    Ok(())
}

fn content_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn metadata_stamp(metadata: &std::fs::Metadata, hash: String) -> FileStamp {
    FileStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        hash,
    }
}

fn read_observed(file: &File) -> Result<ObservedFile, StorageError> {
    let before = checked_regular(file)?;
    if before.len() > MAX_FILE_BYTES as u64 {
        return Err(StorageError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(StorageError::TooLarge);
    }
    let after = checked_regular(file)?;
    if metadata_stamp(&before, String::new()) != metadata_stamp(&after, String::new())
        || after.len() != bytes.len() as u64
    {
        return Err(StorageError::Changed);
    }
    let stamp = metadata_stamp(&after, content_hash(&bytes));
    Ok(ObservedFile { bytes, stamp })
}

struct TemporaryFile<'a> {
    root: &'a File,
    name: String,
    file: File,
    armed: bool,
}

impl<'a> TemporaryFile<'a> {
    fn new(root: &'a File) -> Result<Self, StorageError> {
        let name = format!(".mdw-tmp-{}", uuid::Uuid::new_v4());
        let file = create_regular(root, &name)?;
        Ok(Self {
            root,
            name,
            file,
            armed: true,
        })
    }

    fn validate(&self, expected: &FileStamp) -> Result<(), StorageError> {
        let observed = read_observed(&open_regular(self.root, &self.name)?)?;
        if &observed.stamp != expected {
            return Err(StorageError::Changed);
        }
        Ok(())
    }
}

impl Drop for TemporaryFile<'_> {
    fn drop(&mut self) {
        if self.armed {
            let original = self.file.metadata();
            let current =
                open_regular(self.root, &self.name).and_then(|file| checked_regular(&file));
            if let (Ok(original), Ok(current)) = (original, current)
                && (original.dev(), original.ino()) == (current.dev(), current.ino())
            {
                let _ = fs::unlinkat(self.root, self.name.as_str(), fs::AtFlags::empty());
            }
        }
    }
}

impl FileStore {
    pub fn init(
        notes: impl AsRef<Path>,
        state: impl AsRef<Path>,
        workspace_id: WorkspaceId,
    ) -> Result<Self, StorageError> {
        let notes = open_root(notes.as_ref())?;
        let state = open_root(state.as_ref())?;
        distinct_roots(&notes, &state)?;
        ensure_entries(&notes, &[])?;
        ensure_entries(&state, &[])?;
        let notes_lock = lock_file(&notes, ".mdw-server.lock", true)?;
        let state_lock = lock_file(&state, "server.lock", true)?;
        ensure_entries(&notes, &[b".mdw-server.lock"])?;
        ensure_entries(&state, &[b"server.lock"])?;
        fs::fchmod(&state, Mode::RWXU).map_err(errno)?;
        fs::mkdirat(&state, "objects", Mode::RWXU).map_err(errno)?;
        let object_parent = open_at(
            &state,
            "objects",
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        fs::mkdirat(&object_parent, "sha256", Mode::RWXU).map_err(errno)?;
        let objects = open_at(
            &object_parent,
            "sha256",
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        objects.sync_all().map_err(io)?;
        object_parent.sync_all().map_err(io)?;
        state.sync_all().map_err(io)?;
        let marker = Marker {
            format_version: 1,
            workspace_id,
        };
        let bytes = serde_json::to_vec(&marker).map_err(|_| StorageError::InvalidMarker)?;
        let mut file = create_regular(&notes, ".mdw-workspace.json")?;
        file.write_all(&bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        notes.sync_all().map_err(io)?;
        Ok(Self {
            notes,
            state,
            objects,
            _notes_lock: notes_lock,
            _state_lock: state_lock,
            workspace_id,
        })
    }

    pub fn open(notes: impl AsRef<Path>, state: impl AsRef<Path>) -> Result<Self, StorageError> {
        let notes = open_root(notes.as_ref())?;
        let state = open_root(state.as_ref())?;
        distinct_roots(&notes, &state)?;
        private_state(&state)?;
        let notes_lock = lock_file(&notes, ".mdw-server.lock", false)?;
        let state_lock = lock_file(&state, "server.lock", false)?;
        let file = open_regular(&notes, ".mdw-workspace.json")?;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes).map_err(io)?;
        if bytes.len() > 4096 {
            return Err(StorageError::InvalidMarker);
        }
        let marker: Marker =
            serde_json::from_slice(&bytes).map_err(|_| StorageError::InvalidMarker)?;
        if marker.format_version != 1 {
            return Err(StorageError::InvalidMarker);
        }
        let objects = open_at(
            &state,
            "objects/sha256",
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        let result = Self {
            notes,
            state,
            objects,
            _notes_lock: notes_lock,
            _state_lock: state_lock,
            workspace_id: marker.workspace_id,
        };
        result.validate_state_database()?;
        Ok(result)
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn observe(&self, name: &str) -> Result<ObservedFile, StorageError> {
        validate_name(name)?;
        read_observed(&open_regular(&self.notes, name)?)
    }

    pub fn put_object(&self, bytes: &[u8]) -> Result<String, StorageError> {
        self.put_object_inner(bytes, &mut |_| Ok(()))
    }

    pub fn read_object(&self, hash: &str) -> Result<Vec<u8>, StorageError> {
        self.verified_object(hash).map(|(_, bytes)| bytes)
    }

    pub fn replace(
        &self,
        name: &str,
        expected: &FileStamp,
        object_hash: &str,
    ) -> Result<FileStamp, StorageError> {
        self.replace_inner(name, expected, object_hash, &mut |_| Ok(()))
    }

    pub fn sync_observed(&self, name: &str, expected: &FileStamp) -> Result<(), StorageError> {
        self.sync_observed_inner(name, expected, &mut |_| Ok(()))
    }

    /// Keep this store alive and validate the database before opening this pinned path.
    pub fn state_db_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}/state.db", self.state.as_raw_fd()))
    }

    pub fn validate_state_database(&self) -> Result<(), StorageError> {
        private_state(&self.state)?;
        for name in ["state.db", "state.db-wal", "state.db-shm"] {
            match open_regular(&self.state, name) {
                Ok(_) => {}
                Err(StorageError::NotFound) if name != "state.db" => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn sync_state_dir(&self) -> Result<(), StorageError> {
        self.state.sync_all().map_err(io)
    }

    fn verified_object(&self, hash: &str) -> Result<(File, Vec<u8>), StorageError> {
        validate_hash(hash)?;
        let file = open_regular(&self.objects, hash)?;
        let observed = read_observed(&file)?;
        if observed.stamp.hash != hash {
            return Err(StorageError::ChecksumMismatch);
        }
        Ok((file, observed.bytes))
    }

    fn put_object_inner(
        &self,
        bytes: &[u8],
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<String, StorageError> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(StorageError::TooLarge);
        }
        let hash = content_hash(bytes);
        let published = match self.verified_object(&hash) {
            Ok((file, _)) => file,
            Err(StorageError::NotFound) => {
                let mut temporary = TemporaryFile::new(&self.objects)?;
                temporary.file.write_all(bytes).map_err(io)?;
                fs::fchmod(&temporary.file, Mode::RUSR).map_err(errno)?;
                hook(StorageStage::BeforeObjectFileSync)?;
                temporary.file.sync_all().map_err(io)?;
                let durable = metadata_stamp(&temporary.file.metadata().map_err(io)?, hash.clone());
                hook(StorageStage::AfterObjectFileSync)?;
                hook(StorageStage::BeforeObjectPublish)?;
                temporary.validate(&durable)?;
                match fs::renameat_with(
                    &self.objects,
                    temporary.name.as_str(),
                    &self.objects,
                    hash.as_str(),
                    fs::RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => {
                        temporary.armed = false;
                    }
                    Err(Errno::EXIST) => {}
                    Err(error) => return Err(errno(error)),
                }
                hook(StorageStage::AfterObjectPublish)?;
                self.verified_object(&hash)?.0
            }
            Err(error) => return Err(error),
        };
        hook(StorageStage::BeforeObjectFileSync)?;
        published.sync_all().map_err(io)?;
        hook(StorageStage::AfterObjectFileSync)?;
        hook(StorageStage::BeforeObjectDirectorySync)?;
        self.objects.sync_all().map_err(io)?;
        hook(StorageStage::AfterObjectDirectorySync)?;
        Ok(hash)
    }

    fn replace_inner(
        &self,
        name: &str,
        expected: &FileStamp,
        object_hash: &str,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FileStamp, StorageError> {
        validate_name(name)?;
        let bytes = self.read_object(object_hash)?;
        let target = open_regular(&self.notes, name)?;
        let initial = read_observed(&target)?;
        if &initial.stamp != expected {
            return Err(StorageError::Changed);
        }
        let mut temporary = TemporaryFile::new(&self.notes)?;
        temporary.file.write_all(&bytes).map_err(io)?;
        let permissions = target.metadata().map_err(io)?.mode() & 0o777;
        fs::fchmod(&temporary.file, Mode::from_raw_mode(permissions)).map_err(errno)?;
        hook(StorageStage::BeforeTemporaryFileSync)?;
        temporary.file.sync_all().map_err(io)?;
        let durable = metadata_stamp(
            &temporary.file.metadata().map_err(io)?,
            object_hash.to_owned(),
        );
        hook(StorageStage::AfterTemporaryFileSync)?;
        hook(StorageStage::BeforeTargetRevalidation)?;
        let current = self.observe(name)?;
        if &current.stamp != expected {
            return Err(StorageError::Changed);
        }
        hook(StorageStage::AfterTargetRevalidation)?;
        hook(StorageStage::BeforeRename)?;
        // A non-cooperating writer can still race after these checks.
        temporary.validate(&durable)?;
        fs::renameat(&self.notes, temporary.name.as_str(), &self.notes, name).map_err(errno)?;
        temporary.armed = false;
        hook(StorageStage::AfterRename)?;
        hook(StorageStage::BeforeParentDirectorySync)?;
        self.notes.sync_all().map_err(io)?;
        hook(StorageStage::AfterParentDirectorySync)?;
        let final_observation = self.observe(name)?;
        if final_observation.stamp.hash != object_hash {
            return Err(StorageError::Changed);
        }
        Ok(final_observation.stamp)
    }

    fn sync_observed_inner(
        &self,
        name: &str,
        expected: &FileStamp,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        validate_name(name)?;
        let file = open_regular(&self.notes, name)?;
        let observed = read_observed(&file)?;
        if &observed.stamp != expected {
            return Err(StorageError::Changed);
        }
        hook(StorageStage::BeforeRecoveryFileSync)?;
        file.sync_all().map_err(io)?;
        hook(StorageStage::AfterRecoveryFileSync)?;
        hook(StorageStage::BeforeRecoveryDirectorySync)?;
        self.notes.sync_all().map_err(io)?;
        hook(StorageStage::AfterRecoveryDirectorySync)?;
        if self.observe(name)?.stamp != *expected {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    #[cfg(feature = "test-hooks")]
    pub fn put_object_with_hook(
        &self,
        bytes: &[u8],
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<String, StorageError> {
        self.put_object_inner(bytes, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn replace_with_hook(
        &self,
        name: &str,
        expected: &FileStamp,
        object_hash: &str,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<FileStamp, StorageError> {
        self.replace_inner(name, expected, object_hash, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn sync_observed_with_hook(
        &self,
        name: &str,
        expected: &FileStamp,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.sync_observed_inner(name, expected, &mut hook)
    }
}
