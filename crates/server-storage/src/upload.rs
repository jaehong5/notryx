use super::{
    AnonymousFile, FileStamp, FileStore, MAX_FILE_BYTES, StorageError, StorageStage, StoreLease,
    checked_regular, copy_observed, create_regular, errno, hash_observed, io, metadata_stamp,
    open_at, open_regular, private_state, validate_hash,
};
use rustix::fs::{self, Dir, Mode, OFlags};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Weak};
use uuid::Uuid;

pub struct UploadWriter {
    temporary: OwnedUpload,
    hasher: Sha256,
    size: u64,
    max_bytes: u64,
    error: Option<StorageError>,
    alive: Arc<()>,
}

pub struct SealedUpload {
    temporary: OwnedUpload,
    stamp: FileStamp,
    _alive: Arc<()>,
}

/// Persisted metadata only; dropping it never removes staging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUpload {
    pub id: Uuid,
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingEntry {
    pub id: Uuid,
    pub complete: bool,
    pub stamp: FileStamp,
}

struct OwnedUpload {
    id: Uuid,
    directory: File,
    name: String,
    file: File,
    armed: bool,
    // Keep both store locks until this original inode has been cleaned up.
    lease: Arc<StoreLease>,
}

fn upload_name(id: Uuid, complete: bool) -> String {
    format!("upload-{id}.{}", if complete { "ready" } else { "part" })
}

fn parse_upload_name(name: &str) -> Option<(Uuid, bool)> {
    let (id, suffix) = name.strip_prefix("upload-")?.rsplit_once('.')?;
    let complete = match suffix {
        "ready" => true,
        "part" => false,
        _ => return None,
    };
    let id = Uuid::parse_str(id).ok()?;
    (name == upload_name(id, complete)).then_some((id, complete))
}

impl Drop for OwnedUpload {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let original = self.file.metadata();
        let current =
            open_regular(&self.directory, &self.name).and_then(|file| checked_regular(&file));
        if let (Ok(original), Ok(current)) = (original, current)
            && (original.dev(), original.ino()) == (current.dev(), current.ino())
        {
            let _ = fs::unlinkat(&self.directory, self.name.as_str(), fs::AtFlags::empty());
            let _ = self.directory.sync_all();
        }
    }
}

impl UploadWriter {
    pub fn id(&self) -> Uuid {
        self.temporary.id
    }

    pub fn liveness(&self) -> Weak<()> {
        Arc::downgrade(&self.alive)
    }

    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let next = self.size.checked_add(bytes.len() as u64);
        if next.is_none_or(|size| size > self.max_bytes) {
            self.error = Some(StorageError::TooLarge);
            return Err(StorageError::TooLarge);
        }
        if let Err(error) = self.temporary.file.write_all(bytes).map_err(io) {
            // A partial write must never become a successful truncated upload.
            self.error = Some(error);
            return Err(error);
        }
        self.hasher.update(bytes);
        self.size = next.unwrap();
        Ok(())
    }

    pub fn seal(mut self) -> Result<SealedUpload, StorageError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        fs::fchmod(&self.temporary.file, Mode::RUSR).map_err(errno)?;
        self.temporary.file.sync_all().map_err(io)?;
        let original = checked_regular(&self.temporary.file)?;
        let file = open_regular(&self.temporary.directory, &self.temporary.name)?;
        let stamp = hash_observed(&file)?;
        if (stamp.device, stamp.inode) != (original.dev(), original.ino()) {
            return Err(StorageError::Changed);
        }
        let hash: String = self
            .hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if stamp.size != self.size || stamp.hash != hash {
            return Err(StorageError::ChecksumMismatch);
        }
        self.temporary.file = file;
        Ok(SealedUpload {
            temporary: self.temporary,
            stamp,
            _alive: self.alive,
        })
    }
}

impl SealedUpload {
    pub fn id(&self) -> Uuid {
        self.temporary.id
    }
    pub fn hash(&self) -> &str {
        &self.stamp.hash
    }
    pub fn size(&self) -> u64 {
        self.stamp.size
    }
}

