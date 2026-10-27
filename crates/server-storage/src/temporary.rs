use super::{
    FileStamp, FileStore, MAX_FILE_BYTES, StorageError, StorageStage, content_hash, errno,
    hash_observed, io, metadata_stamp, open_at, open_regular, private_state, validate_hash,
    validate_name,
};
use crate::tree::{NoteParent, identity};
use notryx_domain::WorkspaceId;
use rustix::fs::{self, AtFlags, Dir, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use uuid::{Uuid, Variant};

const RECORD_LIMIT: usize = 4096;

pub(super) struct AnonymousFile {
    pub file: File,
}

impl AnonymousFile {
    pub(super) fn new(parent: &File) -> Result<Self, StorageError> {
        let file = fs::openat2(
            parent,
            ".",
            OFlags::RDWR | OFlags::TMPFILE | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
            super::RESOLVE,
        )
        .map(File::from)
        .map_err(|error| match error {
            Errno::OPNOTSUPP | Errno::ISDIR | Errno::NOSYS | Errno::INVAL => {
                StorageError::UnsupportedPlatform
            }
            other => errno(other),
        })?;
        Ok(Self { file })
    }

    pub(super) fn publish(&self, parent: &File, name: &str) -> Result<(), StorageError> {
        if name.is_empty() || matches!(name, "." | "..") || name.contains('/') {
            return Err(StorageError::InvalidName);
        }
        let metadata = self.file.metadata().map_err(io)?;
        if !metadata.is_file() || metadata.nlink() != 0 {
            return Err(StorageError::UnsafeFile);
        }
        // Linking our live descriptor through procfs works without CAP_DAC_READ_SEARCH.
        fs::linkat(
            fs::CWD,
            format!("/proc/self/fd/{}", self.file.as_raw_fd()),
            parent,
            name,
            AtFlags::SYMLINK_FOLLOW,
        )
        .map_err(errno)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedStamp {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    mode: u32,
    hash: String,
}

impl OwnedStamp {
    fn new(metadata: &std::fs::Metadata, hash: String) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            mode: metadata.mode() & 0o7777,
            hash,
        }
    }

    fn matches(&self, metadata: &std::fs::Metadata, stamp: &FileStamp) -> bool {
        // Link and rename change ctime; content, mtime, mode and inode ownership must remain stable.
        metadata.is_file()
            && metadata.nlink() == 1
            && self == &Self::new(metadata, stamp.hash.clone())
            && (stamp.device, stamp.inode, stamp.size)
                == (metadata.dev(), metadata.ino(), metadata.len())
            && (stamp.modified_seconds, stamp.modified_nanoseconds)
                == (metadata.mtime(), metadata.mtime_nsec())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemporaryRecord {
    version: u32,
    workspace_id: WorkspaceId,
    parent_path: String,
    parent_device: u64,
    parent_inode: u64,
    name: String,
    stamp: OwnedStamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
}

impl TemporaryRecord {
    fn owns_unchanged(&self, file: &File, observed: &FileStamp) -> bool {
        let Some(expected) = &self.metadata else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        self.stamp.matches(&metadata, observed)
            && super::file_metadata::fingerprint(file).as_ref() == Ok(expected)
            && hash_observed(file).as_ref() == Ok(observed)
    }
}

pub(super) struct TrackedTemporary<'a> {
    pub file: File,
    pub name: String,
    pub armed: bool,
    store: &'a FileStore,
    parent: &'a NoteParent,
    directory: File,
    record_name: String,
    record: TemporaryRecord,
    record_stamp: FileStamp,
    finished: bool,
}

impl TrackedTemporary<'_> {
    pub(super) fn validate(&self, expected: &FileStamp) -> Result<(), StorageError> {
        self.parent.revalidate(self.store)?;
        let observed = hash_observed(&open_regular(&self.parent.directory, &self.name)?)?;
        if &observed != expected {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    pub(super) fn finish_after_parent_sync(&mut self) -> Result<(), StorageError> {
        if self.finished {
            return Ok(());
        }
        if self.armed {
            return Err(StorageError::Changed);
        }
        if !self.store.cleanup_temporary_record(
            &self.directory,
            &self.record_name,
            &self.record,
            &self.record_stamp,
            false,
            &mut |_| Ok(()),
        )? {
            return Err(StorageError::Changed);
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for TrackedTemporary<'_> {
    fn drop(&mut self) {
        if self.armed && !self.finished {
            let _ = self.store.cleanup_temporary_record(
                &self.directory,
                &self.record_name,
                &self.record,
                &self.record_stamp,
                true,
                &mut |_| Ok(()),
            );
        }
    }
}

fn anonymous_stamp(file: &File, expected_hash: &str) -> Result<OwnedStamp, StorageError> {
    let before = file.metadata().map_err(io)?;
    if !before.is_file() || before.nlink() != 0 {
        return Err(StorageError::UnsafeFile);
    }
    if before.len() > MAX_FILE_BYTES as u64 {
        return Err(StorageError::TooLarge);
    }
    let mut reader = file;
    reader.rewind().map_err(io)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let count = reader.read(&mut buffer).map_err(io)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > MAX_FILE_BYTES as u64 {
            return Err(StorageError::TooLarge);
        }
        hash.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(io)?;
    if after.nlink() != 0
        || metadata_stamp(&before, String::new()) != metadata_stamp(&after, String::new())
        || size != after.len()
    {
        return Err(StorageError::Changed);
    }
    let hash: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if hash != expected_hash {
        return Err(StorageError::ChecksumMismatch);
    }
    Ok(OwnedStamp::new(&after, hash))
}

fn record_id(name: &str) -> Option<Uuid> {
    let text = name.strip_suffix(".json")?;
    let id = Uuid::parse_str(text).ok()?;
    (id.get_version_num() == 4 && id.get_variant() == Variant::RFC4122 && id.to_string() == text)
        .then_some(id)
}

fn preservation_error(error: StorageError) -> bool {
    matches!(
        error,
        StorageError::NotFound
            | StorageError::UnsafeFile
            | StorageError::Changed
            | StorageError::TooLarge
    )
}

impl FileStore {
    fn temporary_directory(&self, create: bool) -> Result<File, StorageError> {
        if create {
            match fs::mkdirat(&self.state, "temporary", Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(errno(error)),
            }
        }
        let directory = open_at(
            &self.state,
            "temporary",
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        private_state(&directory)?;
        Ok(directory)
    }

    fn revalidate_temporary_directory(&self, original: &File) -> Result<(), StorageError> {
        if identity(&self.temporary_directory(false)?)? != identity(original)? {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    pub(super) fn track_temporary<'a>(
        &'a self,
        parent: &'a NoteParent,
        file: AnonymousFile,
        hash: &str,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<TrackedTemporary<'a>, StorageError> {
        validate_hash(hash)?;
        parent.revalidate(self)?;
        let before = metadata_stamp(&file.file.metadata().map_err(io)?, String::new());
        let stamp = anonymous_stamp(&file.file, hash)?;
        let metadata = super::file_metadata::fingerprint(&file.file)?;
        if metadata_stamp(&file.file.metadata().map_err(io)?, String::new()) != before {
            return Err(StorageError::Changed);
        }
        file.file.sync_all().map_err(io)?;
        hook(StorageStage::AfterAnonymousFileSync)?;
        let directory = self.temporary_directory(true)?;
        let id = Uuid::new_v4();
        let name = format!(".mdw-tmp-{id}");
        let record_name = format!("{id}.json");
        let (parent_device, parent_inode) = identity(&parent.directory)?;
        let record = TemporaryRecord {
            version: 2,
            workspace_id: self.workspace_id,
            parent_path: parent.path.clone(),
            parent_device,
            parent_inode,
            name: name.clone(),
            stamp,
            metadata: Some(metadata),
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| StorageError::InvalidMarker)?;
        if bytes.len() > RECORD_LIMIT {
            return Err(StorageError::InvalidMarker);
        }
        let mut record_file = AnonymousFile::new(&directory)?;
        record_file.file.write_all(&bytes).map_err(io)?;
        fs::fchmod(&record_file.file, Mode::RUSR).map_err(errno)?;
        record_file.file.sync_all().map_err(io)?;
        self.revalidate_temporary_directory(&directory)?;
        record_file.publish(&directory, &record_name)?;
        directory.sync_all().map_err(io)?;
        self.state.sync_all().map_err(io)?;
        self.revalidate_temporary_directory(&directory)?;
        let record_stamp = hash_observed(&open_regular(&directory, &record_name)?)?;
        if record_stamp.hash != content_hash(&bytes) {
            return Err(StorageError::Changed);
        }
        hook(StorageStage::AfterTemporaryRecordSync)?;
        parent.revalidate(self)?;
        file.publish(&parent.directory, &name)?;
        hook(StorageStage::AfterTemporaryLink)?;
        Ok(TrackedTemporary {
            file: file.file,
            name,
            armed: true,
            store: self,
            parent,
            directory,
            record_name,
            record,
            record_stamp,
            finished: false,
        })
    }

    fn read_temporary_record(
        &self,
        directory: &File,
        name: &str,
        id: Uuid,
    ) -> Result<(TemporaryRecord, FileStamp), StorageError> {
        let file = open_regular(directory, name)?;
        let mut bytes = Vec::new();
        (&file)
            .take(RECORD_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() > RECORD_LIMIT {
            return Err(StorageError::InvalidMarker);
        }
        let record: TemporaryRecord =
            serde_json::from_slice(&bytes).map_err(|_| StorageError::InvalidMarker)?;
        if !matches!((record.version, &record.metadata), (1, None) | (2, Some(_)))
            || record.workspace_id != self.workspace_id
            || record.name != format!(".mdw-tmp-{id}")
            || record.parent_path.len() > 1024
            || record.stamp.size > MAX_FILE_BYTES as u64
            || record.stamp.mode & !0o7777 != 0
        {
            return Err(StorageError::InvalidMarker);
        }
        if !record.parent_path.is_empty() {
            for segment in record.parent_path.split('/') {
                validate_name(segment).map_err(|_| StorageError::InvalidMarker)?;
            }
        }
        validate_hash(&record.stamp.hash).map_err(|_| StorageError::InvalidMarker)?;
        if let Some(metadata) = &record.metadata {
            validate_hash(metadata).map_err(|_| StorageError::InvalidMarker)?;
        }
        let stamp = hash_observed(&file)?;
        if stamp.hash != content_hash(&bytes) {
            return Err(StorageError::Changed);
        }
        Ok((record, stamp))
    }

    fn record_parent(&self, record: &TemporaryRecord) -> Result<File, StorageError> {
        let parent = if record.parent_path.is_empty() {
            self.notes.try_clone().map_err(io)?
        } else {
            open_at(
                &self.notes,
                &record.parent_path,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
            )?
        };
        if identity(&parent)? != (record.parent_device, record.parent_inode) {
            return Err(StorageError::Changed);
        }
        Ok(parent)
    }

    fn cleanup_temporary_record(
        &self,
        directory: &File,
        record_name: &str,
        record: &TemporaryRecord,
        record_stamp: &FileStamp,
        remove_owned: bool,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<bool, StorageError> {
        self.revalidate_temporary_directory(directory)?;
        let current_record =
            match open_regular(directory, record_name).and_then(|file| hash_observed(&file)) {
                Ok(stamp) => stamp,
                Err(error) if preservation_error(error) => return Ok(false),
                Err(error) => return Err(error),
            };
        if &current_record != record_stamp {
            return Ok(false);
        }
        let parent = match self.record_parent(record) {
            Ok(parent) => parent,
            Err(error) if preservation_error(error) => return Ok(false),
            Err(error) => return Err(error),
        };
        match open_regular(&parent, &record.name) {
            Ok(file) => {
                if !remove_owned {
                    return Ok(false);
                }
                let observed = match hash_observed(&file) {
                    Ok(stamp) => stamp,
                    Err(error) if preservation_error(error) => return Ok(false),
                    Err(error) => return Err(error),
                };
                if !record.owns_unchanged(&file, &observed) {
                    return Ok(false);
                }
                match self.record_parent(record) {
                    Ok(_) => {}
                    Err(error) if preservation_error(error) => return Ok(false),
                    Err(error) => return Err(error),
                }
                let current = match open_regular(&parent, &record.name).and_then(|file| {
                    let stamp = hash_observed(&file)?;
                    Ok((file, stamp))
                }) {
                    Ok(current) => current,
                    Err(error) if preservation_error(error) => return Ok(false),
                    Err(error) => return Err(error),
                };
                if !record.owns_unchanged(&current.0, &current.1) {
                    return Ok(false);
                }
                fs::unlinkat(&parent, record.name.as_str(), AtFlags::empty()).map_err(errno)?;
                hook(StorageStage::AfterTemporaryCleanupUnlink)?;
            }
            Err(StorageError::NotFound) => {}
            Err(error) if preservation_error(error) => return Ok(false),
            Err(error) => return Err(error),
        }
        if remove_owned {
            hook(StorageStage::BeforeTemporaryCleanupDirectorySync)?;
            // Recovery and Drop must sync even an absent name; only finish_after_parent_sync uses false.
            parent.sync_all().map_err(io)?;
        }
        match self.record_parent(record) {
            Ok(_) => {}
            Err(error) if preservation_error(error) => return Ok(false),
            Err(error) => return Err(error),
        }
        match open_at(
            &parent,
            &record.name,
            OFlags::PATH | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Err(StorageError::NotFound) => {}
            Ok(_) => return Ok(false),
            Err(error) => return Err(error),
        }
        self.revalidate_temporary_directory(directory)?;
        if hash_observed(&open_regular(directory, record_name)?)? != *record_stamp {
            return Ok(false);
        }
        fs::unlinkat(directory, record_name, AtFlags::empty()).map_err(errno)?;
        directory.sync_all().map_err(io)?;
        self.revalidate_temporary_directory(directory)?;
        Ok(true)
    }

    pub fn cleanup_temporary_files(&self) -> Result<(), StorageError> {
        self.cleanup_temporary_files_inner(&mut |_| Ok(()))
    }

    fn cleanup_temporary_files_inner(
        &self,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let directory = match self.temporary_directory(false) {
            Ok(directory) => directory,
            Err(StorageError::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in Dir::read_from(&directory).map_err(errno)? {
            let entry = entry.map_err(errno)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some(id) = record_id(name) else {
                continue;
            };
            let (record, stamp) = self.read_temporary_record(&directory, name, id)?;
            self.cleanup_temporary_record(&directory, name, &record, &stamp, true, hook)?;
        }
        self.revalidate_temporary_directory(&directory)
    }

    #[cfg(feature = "test-hooks")]
    pub fn cleanup_temporary_files_with_hook(
        &self,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.cleanup_temporary_files_inner(&mut hook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn fixture() -> (tempfile::TempDir, FileStore) {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&state).unwrap();
        let store = FileStore::init(
            &notes,
            &state,
            "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
        )
        .unwrap();
        std::fs::create_dir(notes.join("Folder")).unwrap();
        (directory, store)
    }

    fn prepared(parent: &File) -> AnonymousFile {
        let mut file = AnonymousFile::new(parent).unwrap();
        file.file.write_all(b"owned bytes").unwrap();
        file.file.sync_all().unwrap();
        file
    }

    fn orphan(store: &FileStore) -> String {
        let parent = store.note_parent("Folder/Note.md").unwrap();
        let mut temporary = store
            .track_temporary(
                &parent,
                prepared(&parent.directory),
                &content_hash(b"owned bytes"),
                &mut |_| Ok(()),
            )
            .unwrap();
        let name = temporary.name.clone();
        temporary.armed = false;
        drop(temporary);
        name
    }

    fn records(directory: &std::path::Path) -> usize {
        std::fs::read_dir(directory.join("state/temporary"))
            .unwrap()
            .count()
    }

    fn reopen(directory: &std::path::Path, store: FileStore) -> FileStore {
        drop(store);
        std::fs::File::create(directory.join("state/state.db")).unwrap();
        FileStore::open(directory.join("notes"), directory.join("state")).unwrap()
    }

    #[test]
    fn cleanup_after_restart_preserves_metadata_only_changes() {
        use rustix::fs::{Gid, XattrFlags};
        use std::os::unix::fs::MetadataExt;

        for modification in ["user_xattr", "access_acl", "group"] {
            let (directory, store) = fixture();
            let name = orphan(&store);
            let path = directory.path().join("notes/Folder").join(name);
            let file = std::fs::File::open(&path).unwrap();
            match modification {
                "user_xattr" => {
                    fs::fsetxattr(&file, "user.synthetic", b"changed", XattrFlags::empty()).unwrap()
                }
                "access_acl" => {
                    let mut acl = 2u32.to_le_bytes().to_vec();
                    for (tag, permission, id) in [
                        (1u16, 6u16, u32::MAX),
                        (2, 4, 65_534),
                        (4, 0, u32::MAX),
                        (16, 0, u32::MAX),
                        (32, 0, u32::MAX),
                    ] {
                        acl.extend_from_slice(&tag.to_le_bytes());
                        acl.extend_from_slice(&permission.to_le_bytes());
                        acl.extend_from_slice(&id.to_le_bytes());
                    }
                    fs::fsetxattr(&file, "system.posix_acl_access", &acl, XattrFlags::empty())
                        .unwrap();
                }
                "group" => {
                    let current = file.metadata().unwrap().gid();
                    let status = std::fs::read_to_string("/proc/self/status").unwrap();
                    let alternate = status
                        .lines()
                        .find_map(|line| line.strip_prefix("Groups:"))
                        .unwrap()
                        .split_whitespace()
                        .filter_map(|value| value.parse::<u32>().ok())
                        .find(|value| *value != current);
                    let Some(alternate) = alternate else { continue };
                    fs::fchown(&file, None, Some(Gid::from_raw(alternate))).unwrap();
                }
                _ => unreachable!(),
            }
            let store = reopen(directory.path(), store);
            store.cleanup_temporary_files().unwrap();
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"owned bytes",
                "{modification}"
            );
            assert_eq!(records(directory.path()), 1, "{modification}");
        }
    }

    #[test]
    fn legacy_records_preserve_existing_files_but_clean_absent_files() {
        for present in [true, false] {
            let (directory, store) = fixture();
            let name = orphan(&store);
            let path = directory.path().join("notes/Folder").join(&name);
            let second = directory.path().join("notes/Folder").join(orphan(&store));
            let record_path = directory
                .path()
                .join("state/temporary")
                .join(format!("{}.json", name.strip_prefix(".mdw-tmp-").unwrap()));
            let mut record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
            record["version"] = 1.into();
            record.as_object_mut().unwrap().remove("metadata");
            std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
            std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o400)).unwrap();
            if !present {
                std::fs::remove_file(&path).unwrap();
            }
            let store = reopen(directory.path(), store);
            store.cleanup_temporary_files().unwrap();
            assert_eq!(path.exists(), present);
            assert!(!second.exists());
            assert_eq!(records(directory.path()), usize::from(present));
        }
    }

    #[test]
    fn anonymous_files_leave_no_scratch_name_and_publish_without_replacing() {
        let (_directory, store) = fixture();
        let parent = store.note_parent("Folder/Note.md").unwrap();
        drop(prepared(&parent.directory));
        assert!(store.list_children("Folder").unwrap().is_empty());
        let first = prepared(&parent.directory);
        first.publish(&parent.directory, "Note.md").unwrap();
        let second = prepared(&parent.directory);
        assert_eq!(
            second.publish(&parent.directory, "Note.md"),
            Err(StorageError::AlreadyExists)
        );
        assert_eq!(
            store.observe("Folder/Note.md").unwrap().bytes,
            b"owned bytes"
        );
    }

    #[test]
    fn registered_orphans_are_removed_without_touching_legacy_or_unknown_entries() {
        let (directory, store) = fixture();
        let name = orphan(&store);
        let folder = directory.path().join("notes/Folder");
        std::fs::write(folder.join(".mdw-tmp-legacy"), b"legacy").unwrap();
        std::fs::write(directory.path().join("state/temporary/unknown"), b"retain").unwrap();
        store.cleanup_temporary_files().unwrap();
        assert!(!folder.join(name).exists());
        assert_eq!(
            std::fs::read(folder.join(".mdw-tmp-legacy")).unwrap(),
            b"legacy"
        );
        assert_eq!(records(directory.path()), 1);
    }

    #[test]
    fn a_record_without_a_link_still_requires_directory_durability_before_removal() {
        let (directory, store) = fixture();
        let parent = store.note_parent("Folder/Note.md").unwrap();
        let result = store.track_temporary(
            &parent,
            prepared(&parent.directory),
            &content_hash(b"owned bytes"),
            &mut |stage| {
                if stage == StorageStage::AfterTemporaryRecordSync {
                    Err(StorageError::Io)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(StorageError::Io)));
        assert_eq!(records(directory.path()), 1);
        assert_eq!(
            std::fs::read_dir(directory.path().join("notes/Folder"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            store.cleanup_temporary_files_inner(&mut |stage| {
                if stage == StorageStage::BeforeTemporaryCleanupDirectorySync {
                    Err(StorageError::Io)
                } else {
                    Ok(())
                }
            }),
            Err(StorageError::Io)
        );
        assert_eq!(records(directory.path()), 1);
        store.cleanup_temporary_files().unwrap();
        assert_eq!(records(directory.path()), 0);
    }

    #[test]
    fn cleanup_failure_after_unlink_retains_the_record_until_directory_sync_succeeds() {
        let (directory, store) = fixture();
        let name = orphan(&store);
        assert_eq!(
            store.cleanup_temporary_files_inner(&mut |stage| {
                if stage == StorageStage::BeforeTemporaryCleanupDirectorySync {
                    Err(StorageError::Io)
                } else {
                    Ok(())
                }
            }),
            Err(StorageError::Io)
        );
        assert!(!directory.path().join("notes/Folder").join(name).exists());
        assert_eq!(records(directory.path()), 1);
        store.cleanup_temporary_files().unwrap();
        assert_eq!(records(directory.path()), 0);
    }

    #[test]
    fn cleanup_preserves_modified_replaced_linked_or_permission_changed_files() {
        for modification in ["content", "replacement", "hardlink", "symlink", "mode"] {
            let (directory, store) = fixture();
            let name = orphan(&store);
            let path = directory.path().join("notes/Folder").join(name);
            let preserved = directory.path().join("preserved");
            match modification {
                "content" => std::fs::write(&path, b"external bytes").unwrap(),
                "replacement" => {
                    std::fs::rename(&path, &preserved).unwrap();
                    std::fs::write(&path, b"external bytes").unwrap();
                }
                "hardlink" => std::fs::hard_link(&path, &preserved).unwrap(),
                "symlink" => {
                    std::fs::rename(&path, &preserved).unwrap();
                    symlink(&preserved, &path).unwrap();
                }
                "mode" => {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap()
                }
                _ => unreachable!(),
            }
            let expected = std::fs::read(&path).unwrap();
            store.cleanup_temporary_files().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected, "{modification}");
            assert_eq!(records(directory.path()), 1, "{modification}");
        }
    }

    #[test]
    fn replaced_parent_preserves_both_directory_trees_and_the_record() {
        let (directory, store) = fixture();
        let name = orphan(&store);
        let notes = directory.path().join("notes");
        std::fs::rename(notes.join("Folder"), notes.join("Moved")).unwrap();
        std::fs::create_dir(notes.join("Folder")).unwrap();
        std::fs::write(notes.join("Folder").join(&name), b"replacement parent").unwrap();
        store.cleanup_temporary_files().unwrap();
        assert_eq!(
            std::fs::read(notes.join("Folder").join(&name)).unwrap(),
            b"replacement parent"
        );
        assert_eq!(
            std::fs::read(notes.join("Moved").join(&name)).unwrap(),
            b"owned bytes"
        );
        assert_eq!(records(directory.path()), 1);
    }

    #[test]
    fn published_files_keep_the_record_until_explicit_finish_or_startup_cleanup() {
        for explicit_finish in [false, true] {
            let (directory, store) = fixture();
            let parent = store.note_parent("Folder/Note.md").unwrap();
            let mut temporary = store
                .track_temporary(
                    &parent,
                    prepared(&parent.directory),
                    &content_hash(b"owned bytes"),
                    &mut |_| Ok(()),
                )
                .unwrap();
            fs::renameat(
                &parent.directory,
                &temporary.name,
                &parent.directory,
                "Note.md",
            )
            .unwrap();
            temporary.armed = false;
            parent.directory.sync_all().unwrap();
            if explicit_finish {
                temporary.finish_after_parent_sync().unwrap();
            }
            drop(temporary);
            assert_eq!(records(directory.path()), usize::from(!explicit_finish));
            store.cleanup_temporary_files().unwrap();
            assert_eq!(records(directory.path()), 0);
            assert_eq!(
                store.observe("Folder/Note.md").unwrap().bytes,
                b"owned bytes"
            );
        }
    }

    #[test]
    fn malformed_registered_metadata_is_preserved_and_blocks_cleanup() {
        let (directory, store) = fixture();
        let name = orphan(&store);
        let record = std::fs::read_dir(directory.path().join("state/temporary"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&record, b"not a record").unwrap();
        assert_eq!(
            store.cleanup_temporary_files(),
            Err(StorageError::InvalidMarker)
        );
        assert_eq!(records(directory.path()), 1);
        assert!(directory.path().join("notes/Folder").join(name).exists());
    }
}
