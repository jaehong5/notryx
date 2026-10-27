use crate::{StorageError, errno, io, metadata_stamp};
use rustix::fs::{self, Gid, Mode, Uid, XattrFlags};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;

const XATTR_MAX: usize = 65_536;
// Untrusted attributes can be numerous; reject excessive metadata without dropping it.
const TOTAL_XATTR_MAX: usize = 4 * 1024 * 1024;
const ACL: &[u8] = b"system.posix_acl_access";

fn attributes(file: &File) -> Result<BTreeMap<OsString, Vec<u8>>, StorageError> {
    let mut names = vec![0; XATTR_MAX];
    let length = fs::flistxattr(file, &mut names).map_err(errno)?;
    names.truncate(length);
    let mut result = BTreeMap::new();
    let mut total = 0usize;
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = OsStr::from_bytes(name);
        let size = fs::fgetxattr(file, name, &mut [] as &mut [u8]).map_err(errno)?;
        total = total.checked_add(size).ok_or(StorageError::UnsafeFile)?;
        if size > XATTR_MAX || total > TOTAL_XATTR_MAX {
            return Err(StorageError::UnsafeFile);
        }
        let mut value = vec![0; size];
        let actual = fs::fgetxattr(file, name, &mut value).map_err(errno)?;
        if actual != size {
            return Err(StorageError::Changed);
        }
        result.insert(OsString::from_vec(name.as_bytes().to_vec()), value);
    }
    Ok(result)
}

fn managed(name: &OsStr) -> bool {
    name.as_bytes().starts_with(b"user.") || name.as_bytes() == ACL
}

pub(crate) fn fingerprint(file: &File) -> Result<String, StorageError> {
    let before = file.metadata().map_err(io)?;
    let attributes = attributes(file)?;
    let after = file.metadata().map_err(io)?;
    if metadata_stamp(&before, String::new()) != metadata_stamp(&after, String::new()) {
        return Err(StorageError::Changed);
    }
    let mut digest = Sha256::new();
    digest.update(before.uid().to_be_bytes());
    digest.update(before.gid().to_be_bytes());
    for (name, value) in attributes {
        let name = name.as_bytes();
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name);
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(crate) fn preserve(source: &File, target: &File) -> Result<(), StorageError> {
    let original = source.metadata().map_err(io)?;
    if original.mode() & 0o6000 != 0 {
        return Err(StorageError::UnsafeFile);
    }
    let expected = attributes(source)?;
    let created = target.metadata().map_err(io)?;
    if original.uid() != created.uid() || original.gid() != created.gid() {
        fs::fchown(
            target,
            (original.uid() != created.uid()).then(|| Uid::from_raw(original.uid())),
            (original.gid() != created.gid()).then(|| Gid::from_raw(original.gid())),
        )
        .map_err(errno)?;
    }
    fs::fchmod(target, Mode::from_raw_mode(original.mode() & 0o7777)).map_err(errno)?;
    for name in attributes(target)?.keys().filter(|name| managed(name)) {
        fs::fremovexattr(target, name.as_os_str()).map_err(errno)?;
    }
    for (name, value) in expected.iter().filter(|(name, _)| managed(name)) {
        fs::fsetxattr(target, name.as_os_str(), value, XattrFlags::empty()).map_err(errno)?;
    }
    let actual = target.metadata().map_err(io)?;
    if (actual.uid(), actual.gid(), actual.mode() & 0o7777)
        != (original.uid(), original.gid(), original.mode() & 0o7777)
        || attributes(target)? != expected
    {
        return Err(StorageError::UnsafeFile);
    }
    Ok(())
}