impl FileStore {
    fn open_upload_staging(&self) -> Result<File, StorageError> {
        let directory = open_at(
            &self.state,
            "staging",
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        private_state(&directory)?;
        Ok(directory)
    }

    fn revalidate_upload_staging(&self, original: &File) -> Result<(), StorageError> {
        let current = self.open_upload_staging()?;
        if super::tree::identity(original)? != super::tree::identity(&current)? {
            return Err(StorageError::Changed);
        }
        Ok(())
    }

    pub fn prepare_upload_staging(&self) -> Result<(), StorageError> {
        match fs::mkdirat(&self.state, "staging", Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(errno(error)),
        }
        let directory = self.open_upload_staging()?;
        directory.sync_all().map_err(io)?;
        self.state.sync_all().map_err(io)?;
        self.revalidate_upload_staging(&directory)
    }

    pub fn begin_upload(&self, id: Uuid, max_bytes: u64) -> Result<UploadWriter, StorageError> {
        if max_bytes > MAX_FILE_BYTES as u64 {
            return Err(StorageError::TooLarge);
        }
        let directory = self.open_upload_staging()?;
        let name = upload_name(id, false);
        let file = create_regular(&directory, &name)?;
        let temporary = OwnedUpload {
            id,
            directory,
            name,
            file,
            armed: true,
            lease: Arc::clone(&self.lease),
        };
        self.revalidate_upload_staging(&temporary.directory)?;
        Ok(UploadWriter {
            temporary,
            hasher: Sha256::new(),
            size: 0,
            max_bytes,
            error: None,
            alive: Arc::new(()),
        })
    }

    pub fn commit_upload(&self, sealed: SealedUpload) -> Result<StoredUpload, StorageError> {
        self.commit_upload_inner(sealed, &mut |_| Ok(()))
    }

    fn commit_upload_inner(
        &self,
        mut sealed: SealedUpload,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<StoredUpload, StorageError> {
        if !Arc::ptr_eq(&self.lease, &sealed.temporary.lease) {
            return Err(StorageError::Changed);
        }
        self.revalidate_upload_staging(&sealed.temporary.directory)?;
        hook(StorageStage::BeforeUploadPublish)?;
        if hash_observed(&open_regular(
            &sealed.temporary.directory,
            &sealed.temporary.name,
        )?)? != sealed.stamp
        {
            return Err(StorageError::Changed);
        }
        self.revalidate_upload_staging(&sealed.temporary.directory)?;
        let name = upload_name(sealed.id(), true);
        fs::renameat_with(
            &sealed.temporary.directory,
            sealed.temporary.name.as_str(),
            &sealed.temporary.directory,
            name.as_str(),
            fs::RenameFlags::NOREPLACE,
        )
        .map_err(errno)?;
        sealed.temporary.name = name;
        hook(StorageStage::AfterUploadPublish)?;
        hook(StorageStage::BeforeUploadFileSync)?;
        sealed.temporary.file.sync_all().map_err(io)?;
        hook(StorageStage::AfterUploadFileSync)?;
        hook(StorageStage::BeforeUploadDirectorySync)?;
        sealed.temporary.directory.sync_all().map_err(io)?;
        hook(StorageStage::AfterUploadDirectorySync)?;
        let published = hash_observed(&open_regular(
            &sealed.temporary.directory,
            &sealed.temporary.name,
        )?)?;
        if published.hash != sealed.stamp.hash
            || published.size != sealed.stamp.size
            || (published.device, published.inode) != (sealed.stamp.device, sealed.stamp.inode)
        {
            return Err(StorageError::Changed);
        }
        self.revalidate_upload_staging(&sealed.temporary.directory)?;
        sealed.temporary.armed = false;
        Ok(StoredUpload {
            id: sealed.id(),
            hash: sealed.stamp.hash.clone(),
            size: sealed.stamp.size,
        })
    }

    pub fn validate_upload(&self, stored: &StoredUpload) -> Result<(), StorageError> {
        self.open_stored_upload(stored).map(|_| ())
    }

    fn open_stored_upload(
        &self,
        stored: &StoredUpload,
    ) -> Result<(File, File, FileStamp), StorageError> {
        validate_hash(&stored.hash)?;
        let directory = self.open_upload_staging()?;
        let source = open_regular(&directory, &upload_name(stored.id, true))?;
        let stamp = hash_observed(&source)?;
        if stamp.hash != stored.hash || stamp.size != stored.size {
            return Err(StorageError::ChecksumMismatch);
        }
        self.revalidate_upload_staging(&directory)?;
        Ok((directory, source, stamp))
    }

    pub fn prepare_upload_object(&self, stored: &StoredUpload) -> Result<String, StorageError> {
        self.prepare_upload_object_inner(stored, &mut |_| Ok(()))
    }

    fn prepare_upload_object_inner(
        &self,
        stored: &StoredUpload,
        hook: &mut impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<String, StorageError> {
        let (directory, source, expected) = self.open_stored_upload(stored)?;
        let published = match self.verified_object(&stored.hash) {
            Ok((file, _)) => file,
            Err(StorageError::NotFound) => {
                let mut temporary = AnonymousFile::new(&self.objects)?;
                if copy_observed(&source, &mut temporary.file)? != expected {
                    return Err(StorageError::Changed);
                }
                fs::fchmod(&temporary.file, Mode::RUSR).map_err(errno)?;
                hook(StorageStage::BeforeObjectFileSync)?;
                temporary.file.sync_all().map_err(io)?;
                hook(StorageStage::AfterObjectFileSync)?;
                hook(StorageStage::BeforeObjectPublish)?;
                match temporary.publish(&self.objects, &stored.hash) {
                    Ok(()) | Err(StorageError::AlreadyExists) => {}
                    Err(error) => return Err(error),
                }
                hook(StorageStage::AfterObjectPublish)?;
                self.verified_object(&stored.hash)?.0
            }
            Err(error) => return Err(error),
        };
        hook(StorageStage::BeforeObjectFileSync)?;
        published.sync_all().map_err(io)?;
        hook(StorageStage::AfterObjectFileSync)?;
        hook(StorageStage::BeforeObjectDirectorySync)?;
        self.objects.sync_all().map_err(io)?;
        hook(StorageStage::AfterObjectDirectorySync)?;
        self.revalidate_upload_staging(&directory)?;
        Ok(stored.hash.clone())
    }

    /// The coordinator must exclude uploads referenced by pending intents.
    pub fn remove_upload(&self, stored: &StoredUpload) -> Result<(), StorageError> {
        validate_hash(&stored.hash)?;
        let directory = self.open_upload_staging()?;
        let stamp = match open_regular(&directory, &upload_name(stored.id, true)) {
            Ok(file) => hash_observed(&file)?,
            Err(StorageError::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        if stamp.hash != stored.hash || stamp.size != stored.size {
            return Err(StorageError::ChecksumMismatch);
        }
        self.remove_staged_entry(&StagingEntry {
            id: stored.id,
            complete: true,
            stamp: FileStamp {
                hash: String::new(),
                ..stamp
            },
        })
    }

    pub fn list_upload_staging(&self) -> Result<Vec<StagingEntry>, StorageError> {
        let directory = self.open_upload_staging()?;
        let mut entries = Vec::new();
        for entry in Dir::read_from(&directory).map_err(errno)? {
            let entry = entry.map_err(errno)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some((id, complete)) = parse_upload_name(name) else {
                continue;
            };
            let file = match open_regular(&directory, name) {
                Ok(file) => file,
                Err(StorageError::NotFound) => continue,
                Err(error) => return Err(error),
            };
            let stamp = metadata_stamp(&checked_regular(&file)?, String::new());
            entries.push(StagingEntry {
                id,
                complete,
                stamp,
            });
        }
        self.revalidate_upload_staging(&directory)?;
        entries.sort_unstable_by_key(|entry| (entry.id, entry.complete));
        Ok(entries)
    }

    /// The coordinator must exclude active writers and pending intent references.
    pub fn remove_staged_entry(&self, entry: &StagingEntry) -> Result<(), StorageError> {
        let directory = self.open_upload_staging()?;
        let name = upload_name(entry.id, entry.complete);
        let file = match open_regular(&directory, &name) {
            Ok(file) => file,
            Err(StorageError::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata_stamp(&checked_regular(&file)?, String::new()) != entry.stamp {
            return Err(StorageError::Changed);
        }
        self.revalidate_upload_staging(&directory)?;
        if metadata_stamp(
            &checked_regular(&open_regular(&directory, &name)?)?,
            String::new(),
        ) != entry.stamp
        {
            return Err(StorageError::Changed);
        }
        fs::unlinkat(&directory, name.as_str(), fs::AtFlags::empty()).map_err(errno)?;
        directory.sync_all().map_err(io)?;
        self.revalidate_upload_staging(&directory)
    }

    #[cfg(feature = "test-hooks")]
    pub fn commit_upload_with_hook(
        &self,
        sealed: SealedUpload,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<StoredUpload, StorageError> {
        self.commit_upload_inner(sealed, &mut hook)
    }

    #[cfg(feature = "test-hooks")]
    pub fn prepare_upload_object_with_hook(
        &self,
        stored: &StoredUpload,
        mut hook: impl FnMut(StorageStage) -> Result<(), StorageError>,
    ) -> Result<String, StorageError> {
        self.prepare_upload_object_inner(stored, &mut hook)
    }
}
