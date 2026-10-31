mod online;
pub(crate) use online::create as create_online;

use crate::config::Config;
use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_server_storage::FileStore;
use rustix::fs::{self as rfs, Dir, Mode, OFlags, ResolveFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_SYMLINKS);
const CHILD_RESOLVE: ResolveFlags = RESOLVE.union(ResolveFlags::NO_XDEV);
const MAX_DIRECTORY_DEPTH: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 107;
const CURRENT_BACKUP_SCHEMA_VERSION: i64 = 10;

fn supported_backup_schema(version: i64) -> bool {
    matches!(version, 9 | CURRENT_BACKUP_SCHEMA_VERSION)
}

type BackupResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    kind: String,
    mode: u32,
    size: u64,
    sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    workspace_id: String,
    epoch: String,
    root_id: String,
    head_seq: String,
    schema_version: i64,
    app_version: String,
    observation: String,
    deployment: String,
    filesystem_metadata: String,
    entries: Vec<Entry>,
}

fn invalid() -> Box<dyn std::error::Error> {
    "backup_invalid".into()
}

fn open_dir(path: &Path) -> BackupResult<File> {
    let root = File::open("/")?;
    let relative = path.strip_prefix("/").map_err(|_| invalid())?;
    Ok(File::from(rfs::openat2(
        &root,
        relative,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        RESOLVE,
    )?))
}

fn open_child(parent: &File, name: &std::ffi::OsStr, directory: bool) -> BackupResult<File> {
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    Ok(File::from(rfs::openat2(
        parent,
        name,
        flags,
        Mode::empty(),
        CHILD_RESOLVE,
    )?))
}

fn entry_still_matches(
    parent: &File,
    name: &std::ffi::OsStr,
    expected: &fs::Metadata,
) -> BackupResult<bool> {
    let entry = File::from(rfs::openat2(
        parent,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        CHILD_RESOLVE,
    )?);
    Ok(stable(expected, &entry.metadata()?))
}

fn encode_path(path: &Path) -> String {
    STANDARD.encode(path.as_os_str().as_bytes())
}

fn decode_path(value: &str) -> BackupResult<PathBuf> {
    let bytes = STANDARD.decode(value).map_err(|_| invalid())?;
    let path = PathBuf::from(OsString::from_vec(bytes));
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|item| !matches!(item, Component::Normal(_)))
        || encode_path(&path) != value
    {
        return Err(invalid());
    }
    Ok(path)
}

fn digest_reader(
    mut reader: impl Read,
    mut output: Option<&mut File>,
) -> BackupResult<(String, u64)> {
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        if let Some(writer) = output.as_mut() {
            writer.write_all(&buffer[..n])?;
        }
        total = total.checked_add(n as u64).ok_or_else(invalid)?;
    }
    Ok((hex(&digest.finalize()), total))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn stable(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) == (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    )
}

fn same_inode(left: &File, right: &File) -> BackupResult<bool> {
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

fn same_restore_directories(
    parent: &File,
    name: &std::ffi::OsStr,
    stage: &File,
    notes: &File,
    state: &File,
    config: &File,
) -> BackupResult<bool> {
    let current = open_child(parent, name, true)?;
    Ok(same_inode(stage, &current)?
        && same_inode(
            notes,
            &open_child(&current, std::ffi::OsStr::new("notes"), true)?,
        )?
        && same_inode(
            state,
            &open_child(&current, std::ffi::OsStr::new("state"), true)?,
        )?
        && same_inode(
            config,
            &open_child(&current, std::ffi::OsStr::new("config"), true)?,
        )?)
}

fn contains_directory(root: &File, wanted: &File) -> BackupResult<bool> {
    contains_directory_at_depth(root, wanted, 0)
}

fn contains_directory_at_depth(root: &File, wanted: &File, depth: usize) -> BackupResult<bool> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err("backup_directory_depth_exceeded".into());
    }
    if same_inode(root, wanted)? {
        return Ok(true);
    }
    for entry in Dir::read_from(root)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = std::ffi::OsStr::from_bytes(name);
        let probe = File::from(rfs::openat2(
            root,
            name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            CHILD_RESOLVE,
        )?);
        if probe.metadata()?.is_dir() {
            let child = open_child(root, name, true)?;
            if !same_inode(&probe, &child)? {
                return Err(invalid());
            }
            drop(probe);
            if contains_directory_at_depth(&child, wanted, depth + 1)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn check_private_parent(parent: &File) -> BackupResult<()> {
    let metadata = parent.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err("backup_parent_not_private".into());
    }
    match rfs::fgetxattr(parent, "system.posix_acl_access", &mut [] as &mut [u8]) {
        Err(rustix::io::Errno::NODATA) => Ok(()),
        Ok(_) => Err("backup_parent_acl_unsupported".into()),
        Err(error) => Err(error.into()),
    }
}

fn create_dir(parent: &File, name: &std::ffi::OsStr) -> BackupResult<File> {
    rfs::mkdirat(parent, name, Mode::RWXU)?;
    open_child(parent, name, true)
}

fn create_file(parent: &File, name: &std::ffi::OsStr) -> BackupResult<File> {
    Ok(File::from(rfs::openat2(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
        CHILD_RESOLVE,
    )?))
}

fn walk(
    source: &File,
    relative: &Path,
    destination: Option<&File>,
    skip_lock: Option<&[u8]>,
    entries: &mut Vec<Entry>,
) -> BackupResult<()> {
    walk_at_depth(
        source,
        relative,
        destination,
        skip_lock,
        entries,
        0,
        #[cfg(test)]
        false,
    )
}

fn walk_at_depth(
    source: &File,
    relative: &Path,
    destination: Option<&File>,
    skip_lock: Option<&[u8]>,
    entries: &mut Vec<Entry>,
    depth: usize,
    #[cfg(test)] fail_copy_sync: bool,
) -> BackupResult<()> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err("backup_directory_depth_exceeded".into());
    }
    let directory_before = source.metadata()?;
    let mut names = Vec::new();
    for item in Dir::read_from(source)? {
        let item = item?;
        let bytes = item.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if skip_lock == Some(bytes) {
            continue;
        }
        names.push(OsString::from_vec(bytes.to_vec()));
    }
    names.sort();
    for name in names {
        let probe = File::from(rfs::openat2(
            source,
            &name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            CHILD_RESOLVE,
        )?);
        let metadata = probe.metadata()?;
        let rel = relative.join(&name);
        if metadata.is_dir() {
            let child = open_child(source, &name, true)?;
            if !stable(&metadata, &child.metadata()?)
                || !entry_still_matches(source, &name, &metadata)?
            {
                return Err(invalid());
            }
            let target = destination
                .map(|directory| create_dir(directory, &name))
                .transpose()?;
            entries.push(Entry {
                path: encode_path(&rel),
                kind: "directory".into(),
                mode: metadata.mode() & 0o7777,
                size: 0,
                sha256: None,
            });
            walk_at_depth(
                &child,
                &rel,
                target.as_ref(),
                None,
                entries,
                depth + 1,
                #[cfg(test)]
                fail_copy_sync,
            )?;
            if !stable(&metadata, &child.metadata()?)
                || !entry_still_matches(source, &name, &metadata)?
            {
                return Err(invalid());
            }
            if let Some(directory) = target {
                directory.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o7777))?;
                directory.sync_all()?;
            }
        } else if metadata.is_file() {
            let mut input = open_child(source, &name, false)?;
            if !stable(&metadata, &input.metadata()?) {
                return Err(invalid());
            }
            let (hash, size) = if let Some(directory) = destination {
                let mut output = create_file(directory, &name)?;
                let digest = digest_reader(&mut input, Some(&mut output))?;
                output.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o7777))?;
                #[cfg(test)]
                if fail_copy_sync {
                    return Err("restore_injected_copy_fsync_failure".into());
                }
                output.sync_all()?;
                digest
            } else {
                digest_reader(&mut input, None)?
            };
            if size != metadata.len()
                || !stable(&metadata, &input.metadata()?)
                || !entry_still_matches(source, &name, &metadata)?
            {
                return Err(invalid());
            }
            entries.push(Entry {
                path: encode_path(&rel),
                kind: "file".into(),
                mode: metadata.mode() & 0o7777,
                size,
                sha256: Some(hash),
            });
        } else if metadata.file_type().is_symlink() {
            let link = rfs::readlinkat(&probe, "", Vec::new())?;
            if !stable(&metadata, &probe.metadata()?)
                || !entry_still_matches(source, &name, &metadata)?
            {
                return Err(invalid());
            }
            let bytes = link.as_bytes();
            if let Some(directory) = destination {
                rfs::symlinkat(std::ffi::OsStr::from_bytes(bytes), directory, &name)?;
            }
            entries.push(Entry {
                path: encode_path(&rel),
                kind: "symlink".into(),
                mode: metadata.mode() & 0o7777,
                size: bytes.len() as u64,
                sha256: Some(hex(&Sha256::digest(bytes))),
            });
        } else {
            return Err("backup_unsupported_file_type".into());
        }
    }
    if !stable(&directory_before, &source.metadata()?) {
        return Err(invalid());
    }
    Ok(())
}

fn write_synced(
    parent: &File,
    name: &str,
    bytes: &[u8],
    #[cfg(test)] fail_sync: bool,
) -> BackupResult<()> {
    let mut file = create_file(parent, std::ffi::OsStr::new(name))?;
    file.write_all(bytes)?;
    #[cfg(test)]
    if fail_sync {
        return Err("backup_injected_fsync_failure".into());
    }
    file.sync_all()?;
    Ok(())
}

fn read_bounded(root: &File, name: &str, limit: u64) -> BackupResult<Vec<u8>> {
    let file = open_child(root, std::ffi::OsStr::new(name), false)?;
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid());
    }
    Ok(bytes)
}

async fn database_info(
    stage: &File,
    expected_workspace: &str,
) -> BackupResult<(String, String, String, i64)> {
    let state = open_child(stage, std::ffi::OsStr::new("state"), true)?;
    let scratch = create_dir(stage, std::ffi::OsStr::new(".verification"))?;
    for name in ["state.db", "state.db-wal"] {
        let source = match rfs::openat2(
            &state,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            CHILD_RESOLVE,
        ) {
            Ok(file) => File::from(file),
            Err(rustix::io::Errno::NOENT) if name == "state.db-wal" => continue,
            Err(error) => return Err(error.into()),
        };
        if !source.metadata()?.is_file() {
            return Err(invalid());
        }
        let mut target = create_file(&scratch, std::ffi::OsStr::new(name))?;
        std::io::copy(&mut &source, &mut target)?;
    }
    let copied = PathBuf::from(format!("/proc/self/fd/{}/state.db", scratch.as_raw_fd()));
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&copied)
            .read_only(true),
    )
    .await?;
    let app: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut db)
        .await?;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await?;
    let check: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut db)
        .await?;
    let fk = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut db)
        .await?;
    if app != 1314148947 || !supported_backup_schema(version) || check != ["ok"] || !fk.is_empty() {
        return Err(invalid());
    }
    let row =
        sqlx::query("SELECT workspace_id,epoch,root_id,head_seq FROM workspaces WHERE singleton=1")
            .fetch_one(&mut db)
            .await?;
    let workspace: String = row.try_get("workspace_id")?;
    let epoch: String = row.try_get("epoch")?;
    let root: String = row.try_get("root_id")?;
    let head: String = row.try_get("head_seq")?;
    if workspace != expected_workspace
        || epoch.parse::<notryx_domain::WorkspaceEpoch>().is_err()
        || root.parse::<notryx_domain::NodeId>().is_err()
        || head
            .parse::<u64>()
            .map(|v| v.to_string() != head)
            .unwrap_or(true)
    {
        return Err(invalid());
    }
    db.close().await?;
    crate::validate_backup_state_copy(&copied, expected_workspace.parse().map_err(|_| invalid())?)
        .await?;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&copied)
            .read_only(true),
    )
    .await?;
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM write_intents")
        .fetch_one(&mut db)
        .await?;
    let unresolved: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM recovery_cases WHERE resolved=0")
            .fetch_one(&mut db)
            .await?;
    if pending != 0 || unresolved != 0 {
        return Err("backup_recovery_required".into());
    }
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    )?;
    let uploads: Vec<(String, String, i64)> = sqlx::query_as("SELECT upload_id,content_hash,size FROM uploads WHERE status='ready' AND (pending_operation_id IS NOT NULL OR expires_at>?)")
        .bind(now).fetch_all(&mut db).await?;
    let staging = open_child(&state, std::ffi::OsStr::new("staging"), true)?;
    if staging.metadata()?.mode() & 0o777 != 0o700 {
        return Err(invalid());
    }
    for entry in Dir::read_from(&staging)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        let Some((id, extension)) = name
            .strip_prefix("upload-")
            .and_then(|name| name.rsplit_once('.'))
        else {
            continue;
        };
        if !matches!(extension, "ready" | "part") {
            continue;
        }
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            continue;
        };
        if uuid.to_string() != id {
            continue;
        }
        let file = open_child(&staging, std::ffi::OsStr::new(name), false)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || !entry_still_matches(&staging, std::ffi::OsStr::new(name), &metadata)?
        {
            return Err(invalid());
        }
    }
    for (id, expected_hash, expected_size) in uploads {
        let id = uuid::Uuid::parse_str(&id)?;
        let name = format!("upload-{id}.ready");
        let file = open_child(&staging, std::ffi::OsStr::new(&name), false)?;
        let before = file.metadata()?;
        if !before.is_file() || before.nlink() != 1 {
            return Err(invalid());
        }
        let (hash, size) = digest_reader(&file, None)?;
        if hash != expected_hash
            || i64::try_from(size)? != expected_size
            || !stable(&before, &file.metadata()?)
            || !entry_still_matches(&staging, std::ffi::OsStr::new(&name), &before)?
        {
            return Err(invalid());
        }
    }
    let hashes: Vec<String> = sqlx::query_scalar("SELECT content_hash FROM nodes WHERE content_hash IS NOT NULL UNION SELECT hash FROM recovery_objects").fetch_all(&mut db).await?;
    let object_parent = open_child(&state, std::ffi::OsStr::new("objects"), true)?;
    let objects = open_child(&object_parent, std::ffi::OsStr::new("sha256"), true)?;
    let mut object_sizes = BTreeMap::new();
    for hash in hashes {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid());
        }
        let object = open_child(&objects, std::ffi::OsStr::new(&hash), false)?;
        if !object.metadata()?.is_file() {
            return Err(invalid());
        }
        let (actual, size) = digest_reader(object, None)?;
        if actual != hash {
            return Err(invalid());
        }
        object_sizes.insert(hash, size);
    }
    verify_tree_and_notes(&mut db, stage, &root, head.parse()?, &object_sizes).await?;
    db.close().await?;
    cleanup_verification(stage, &scratch)?;
    Ok((epoch, root, head, version))
}

fn cleanup_verification(stage: &File, scratch: &File) -> BackupResult<()> {
    let mut names = Vec::new();
    for entry in Dir::read_from(scratch)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if !matches!(
            name,
            b"state.db" | b"state.db-wal" | b"state.db-shm" | b"state.db-journal"
        ) {
            return Err(invalid());
        }
        let name = OsString::from_vec(name.to_vec());
        let probe = File::from(rfs::openat2(
            scratch,
            &name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            CHILD_RESOLVE,
        )?);
        if !probe.metadata()?.is_file() {
            return Err(invalid());
        }
        names.push(name);
    }
    for name in names {
        rfs::unlinkat(scratch, &name, rfs::AtFlags::empty())?;
    }
    scratch.sync_all()?;
    if !same_inode(
        scratch,
        &open_child(stage, std::ffi::OsStr::new(".verification"), true)?,
    )? {
        return Err(invalid());
    }
    rfs::unlinkat(stage, ".verification", rfs::AtFlags::REMOVEDIR)?;
    Ok(())
}

struct Node {
    parent: Option<String>,
    name: String,
    kind: String,
    deleted: bool,
    hash: Option<String>,
    size: Option<u64>,
}

async fn verify_tree_and_notes(
    db: &mut SqliteConnection,
    stage: &File,
    root: &str,
    head: u64,
    object_sizes: &BTreeMap<String, u64>,
) -> BackupResult<()> {
    let rows = sqlx::query("SELECT node_id,parent_id,name,normalized_name_key,kind,deleted,content_hash,size,last_change_seq FROM nodes")
        .fetch_all(&mut *db).await?;
    let mut nodes = BTreeMap::new();
    for row in rows {
        let id: String = row.try_get("node_id")?;
        let parsed: notryx_domain::NodeId = id.parse().map_err(|_| invalid())?;
        let parent: Option<String> = row.try_get("parent_id")?;
        let name: String = row.try_get("name")?;
        let kind: String = row.try_get("kind")?;
        let deleted: i64 = row.try_get("deleted")?;
        let hash: Option<String> = row.try_get("content_hash")?;
        let size: Option<String> = row.try_get("size")?;
        let size = size
            .map(|value| {
                value.parse::<u64>().map_err(|_| invalid()).and_then(|n| {
                    if n.to_string() == value {
                        Ok(n)
                    } else {
                        Err(invalid())
                    }
                })
            })
            .transpose()?;
        let seq_text: String = row.try_get("last_change_seq")?;
        let seq = seq_text.parse::<u64>().map_err(|_| invalid())?;
        if parsed.to_string() != id
            || seq > head
            || !matches!(deleted, 0 | 1)
            || seq.to_string() != seq_text
            || (id == root
                && (parent.is_some() || !name.is_empty() || kind != "folder" || deleted != 0))
            || (id != root
                && (parent.is_none()
                    || seq == 0
                    || notryx_domain::mutation::validate_name(&name).is_err()))
            || row.try_get::<String, _>("normalized_name_key")?
                != notryx_domain::mutation::normalized_name_key(&name)
        {
            return Err(invalid());
        }
        match kind.as_str() {
            "file"
                if hash
                    .as_ref()
                    .and_then(|hash| object_sizes.get(hash))
                    .copied()
                    == size
                    && size.is_some() => {}
            "folder" if hash.is_none() && size.is_none() => {}
            _ => return Err(invalid()),
        }
        if nodes
            .insert(
                id,
                Node {
                    parent,
                    name,
                    kind,
                    deleted: deleted == 1,
                    hash,
                    size,
                },
            )
            .is_some()
        {
            return Err(invalid());
        }
    }
    if !nodes.contains_key(root) {
        return Err(invalid());
    }
    let notes = open_child(stage, std::ffi::OsStr::new("notes"), true)?;
    let mut names = HashSet::new();
    for (id, node) in &nodes {
        if node.deleted || id == root {
            continue;
        }
        let parent_id = node.parent.as_ref().ok_or_else(invalid)?;
        let parent = nodes.get(parent_id).ok_or_else(invalid)?;
        if parent.deleted
            || parent.kind != "folder"
            || !names.insert((
                parent_id,
                notryx_domain::mutation::normalized_name_key(&node.name),
            ))
        {
            return Err(invalid());
        }
        let mut current = id.as_str();
        let mut seen = HashSet::new();
        let mut components = Vec::new();
        while current != root {
            if !seen.insert(current) {
                return Err(invalid());
            }
            let item = nodes.get(current).ok_or_else(invalid)?;
            if item.deleted {
                return Err(invalid());
            }
            components.push(item.name.as_str());
            current = item.parent.as_deref().ok_or_else(invalid)?;
        }
        let path: PathBuf = components.iter().rev().collect();
        if path.as_os_str().as_bytes().len() > 1024 {
            return Err(invalid());
        }
        let directory = node.kind == "folder";
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if directory {
            flags |= OFlags::DIRECTORY;
        }
        let file = File::from(rfs::openat2(
            &notes,
            &path,
            flags,
            Mode::empty(),
            CHILD_RESOLVE,
        )?);
        let metadata = file.metadata()?;
        if directory {
            if !metadata.is_dir() {
                return Err(invalid());
            }
        } else {
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(invalid());
            }
            let (hash, size) = digest_reader(&file, None)?;
            if Some(&hash) != node.hash.as_ref()
                || Some(size) != node.size
                || !stable(&metadata, &file.metadata()?)
            {
                return Err("backup_external_change".into());
            }
        }
    }
    Ok(())
}

pub async fn create(config: &Config, config_path: &Path, destination: &Path) -> BackupResult<()> {
    create_inner(
        config,
        config_path,
        destination,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackupFailpoint {
    ManifestFsync,
    StageFsync,
    Rename,
    ParentFsync,
    MarkerFsync,
    MarkerRename,
    CompletedDirectoryFsync,
    MarkerCleanupFailure,
}

async fn create_inner(
    config: &Config,
    config_path: &Path,
    destination: &Path,
    #[cfg(test)] after_open: Option<Box<dyn FnOnce()>>,
    #[cfg(test)] failpoint: Option<BackupFailpoint>,
) -> BackupResult<()> {
    create_with_manifest_limit(
        config,
        config_path,
        destination,
        #[cfg(test)]
        after_open,
        #[cfg(test)]
        failpoint,
        #[cfg(test)]
        None,
    )
    .await
}

async fn create_with_manifest_limit(
    config: &Config,
    config_path: &Path,
    destination: &Path,
    #[cfg(test)] after_open: Option<Box<dyn FnOnce()>>,
    #[cfg(test)] failpoint: Option<BackupFailpoint>,
    #[cfg(test)] manifest_limit: Option<u64>,
) -> BackupResult<()> {
    if !destination.is_absolute() || destination.file_name().is_none() {
        return Err(invalid());
    }
    let destination_parent = destination.parent().ok_or_else(invalid)?;
    let parent_fd = open_dir(destination_parent)?;
    check_private_parent(&parent_fd)?;
    let notes_before = open_dir(&config.workspace.notes_root)?;
    let state_before = open_dir(&config.workspace.state_root)?;
    let parent = fs::canonicalize(destination_parent)?;
    let notes = fs::canonicalize(&config.workspace.notes_root)?;
    let state = fs::canonicalize(&config.workspace.state_root)?;
    if notes.starts_with(&state)
        || state.starts_with(&notes)
        || parent.starts_with(&notes)
        || parent.starts_with(&state)
    {
        return Err(invalid());
    }
    let destination_name = destination.file_name().ok_or_else(invalid)?;
    let destination_bytes = destination_name.as_bytes();
    if destination_bytes.starts_with(b".notryx-backup-")
        && destination_bytes.ends_with(b".incomplete")
    {
        return Err(invalid());
    }
    match rfs::statat(&parent_fd, destination_name, rfs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => return Err("backup_target_exists".into()),
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let files = FileStore::open(&config.workspace.notes_root, &config.workspace.state_root)?;
    let (notes_source, state_source) = files.backup_root_handles()?;
    if !same_inode(&notes_before, &notes_source)?
        || !same_inode(&state_before, &state_source)?
        || !same_inode(&parent_fd, &open_dir(destination_parent)?)?
    {
        return Err(invalid());
    }
    if contains_directory(&notes_source, &parent_fd)?
        || contains_directory(&state_source, &parent_fd)?
    {
        return Err(invalid());
    }
    #[cfg(test)]
    if let Some(after_open) = after_open {
        after_open();
    }
    let workspace_id = files.workspace_id().to_string();
    let stage_name = format!(".notryx-backup-{}.incomplete", uuid::Uuid::new_v4());
    let stage = create_dir(&parent_fd, std::ffi::OsStr::new(&stage_name))?;
    rfs::flock(&stage, rfs::FlockOperation::NonBlockingLockExclusive)?;
    let mut entries = Vec::new();
    for (name, source, lock) in [
        ("notes", &notes_source, b".mdw-server.lock".as_slice()),
        ("state", &state_source, b"server.lock".as_slice()),
    ] {
        let target = create_dir(&stage, std::ffi::OsStr::new(name))?;
        let mode = source.metadata()?.mode() & 0o7777;
        entries.push(Entry {
            path: encode_path(Path::new(name)),
            kind: "directory".into(),
            mode,
            size: 0,
            sha256: None,
        });
        walk(
            source,
            Path::new(name),
            Some(&target),
            Some(lock),
            &mut entries,
        )?;
        target.set_permissions(fs::Permissions::from_mode(mode))?;
        target.sync_all()?;
    }
    let config_dir = create_dir(&stage, std::ffi::OsStr::new("config"))?;
    entries.push(Entry {
        path: encode_path(Path::new("config")),
        kind: "directory".into(),
        mode: 0o700,
        size: 0,
        sha256: None,
    });
    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(config_path)
    };
    let config_parent = open_dir(config_path.parent().ok_or_else(invalid)?)?;
    let mut source = open_child(
        &config_parent,
        config_path.file_name().ok_or_else(invalid)?,
        false,
    )?;
    let before = source.metadata()?;
    if !before.is_file() || before.len() > 65536 {
        return Err(invalid());
    }
    let mut config_bytes = Vec::new();
    source.read_to_end(&mut config_bytes)?;
    if !stable(&before, &source.metadata()?) {
        return Err(invalid());
    }
    let text = std::str::from_utf8(&config_bytes).map_err(|_| invalid())?;
    let copied_config = Config::from_toml(text)?;
    if copied_config.sync != config.sync
        || copied_config.workspace.notes_root != config.workspace.notes_root
        || copied_config.workspace.state_root != config.workspace.state_root
        || copied_config.workspace.uploads.max_device_bytes
            != config.workspace.uploads.max_device_bytes
        || copied_config.workspace.uploads.max_device_uploads
            != config.workspace.uploads.max_device_uploads
        || copied_config.workspace.uploads.max_workspace_bytes
            != config.workspace.uploads.max_workspace_bytes
        || copied_config.workspace.uploads.max_workspace_uploads
            != config.workspace.uploads.max_workspace_uploads
        || copied_config.workspace.uploads.max_receiving != config.workspace.uploads.max_receiving
        || copied_config.server.listen != config.server.listen
        || copied_config.server.admin_socket != config.server.admin_socket
    {
        return Err(invalid());
    }
    let mut target_config = create_file(&config_dir, std::ffi::OsStr::new("server.toml"))?;
    target_config.write_all(&config_bytes)?;
    target_config.sync_all()?;
    entries.push(Entry {
        path: encode_path(Path::new("config/server.toml")),
        kind: "file".into(),
        mode: 0o600,
        size: config_bytes.len() as u64,
        sha256: Some(hex(&Sha256::digest(&config_bytes))),
    });
    config_dir.sync_all()?;
    let staged_notes = open_child(&stage, std::ffi::OsStr::new("notes"), true)?;
    let marker: serde_json::Value =
        serde_json::from_slice(&read_bounded(&staged_notes, ".mdw-workspace.json", 4096)?)?;
    if marker["workspace_id"].as_str() != Some(workspace_id.as_str())
        || marker["format_version"] != 1
    {
        return Err(invalid());
    }
    let (epoch, root_id, head_seq, schema_version) = database_info(&stage, &workspace_id).await?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest {
        format: 1,
        workspace_id,
        epoch,
        root_id,
        head_seq,
        schema_version,
        app_version: env!("CARGO_PKG_VERSION").into(),
        observation: "external_writer_quiescence_and_final_reconciliation_unverified".into(),
        deployment: "not_included".into(),
        filesystem_metadata: "mode_only_uid_gid_acl_xattr_hardlinks_and_timestamps_not_preserved"
            .into(),
        entries,
    };
    let bytes = serde_json::to_vec(&manifest)?;
    #[cfg(test)]
    let limit = manifest_limit.unwrap_or(MAX_MANIFEST_BYTES);
    #[cfg(not(test))]
    let limit = MAX_MANIFEST_BYTES;
    if u64::try_from(bytes.len())? > limit {
        return Err("backup_manifest_too_large".into());
    }
    #[cfg(test)]
    write_synced(
        &stage,
        "manifest.json",
        &bytes,
        failpoint == Some(BackupFailpoint::ManifestFsync),
    )?;
    #[cfg(not(test))]
    write_synced(&stage, "manifest.json", &bytes)?;
    let hash = hex(&Sha256::digest(&bytes));
    #[cfg(test)]
    if failpoint == Some(BackupFailpoint::StageFsync) {
        return Err("backup_injected_fsync_failure".into());
    }
    stage.sync_all()?;
    if !same_inode(&parent_fd, &open_dir(destination_parent)?)?
        || !same_inode(&notes_source, &open_dir(&config.workspace.notes_root)?)?
        || !same_inode(&state_source, &open_dir(&config.workspace.state_root)?)?
    {
        return Err(invalid());
    }
    #[cfg(test)]
    if failpoint == Some(BackupFailpoint::Rename) {
        return Err("backup_injected_rename_failure".into());
    }
    rfs::renameat_with(
        &parent_fd,
        &stage_name,
        &parent_fd,
        destination.file_name().ok_or_else(invalid)?,
        rfs::RenameFlags::NOREPLACE,
    )?;
    #[cfg(test)]
    if failpoint == Some(BackupFailpoint::ParentFsync) {
        return Err("backup_injected_fsync_failure".into());
    }
    parent_fd.sync_all()?;
    #[cfg(test)]
    let marker = write_synced(
        &stage,
        ".COMPLETE.tmp",
        hash.as_bytes(),
        failpoint == Some(BackupFailpoint::MarkerFsync),
    );
    #[cfg(not(test))]
    let marker = write_synced(&stage, ".COMPLETE.tmp", hash.as_bytes());
    let marker = marker.and_then(|()| {
        #[cfg(test)]
        if failpoint == Some(BackupFailpoint::MarkerRename) {
            return Err("backup_injected_rename_failure".into());
        }
        rfs::renameat_with(
            &stage,
            ".COMPLETE.tmp",
            &stage,
            "COMPLETE",
            rfs::RenameFlags::NOREPLACE,
        )?;
        Ok(())
    });
    if let Err(error) = marker.and_then(|()| {
        #[cfg(test)]
        if matches!(
            failpoint,
            Some(BackupFailpoint::CompletedDirectoryFsync | BackupFailpoint::MarkerCleanupFailure)
        ) {
            return Err("backup_injected_fsync_failure".into());
        }
        stage.sync_all().map_err(Into::into)
    }) {
        #[cfg(test)]
        if failpoint == Some(BackupFailpoint::MarkerCleanupFailure) {
            return Err("backup_publication_indeterminate".into());
        }
        for name in ["COMPLETE", ".COMPLETE.tmp"] {
            match rfs::unlinkat(&stage, name, rfs::AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(_) => return Err("backup_publication_indeterminate".into()),
            }
        }
        if stage.sync_all().is_err() {
            return Err("backup_publication_indeterminate".into());
        }
        return Err(error);
    }
    drop(files);
    Ok(())
}

pub fn verify(path: &Path) -> BackupResult<()> {
    if !path.is_absolute() {
        return Err(invalid());
    }
    if path.file_name().is_some_and(|name| {
        name.as_bytes().starts_with(b".notryx-backup-") && name.as_bytes().ends_with(b".incomplete")
    }) {
        return Err(invalid());
    }
    let root = open_dir(path)?;
    rfs::flock(&root, rfs::FlockOperation::NonBlockingLockShared)?;
    verify_root(&root)
}

fn verify_root(root: &File) -> BackupResult<()> {
    let manifest = read_bounded(root, "manifest.json", MAX_MANIFEST_BYTES)?;
    let complete = read_bounded(root, "COMPLETE", 64)?;
    if complete != hex(&Sha256::digest(&manifest)).as_bytes() {
        return Err(invalid());
    }
    let manifest: Manifest = serde_json::from_slice(&manifest)?;
    if manifest.format != 1 || !supported_backup_schema(manifest.schema_version) {
        return Err(invalid());
    }
    let mut expected = BTreeMap::new();
    for entry in manifest.entries {
        let path = decode_path(&entry.path)?;
        if expected.insert(path, entry).is_some() {
            return Err(invalid());
        }
    }
    let mut actual = Vec::new();
    for name in ["notes", "state", "config"] {
        let dir = open_child(root, std::ffi::OsStr::new(name), true)?;
        let metadata = dir.metadata()?;
        actual.push(Entry {
            path: encode_path(Path::new(name)),
            kind: "directory".into(),
            mode: metadata.mode() & 0o7777,
            size: 0,
            sha256: None,
        });
        walk(&dir, Path::new(name), None, None, &mut actual)?;
    }
    let mut seen = HashSet::new();
    for entry in actual {
        let path = decode_path(&entry.path)?;
        if !seen.insert(path.clone()) || expected.get(&path) != Some(&entry) {
            return Err(invalid());
        }
    }
    if seen.len() != expected.len() {
        return Err(invalid());
    }
    let mut top = HashSet::new();
    for item in Dir::read_from(root)? {
        let item = item?;
        let name = item.file_name().to_bytes();
        if name != b"." && name != b".." {
            top.insert(name.to_vec());
        }
    }
    if top
        != [
            b"notes".to_vec(),
            b"state".to_vec(),
            b"config".to_vec(),
            b"manifest.json".to_vec(),
            b"COMPLETE".to_vec(),
        ]
        .into_iter()
        .collect()
    {
        return Err(invalid());
    }
    Ok(())
}

pub async fn restore(backup: &Path, destination: &Path) -> BackupResult<()> {
    restore_inner(
        backup,
        destination,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
type RestorePathHook = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RestoreFailpoint {
    CopyFsync,
    TransactionBeforeCommit,
    Checkpoint,
    DatabaseFsync,
    StageFsync,
    Rename,
    ParentFsync,
}

#[cfg_attr(
    test,
    allow(
        clippy::too_many_arguments,
        reason = "Failure hooks expose each restore boundary only in tests."
    )
)]
async fn restore_inner(
    backup: &Path,
    destination: &Path,
    #[cfg(test)] failpoint: Option<RestoreFailpoint>,
    #[cfg(test)] after_stage: Option<Box<dyn FnOnce()>>,
    #[cfg(test)] after_validation: Option<RestorePathHook>,
    #[cfg(test)] after_final_verification: Option<RestorePathHook>,
    #[cfg(test)] after_publication: Option<RestorePathHook>,
    #[cfg(test)] after_manifest_verification: Option<RestorePathHook>,
) -> BackupResult<()> {
    if !backup.is_absolute() || !destination.is_absolute() {
        return Err(invalid());
    }
    if backup.file_name().is_some_and(|name| {
        name.as_bytes().starts_with(b".notryx-backup-") && name.as_bytes().ends_with(b".incomplete")
    }) {
        return Err(invalid());
    }
    let parent_path = destination.parent().ok_or_else(invalid)?;
    let name = destination.file_name().ok_or_else(invalid)?;
    if name.as_bytes().starts_with(b".notryx-restore-") && name.as_bytes().ends_with(b".incomplete")
    {
        return Err(invalid());
    }
    let parent = open_dir(parent_path)?;
    check_private_parent(&parent)?;
    match rfs::statat(&parent, name, rfs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => return Err("restore_target_exists".into()),
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let source = open_dir(backup)?;
    rfs::flock(&source, rfs::FlockOperation::NonBlockingLockShared)?;
    if contains_directory(&source, &parent)?
        || fs::canonicalize(parent_path)?.starts_with(fs::canonicalize(backup)?)
    {
        return Err(invalid());
    }
    verify_root(&source)?;
    let config_dir = open_child(&source, std::ffi::OsStr::new("config"), true)?;
    let config_bytes = read_bounded(&config_dir, "server.toml", 65536)?;
    let config_text = std::str::from_utf8(&config_bytes).map_err(|_| invalid())?;
    let original_config = Config::from_toml(config_text)?;
    let canonical_parent = fs::canonicalize(parent_path)?;
    for original in [
        &original_config.workspace.notes_root,
        &original_config.workspace.state_root,
    ] {
        match open_dir(original) {
            Ok(root) => {
                if contains_directory(&root, &parent)?
                    || canonical_parent.starts_with(fs::canonicalize(original)?)
                {
                    return Err("restore_target_overlaps_source".into());
                }
            }
            Err(error)
                if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {
            }
            Err(error) => return Err(error),
        }
    }
    let final_path = canonical_parent.join(name);
    let admin_socket = final_path.join("notryx-admin.sock");
    if admin_socket.as_os_str().as_bytes().len() > MAX_UNIX_SOCKET_PATH_BYTES {
        return Err("restore_admin_socket_path_too_long".into());
    }
    let path_text = |path: PathBuf| -> BackupResult<String> {
        path.to_str().map(str::to_owned).ok_or_else(invalid)
    };
    let mut settings: toml::Value = toml::from_str(config_text)?;
    let settings_table = settings.as_table_mut().ok_or_else(invalid)?;
    let workspace = settings_table
        .get_mut("workspace")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(invalid)?;
    workspace.insert(
        "notes_root".into(),
        toml::Value::String(path_text(final_path.join("notes"))?),
    );
    workspace.insert(
        "state_root".into(),
        toml::Value::String(path_text(final_path.join("state"))?),
    );
    let server = settings_table
        .entry("server")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(invalid)?;
    server.insert("listen".into(), toml::Value::String("127.0.0.1:0".into()));
    server.insert(
        "admin_socket".into(),
        toml::Value::String(path_text(admin_socket)?),
    );
    let restored_config = toml::to_string(&settings)?;
    Config::from_toml(&restored_config)?;
    let manifest_bytes = read_bounded(&source, "manifest.json", MAX_MANIFEST_BYTES)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    let complete = read_bounded(&source, "COMPLETE", 64)?;
    let stage_name = format!(".notryx-restore-{}.incomplete", uuid::Uuid::new_v4());
    let stage = create_dir(&parent, std::ffi::OsStr::new(&stage_name))?;
    #[cfg(test)]
    if let Some(after_stage) = after_stage {
        after_stage();
    }
    let mut stage_directories = Vec::with_capacity(3);
    for part in ["notes", "state", "config"] {
        let from = open_child(&source, std::ffi::OsStr::new(part), true)?;
        let to = create_dir(&stage, std::ffi::OsStr::new(part))?;
        let mut entries = Vec::new();
        #[cfg(test)]
        walk_at_depth(
            &from,
            Path::new(part),
            Some(&to),
            None,
            &mut entries,
            0,
            failpoint == Some(RestoreFailpoint::CopyFsync),
        )?;
        #[cfg(not(test))]
        walk(&from, Path::new(part), Some(&to), None, &mut entries)?;
        to.set_permissions(fs::Permissions::from_mode(from.metadata()?.mode() & 0o7777))?;
        to.sync_all()?;
        stage_directories.push(to);
    }
    let [notes, state, restored_config_dir]: [File; 3] =
        stage_directories.try_into().map_err(|_| invalid())?;
    let stage_path = parent_path.join(&stage_name);
    #[cfg(test)]
    write_synced(&stage, "manifest.json", &manifest_bytes, false)?;
    #[cfg(not(test))]
    write_synced(&stage, "manifest.json", &manifest_bytes)?;
    #[cfg(test)]
    write_synced(&stage, "COMPLETE", &complete, false)?;
    #[cfg(not(test))]
    write_synced(&stage, "COMPLETE", &complete)?;
    stage.sync_all()?;
    if !same_restore_directories(
        &parent,
        std::ffi::OsStr::new(&stage_name),
        &stage,
        &notes,
        &state,
        &restored_config_dir,
    )? {
        return Err(invalid());
    }
    verify_root(&stage)?;
    #[cfg(test)]
    if let Some(after_manifest_verification) = after_manifest_verification {
        after_manifest_verification(&stage_path);
    }
    if !same_restore_directories(
        &parent,
        std::ffi::OsStr::new(&stage_name),
        &stage,
        &notes,
        &state,
        &restored_config_dir,
    )? {
        return Err(invalid());
    }
    let (old_epoch, root_id, head, version) = database_info(&stage, &manifest.workspace_id).await?;
    if old_epoch != manifest.epoch
        || root_id != manifest.root_id
        || head != manifest.head_seq
        || version != manifest.schema_version
    {
        return Err(invalid());
    }
    if !same_restore_directories(
        &parent,
        std::ffi::OsStr::new(&stage_name),
        &stage,
        &notes,
        &state,
        &restored_config_dir,
    )? {
        return Err(invalid());
    }
    create_file(&notes, std::ffi::OsStr::new(".mdw-server.lock"))?.sync_all()?;
    create_file(&state, std::ffi::OsStr::new("server.lock"))?.sync_all()?;
    notes.sync_all()?;
    state.sync_all()?;
    if !same_inode(&stage, &open_dir(&stage_path)?)? {
        return Err(invalid());
    }
    crate::validate_pinned_backup_workspace_copy(
        &stage_path.join("notes"),
        &stage_path.join("state"),
        &notes,
        &state,
    )
    .await?;
    #[cfg(test)]
    if let Some(after_validation) = after_validation {
        after_validation(&stage_path);
    }
    if version == 9 {
        crate::Workspace::migrate_restored_copy(
            &stage_path.join("notes"),
            &stage_path.join("state"),
            &notes,
            &state,
        )
        .await?;
    }
    let path = PathBuf::from(format!("/proc/self/fd/{}/state.db", state.as_raw_fd()));
    let options = SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
    let mut db = SqliteConnection::connect_with(&options).await?;
    let mut transaction = db.begin().await?;
    let new_epoch = uuid::Uuid::new_v4().to_string();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    )?;
    let updated = sqlx::query("UPDATE workspaces SET epoch=? WHERE singleton=1 AND epoch=?")
        .bind(&new_epoch)
        .bind(&old_epoch)
        .execute(&mut *transaction)
        .await?;
    if updated.rows_affected() != 1 {
        return Err(invalid());
    }
    sqlx::query("UPDATE devices SET revoked_at=COALESCE(revoked_at,MAX(issued_at,?))")
        .bind(now)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM pairing_secrets WHERE consumed_at IS NULL")
        .execute(&mut *transaction)
        .await?;
    #[cfg(test)]
    if failpoint == Some(RestoreFailpoint::TransactionBeforeCommit) {
        return Err("restore_injected_transaction_failure".into());
    }
    transaction.commit().await?;
    #[cfg(test)]
    if failpoint == Some(RestoreFailpoint::Checkpoint) {
        return Err("restore_injected_checkpoint_failure".into());
    }
    let checkpoint: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(&mut db)
        .await?;
    if checkpoint.0 != 0 {
        return Err("restore_checkpoint_busy".into());
    }
    db.close().await?;
    #[cfg(test)]
    if failpoint == Some(RestoreFailpoint::DatabaseFsync) {
        return Err("restore_injected_database_fsync_failure".into());
    }
    open_child(&state, std::ffi::OsStr::new("state.db"), false)?.sync_all()?;
    state.sync_all()?;
    let (checked_epoch, checked_root, checked_head, checked_version) =
        database_info(&stage, &manifest.workspace_id).await?;
    if checked_epoch != new_epoch
        || checked_root != root_id
        || checked_head != head
        || checked_version != CURRENT_BACKUP_SCHEMA_VERSION
    {
        return Err(invalid());
    }
    if !same_inode(&stage, &open_dir(&stage_path)?)? {
        return Err(invalid());
    }
    crate::validate_pinned_backup_workspace_copy(
        &stage_path.join("notes"),
        &stage_path.join("state"),
        &notes,
        &state,
    )
    .await?;
    open_child(&state, std::ffi::OsStr::new("state.db"), false)?.sync_all()?;
    state.sync_all()?;
    if !same_restore_directories(
        &parent,
        std::ffi::OsStr::new(&stage_name),
        &stage,
        &notes,
        &state,
        &restored_config_dir,
    )? {
        return Err(invalid());
    }
    rfs::renameat_with(
        &restored_config_dir,
        "server.toml",
        &restored_config_dir,
        "original-server.toml",
        rfs::RenameFlags::NOREPLACE,
    )?;
    #[cfg(test)]
    write_synced(
        &restored_config_dir,
        "server.toml",
        restored_config.as_bytes(),
        false,
    )?;
    #[cfg(not(test))]
    write_synced(
        &restored_config_dir,
        "server.toml",
        restored_config.as_bytes(),
    )?;
    restored_config_dir.sync_all()?;
    rfs::unlinkat(&stage, "manifest.json", rfs::AtFlags::empty())?;
    rfs::unlinkat(&stage, "COMPLETE", rfs::AtFlags::empty())?;
    #[cfg(test)]
    if failpoint == Some(RestoreFailpoint::StageFsync) {
        return Err("restore_injected_stage_fsync_failure".into());
    }
    stage.sync_all()?;
    #[cfg(test)]
    if let Some(after_final_verification) = after_final_verification {
        after_final_verification(&stage_path);
    }
    if !same_inode(&source, &open_dir(backup)?)?
        || !same_inode(&parent, &open_dir(parent_path)?)?
        || !same_restore_directories(
            &parent,
            std::ffi::OsStr::new(&stage_name),
            &stage,
            &notes,
            &state,
            &restored_config_dir,
        )?
    {
        return Err(invalid());
    }
    #[cfg(test)]
    if failpoint == Some(RestoreFailpoint::Rename) {
        return Err("restore_injected_rename_failure".into());
    }
    rfs::renameat_with(
        &parent,
        &stage_name,
        &parent,
        name,
        rfs::RenameFlags::NOREPLACE,
    )?;
    #[cfg(test)]
    if let Some(after_publication) = after_publication {
        after_publication(&final_path);
    }
    let current_parent = open_dir(parent_path).map_err(|_| "restore_publication_indeterminate")?;
    if !same_inode(&parent, &current_parent).map_err(|_| "restore_publication_indeterminate")? {
        return Err("restore_publication_indeterminate".into());
    }
    if !same_restore_directories(&parent, name, &stage, &notes, &state, &restored_config_dir)
        .map_err(|_| "restore_publication_indeterminate")?
    {
        return Err("restore_publication_indeterminate".into());
    }
    #[cfg(test)]
    if failpoint == Some(RestoreFailpoint::ParentFsync) {
        return Err("restore_publication_indeterminate".into());
    }
    parent
        .sync_all()
        .map_err(|_| "restore_publication_indeterminate")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workspace;

    #[test]
    fn directory_depth_limit_applies_to_excluded_directories() {
        let root = tempfile::tempdir_in("/dev/shm").unwrap();
        let notes = root.path().join("notes");
        let other = root.path().join("other");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&other).unwrap();
        let mut deepest = notes.clone();
        for index in 0..64 {
            deepest.push(if index == 0 { ".obsidian" } else { "nested" });
            fs::create_dir(&deepest).unwrap();
        }
        let notes_fd = File::open(&notes).unwrap();
        let other_fd = File::open(&other).unwrap();
        assert!(!contains_directory(&notes_fd, &other_fd).unwrap());
        walk(&notes_fd, Path::new("notes"), None, None, &mut Vec::new()).unwrap();

        deepest.push("nested");
        fs::create_dir(&deepest).unwrap();
        let search_error = contains_directory(&notes_fd, &other_fd).unwrap_err();
        assert_eq!(search_error.to_string(), "backup_directory_depth_exceeded");
        let walk_error =
            walk(&notes_fd, Path::new("notes"), None, None, &mut Vec::new()).unwrap_err();
        assert_eq!(walk_error.to_string(), "backup_directory_depth_exceeded");
    }

    #[test]
    fn wide_directory_search_under_low_fd_limit() {
        const CHILD_ROOT: &str = "NOTRYX_TEST_DIRECTORY_SEARCH_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let notes = File::open(root.join("notes")).unwrap();
            let other = File::open(root.join("other")).unwrap();
            assert!(!contains_directory(&notes, &other).unwrap());
            return;
        }

        let root = tempfile::tempdir_in("/dev/shm").unwrap();
        fs::create_dir(root.path().join("notes")).unwrap();
        fs::create_dir(root.path().join("other")).unwrap();
        for index in 0..128 {
            fs::create_dir(
                root.path()
                    .join("notes")
                    .join(format!("directory-{index:03}")),
            )
            .unwrap();
        }
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", "ulimit -n 64; exec \"$@\"", "sh"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cold_backup::tests::wide_directory_search_under_low_fd_limit",
            ])
            .env(CHILD_ROOT, root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn scratch_cleanup_rejects_unexpected_entries_without_removing_anything() {
        let root = tempfile::tempdir().unwrap();
        let stage = File::open(root.path()).unwrap();
        let scratch = create_dir(&stage, std::ffi::OsStr::new(".verification")).unwrap();
        fs::write(root.path().join(".verification/state.db"), b"database").unwrap();
        fs::write(root.path().join(".verification/keep"), b"sentinel").unwrap();

        assert!(cleanup_verification(&stage, &scratch).is_err());
        assert_eq!(
            fs::read(root.path().join(".verification/state.db")).unwrap(),
            b"database"
        );
        assert_eq!(
            fs::read(root.path().join(".verification/keep")).unwrap(),
            b"sentinel"
        );
    }

    #[test]
    fn scratch_cleanup_rejects_replaced_directory_without_touching_replacement() {
        let root = tempfile::tempdir().unwrap();
        let stage = File::open(root.path()).unwrap();
        let scratch = create_dir(&stage, std::ffi::OsStr::new(".verification")).unwrap();
        fs::write(root.path().join(".verification/state.db"), b"database").unwrap();
        fs::rename(
            root.path().join(".verification"),
            root.path().join("old-scratch"),
        )
        .unwrap();
        fs::create_dir(root.path().join(".verification")).unwrap();
        fs::write(root.path().join(".verification/keep"), b"replacement").unwrap();

        assert!(cleanup_verification(&stage, &scratch).is_err());
        assert_eq!(
            fs::read(root.path().join(".verification/keep")).unwrap(),
            b"replacement"
        );
        assert!(!root.path().join("old-scratch/state.db").exists());
    }

    async fn fixture() -> (tempfile::TempDir, Config, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let notes = root.path().join("notes");
        let state = root.path().join("state");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&state).unwrap();
        let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
        fs::write(notes.join("note.md"), b"before\n").unwrap();
        workspace.adopt_root_document("note.md").await.unwrap();
        workspace.close().await.unwrap();
        let config_path = root.path().join("server.toml");
        let config_text = format!(
            "[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n",
            root.path().join("admin.sock").display(),
            notes.display(),
            state.display()
        );
        fs::write(&config_path, &config_text).unwrap();
        let config = Config::from_toml(&config_text).unwrap();
        let destination = root.path().join("output");
        fs::create_dir(&destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
        (root, config, config_path, destination)
    }

    async fn synthetic_v9_backup(backup: &Path) {
        let db_path = backup.join("state/state.db");
        assert!(!backup.join("state/state.db-wal").exists());
        assert!(!backup.join("state/state.db-shm").exists());
        let replacement = backup.join("state/synthetic-v9.db");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&replacement)
                .create_if_missing(true)
                .foreign_keys(false),
        )
        .await
        .unwrap();
        sqlx::raw_sql(include_str!("../tests/fixtures/server_schema_v9.sql"))
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::query("ATTACH DATABASE ? AS source")
            .bind(db_path.to_str().unwrap())
            .execute(&mut db)
            .await
            .unwrap();
        for table in [
            "workspaces",
            "nodes",
            "change_sets",
            "node_changes",
            "operation_streams",
            "operation_receipts",
            "write_intents",
            "principals",
            "devices",
            "pairing_secrets",
            "retired_operations",
            "recovery_cases",
            "recovery_objects",
            "uploads",
        ] {
            let query = format!("PRAGMA main.table_info({table})");
            let rows = sqlx::query(sqlx::AssertSqlSafe(query))
                .fetch_all(&mut db)
                .await
                .unwrap();
            let names = rows
                .iter()
                .map(|row| {
                    let name: String = row.try_get("name").unwrap();
                    assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
                    format!("\"{name}\"")
                })
                .collect::<Vec<_>>()
                .join(",");
            let query =
                format!("INSERT INTO main.{table}({names}) SELECT {names} FROM source.{table}");
            sqlx::query(sqlx::AssertSqlSafe(query))
                .execute(&mut db)
                .await
                .unwrap();
        }
        sqlx::query("DETACH DATABASE source")
            .execute(&mut db)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("PRAGMA user_version")
                .fetch_one(&mut db)
                .await
                .unwrap(),
            9
        );
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&mut db)
                .await
                .unwrap()
                .is_empty()
        );
        db.close().await.unwrap();
        let mode = fs::metadata(&db_path).unwrap().permissions();
        fs::set_permissions(&replacement, mode).unwrap();
        fs::rename(&replacement, &db_path).unwrap();
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(backup.join("manifest.json")).unwrap()).unwrap();
        manifest.schema_version = 9;
        let db_bytes = fs::read(&db_path).unwrap();
        let entry = manifest
            .entries
            .iter_mut()
            .find(|entry| entry.path == encode_path(Path::new("state/state.db")))
            .unwrap();
        entry.size = db_bytes.len() as u64;
        entry.sha256 = Some(hex(&Sha256::digest(&db_bytes)));
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(backup.join("manifest.json"), &bytes).unwrap();
        fs::write(backup.join("COMPLETE"), hex(&Sha256::digest(&bytes))).unwrap();
        assert!(verify(backup).is_ok());
    }

    fn copy_synthetic_tree(source: &Path, destination: &Path) {
        fs::create_dir(destination).unwrap();
        fs::set_permissions(destination, fs::metadata(source).unwrap().permissions()).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_synthetic_tree(&entry.path(), &target);
            } else {
                assert!(entry.file_type().unwrap().is_file());
                fs::copy(entry.path(), &target).unwrap();
                fs::set_permissions(target, entry.metadata().unwrap().permissions()).unwrap();
            }
        }
    }

    fn synthetic_tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    walk(root, &path, result);
                } else {
                    result.insert(
                        path.strip_prefix(root).unwrap().into(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut result = BTreeMap::new();
        walk(root, root, &mut result);
        result
    }

    #[tokio::test]
    async fn v9_restore_rejects_replaced_stage_state_before_migration() {
        let (root, config, config_path, parent) = fixture().await;
        let backup = parent.join("v9-backup");
        create(&config, &config_path, &backup).await.unwrap();
        synthetic_v9_backup(&backup).await;
        let backup_before = synthetic_tree_bytes(&backup);
        let notes_before = fs::read(config.workspace.notes_root.join("note.md")).unwrap();
        let source_before = fs::read(config.workspace.state_root.join("state.db")).unwrap();
        let destination = parent.join("restored");
        let replacement = root.path().join("replacement-state");
        let old_stage_state = root.path().join("old-stage-state");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed_in_hook = observed.clone();
        let result = restore_inner(
            &backup,
            &destination,
            None,
            None,
            Some(Box::new(move |stage| {
                let state = stage.join("state");
                let original_state = synthetic_tree_bytes(&state);
                copy_synthetic_tree(&state, &replacement);
                let original_replacement = synthetic_tree_bytes(&replacement);
                fs::rename(&state, &old_stage_state).unwrap();
                fs::rename(&replacement, &state).unwrap();
                *observed_in_hook.lock().unwrap() =
                    Some((state, original_replacement, old_stage_state, original_state));
            })),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(!destination.exists());
        let (replacement_state, before, original_state, original_before) = observed
            .lock()
            .unwrap()
            .take()
            .expect("validation hook must run");
        assert!(
            synthetic_tree_bytes(&replacement_state) == before,
            "replacement state changed"
        );
        assert!(
            synthetic_tree_bytes(&original_state) == original_before,
            "original stage state changed"
        );
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(replacement_state.join("state.db"))
                .read_only(true),
        )
        .await
        .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("PRAGMA user_version")
                .fetch_one(&mut db)
                .await
                .unwrap(),
            9
        );
        db.close().await.unwrap();
        assert!(
            synthetic_tree_bytes(&backup) == backup_before,
            "backup changed"
        );
        assert!(
            fs::read(config.workspace.notes_root.join("note.md")).unwrap() == notes_before,
            "source note changed"
        );
        assert!(
            fs::read(config.workspace.state_root.join("state.db")).unwrap() == source_before,
            "source database changed"
        );
    }

    #[tokio::test]
    async fn restore_rejects_replaced_notes_before_publication() {
        let (root, config, config_path, parent) = fixture().await;
        let excluded = config.workspace.notes_root.join(".obsidian");
        fs::create_dir(&excluded).unwrap();
        fs::write(excluded.join("workspace.json"), b"synthetic original\n").unwrap();
        let backup = parent.join("backup");
        create(&config, &config_path, &backup).await.unwrap();
        let backup_before = synthetic_tree_bytes(&backup);
        let source_before = synthetic_tree_bytes(&config.workspace.notes_root);
        let destination = parent.join("restored");
        let replacement = root.path().join("replacement-notes");
        let old_stage_notes = root.path().join("old-stage-notes");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed_in_hook = observed.clone();
        let result = restore_inner(
            &backup,
            &destination,
            None,
            None,
            None,
            Some(Box::new(move |stage| {
                let notes = stage.join("notes");
                let original_before = synthetic_tree_bytes(&notes);
                copy_synthetic_tree(&notes, &replacement);
                fs::write(
                    replacement.join(".obsidian/workspace.json"),
                    b"synthetic replacement\n",
                )
                .unwrap();
                let replacement_before = synthetic_tree_bytes(&replacement);
                fs::rename(&notes, &old_stage_notes).unwrap();
                fs::rename(&replacement, &notes).unwrap();
                *observed_in_hook.lock().unwrap() =
                    Some((notes, replacement_before, old_stage_notes, original_before));
            })),
            None,
            None,
        )
        .await;
        assert!(
            result.is_err(),
            "restore published after replacing verified notes"
        );
        assert!(!destination.exists());
        let (replacement_notes, replacement_before, original_notes, original_before) = observed
            .lock()
            .unwrap()
            .take()
            .expect("final verification hook must run");
        assert!(
            synthetic_tree_bytes(&replacement_notes) == replacement_before,
            "replacement notes changed"
        );
        assert!(
            synthetic_tree_bytes(&original_notes) == original_before,
            "original stage notes changed"
        );
        assert!(
            synthetic_tree_bytes(&backup) == backup_before,
            "backup changed"
        );
        assert!(
            synthetic_tree_bytes(&config.workspace.notes_root) == source_before,
            "source notes changed"
        );
    }

    #[tokio::test]
    async fn restore_rejects_config_replaced_after_manifest_verification() {
        let (root, config, config_path, parent) = fixture().await;
        let backup = parent.join("backup");
        create(&config, &config_path, &backup).await.unwrap();
        let control = parent.join("control-restored");
        restore(&backup, &control).await.unwrap();
        let backup_before = synthetic_tree_bytes(&backup);
        let source_before = synthetic_tree_bytes(&config.workspace.notes_root);
        let destination = parent.join("restored");
        let replacement = root.path().join("replacement-config");
        let old_stage_config = root.path().join("old-stage-config");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed_in_hook = observed.clone();
        let result = restore_inner(
            &backup,
            &destination,
            None,
            None,
            None,
            None,
            None,
            Some(Box::new(move |stage| {
                let config = stage.join("config");
                let original_before = synthetic_tree_bytes(&config);
                copy_synthetic_tree(&config, &replacement);
                let replacement_file = replacement.join("server.toml");
                let mut text = fs::read_to_string(&replacement_file).unwrap();
                text.push_str("\n# synthetic replacement\n");
                Config::from_toml(&text).unwrap();
                fs::write(&replacement_file, text).unwrap();
                let replacement_before = synthetic_tree_bytes(&replacement);
                fs::rename(&config, &old_stage_config).unwrap();
                fs::rename(&replacement, &config).unwrap();
                *observed_in_hook.lock().unwrap() = Some((
                    config,
                    replacement_before,
                    old_stage_config,
                    original_before,
                ));
            })),
        )
        .await;
        assert!(
            result.is_err(),
            "restore published after replacing verified config"
        );
        assert!(!destination.exists());
        let (replacement_config, replacement_before, original_config, original_before) = observed
            .lock()
            .unwrap()
            .take()
            .expect("manifest verification hook must run");
        assert!(
            synthetic_tree_bytes(&replacement_config) == replacement_before,
            "replacement config changed"
        );
        assert!(
            synthetic_tree_bytes(&original_config) == original_before,
            "original stage config changed"
        );
        assert!(
            synthetic_tree_bytes(&backup) == backup_before,
            "backup changed"
        );
        assert!(
            synthetic_tree_bytes(&config.workspace.notes_root) == source_before,
            "source notes changed"
        );
    }

    #[tokio::test]
    async fn restore_reports_indeterminate_if_notes_are_replaced_after_publication() {
        let (root, config, config_path, parent) = fixture().await;
        let excluded = config.workspace.notes_root.join(".obsidian");
        fs::create_dir(&excluded).unwrap();
        fs::write(excluded.join("workspace.json"), b"synthetic original\n").unwrap();
        let backup = parent.join("backup");
        create(&config, &config_path, &backup).await.unwrap();
        let backup_before = synthetic_tree_bytes(&backup);
        let source_before = synthetic_tree_bytes(&config.workspace.notes_root);
        let destination = parent.join("restored");
        let replacement = root.path().join("post-publication-replacement");
        let old_notes = root.path().join("published-original-notes");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed_in_hook = observed.clone();
        let result = restore_inner(
            &backup,
            &destination,
            None,
            None,
            None,
            None,
            Some(Box::new(move |published| {
                let notes = published.join("notes");
                let original_before = synthetic_tree_bytes(&notes);
                copy_synthetic_tree(&notes, &replacement);
                fs::write(
                    replacement.join(".obsidian/workspace.json"),
                    b"synthetic replacement\n",
                )
                .unwrap();
                let replacement_before = synthetic_tree_bytes(&replacement);
                fs::rename(&notes, &old_notes).unwrap();
                fs::rename(&replacement, &notes).unwrap();
                *observed_in_hook.lock().unwrap() =
                    Some((notes, replacement_before, old_notes, original_before));
            })),
            None,
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "restore_publication_indeterminate"
        );
        assert!(destination.exists());
        let (replacement_notes, replacement_before, original_notes, original_before) = observed
            .lock()
            .unwrap()
            .take()
            .expect("publication hook must run");
        assert!(
            synthetic_tree_bytes(&replacement_notes) == replacement_before,
            "replacement notes changed"
        );
        assert!(
            synthetic_tree_bytes(&original_notes) == original_before,
            "original published notes changed"
        );
        assert!(
            synthetic_tree_bytes(&backup) == backup_before,
            "backup changed"
        );
        assert!(
            synthetic_tree_bytes(&config.workspace.notes_root) == source_before,
            "source notes changed"
        );
    }

    #[tokio::test]
    async fn restore_reports_indeterminate_if_parent_is_replaced_after_publication() {
        let (root, config, config_path, parent) = fixture().await;
        let backup = parent.join("backup");
        create(&config, &config_path, &backup).await.unwrap();
        let backup_before = synthetic_tree_bytes(&backup);
        let source_before = synthetic_tree_bytes(&config.workspace.notes_root);
        let destination = parent.join("restored");
        let moved_parent = root.path().join("moved-output");
        let old_parent = parent.clone();
        let moved_in_hook = moved_parent.clone();
        let result = restore_inner(
            &backup,
            &destination,
            None,
            None,
            None,
            None,
            Some(Box::new(move |_| {
                fs::rename(&old_parent, &moved_in_hook).unwrap();
                fs::create_dir(&old_parent).unwrap();
                fs::set_permissions(&old_parent, fs::Permissions::from_mode(0o700)).unwrap();
                fs::write(old_parent.join("synthetic-sentinel"), b"untouched\n").unwrap();
            })),
            None,
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "restore_publication_indeterminate"
        );
        assert!(!destination.exists());
        assert!(moved_parent.join("restored").is_dir());
        assert!(fs::read(parent.join("synthetic-sentinel")).unwrap() == b"untouched\n");
        assert!(
            synthetic_tree_bytes(&moved_parent.join("backup")) == backup_before,
            "backup changed"
        );
        assert!(
            synthetic_tree_bytes(&config.workspace.notes_root) == source_before,
            "source notes changed"
        );
    }

    #[tokio::test]
    async fn backup_manifest_size_limit_matches_reader_boundary() {
        let (_root, config, config_path, parent) = fixture().await;
        let reference = parent.join("reference");
        create(&config, &config_path, &reference).await.unwrap();
        let manifest = fs::read(reference.join("manifest.json")).unwrap();
        let length = u64::try_from(manifest.len()).unwrap();
        assert!(length < MAX_MANIFEST_BYTES);
        assert_eq!(
            read_bounded(&File::open(&reference).unwrap(), "manifest.json", length).unwrap(),
            manifest
        );
        assert!(
            read_bounded(
                &File::open(&reference).unwrap(),
                "manifest.json",
                length - 1
            )
            .is_err()
        );

        let exact = parent.join("exact");
        create_with_manifest_limit(&config, &config_path, &exact, None, None, Some(length))
            .await
            .unwrap();
        assert!(verify(&exact).is_ok());
        let restored = parent.join("restored");
        restore(&exact, &restored).await.unwrap();
        assert_eq!(
            fs::read(restored.join("notes/note.md")).unwrap(),
            b"before\n"
        );
        let over = parent.join("over");
        let source_db = fs::read(config.workspace.state_root.join("state.db")).unwrap();
        assert!(
            create_with_manifest_limit(&config, &config_path, &over, None, None, Some(length - 1))
                .await
                .is_err()
        );
        assert!(!over.exists());
        assert_eq!(
            fs::read(config.workspace.state_root.join("state.db")).unwrap(),
            source_db
        );
        assert!(verify(&reference).is_ok());
    }

    #[tokio::test]
    async fn backup_rejects_missing_active_upload_without_touching_source() {
        let (_root, config, config_path, parent) = fixture().await;
        let mut workspace =
            Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
                .await
                .unwrap();
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let status = workspace.auth_status().await.unwrap();
        let grant = workspace
            .issue_pairing(status.owner_principal_id, now)
            .await
            .unwrap();
        let device = workspace
            .pair(
                notryx_protocol::PairRequest {
                    protocol: notryx_protocol::CONTRACT.into(),
                    workspace_id: grant.workspace_id,
                    pairing_secret: grant.pairing_secret,
                },
                now,
            )
            .await
            .unwrap();
        workspace.close().await.unwrap();
        let db_path = config.workspace.state_root.join("state.db");
        let mut db =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&db_path))
                .await
                .unwrap();
        let upload_id = uuid::Uuid::new_v4();
        let hash = hex(&Sha256::digest(b"ok"));
        sqlx::query("INSERT INTO uploads(upload_id,device_id,status,expected_hash,declared_size,reserved_bytes,started_at,expires_at,content_hash,size) VALUES(?,?,'ready',?,2,2,?,?,?,2)")
            .bind(upload_id.to_string()).bind(device.device_id).bind(&hash)
            .bind(now).bind(now + 3600).bind(&hash).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let before = fs::read(&db_path).unwrap();
        let destination = parent.join("backup");
        assert!(create(&config, &config_path, &destination).await.is_err());
        assert!(!destination.exists());
        let staged = config
            .workspace
            .state_root
            .join("staging")
            .join(format!("upload-{upload_id}.ready"));
        for corrupted in [b"longer".as_slice(), b"no".as_slice()] {
            fs::write(&staged, corrupted).unwrap();
            assert!(create(&config, &config_path, &destination).await.is_err());
            assert!(!destination.exists());
            assert_eq!(fs::read(&staged).unwrap(), corrupted);
        }
        fs::write(&staged, b"ok").unwrap();
        create(&config, &config_path, &destination).await.unwrap();
        assert!(verify(&destination).is_ok());
        assert_eq!(fs::read(db_path).unwrap(), before);
        assert_eq!(
            fs::read(config.workspace.notes_root.join("note.md")).unwrap(),
            b"before\n"
        );
    }

    async fn backup_rejects_unsafe_orphan_upload(is_directory: bool) {
        let (_root, config, config_path, parent) = fixture().await;
        let staged = config
            .workspace
            .state_root
            .join("staging")
            .join(format!("upload-{}.ready", uuid::Uuid::new_v4()));
        if is_directory {
            fs::create_dir(&staged).unwrap();
        } else {
            std::os::unix::fs::symlink("outside", &staged).unwrap();
        }
        let database = config.workspace.state_root.join("state.db");
        let before = fs::read(&database).unwrap();
        let destination = parent.join("backup");
        assert!(create(&config, &config_path, &destination).await.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read(database).unwrap(), before);
        let metadata = fs::symlink_metadata(staged).unwrap();
        assert!(if is_directory {
            metadata.is_dir()
        } else {
            metadata.file_type().is_symlink()
        });
    }

    #[tokio::test]
    async fn backup_rejects_orphan_upload_symlink() {
        backup_rejects_unsafe_orphan_upload(false).await;
    }

    #[tokio::test]
    async fn backup_rejects_orphan_upload_directory() {
        backup_rejects_unsafe_orphan_upload(true).await;
    }

    async fn failed_restore_does_not_modify_backup_or_source(point: RestoreFailpoint) {
        let (_root, config, config_path, parent) = fixture().await;
        let backup = parent.join("backup");
        create(&config, &config_path, &backup).await.unwrap();
        let original_db = fs::read(config.workspace.state_root.join("state.db")).unwrap();
        let backup_db = fs::read(backup.join("state/state.db")).unwrap();
        let original_notes = fs::read(config.workspace.notes_root.join("note.md")).unwrap();
        let destination = parent.join("restored");
        let error = restore_inner(
            &backup,
            &destination,
            Some(point),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        if point == RestoreFailpoint::ParentFsync {
            assert_eq!(error.to_string(), "restore_publication_indeterminate");
            assert!(destination.is_dir());
        } else {
            assert!(!destination.exists());
        }
        assert_eq!(
            fs::read(config.workspace.state_root.join("state.db")).unwrap(),
            original_db
        );
        assert_eq!(
            fs::read(config.workspace.notes_root.join("note.md")).unwrap(),
            original_notes
        );
        assert_eq!(fs::read(backup.join("state/state.db")).unwrap(), backup_db);
        assert!(verify(&backup).is_ok());
    }

    #[tokio::test]
    async fn restore_transaction_failure_rolls_back_epoch_and_credentials_together() {
        let (_root, config, config_path, parent) = fixture().await;
        let mut workspace =
            Workspace::open(&config.workspace.notes_root, &config.workspace.state_root)
                .await
                .unwrap();
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let status = workspace.auth_status().await.unwrap();
        let grant = workspace
            .issue_pairing(status.owner_principal_id, now)
            .await
            .unwrap();
        let device = workspace
            .pair(
                notryx_protocol::PairRequest {
                    protocol: notryx_protocol::CONTRACT.into(),
                    workspace_id: grant.workspace_id,
                    pairing_secret: grant.pairing_secret,
                },
                now,
            )
            .await
            .unwrap();
        workspace
            .issue_pairing(status.owner_principal_id, now)
            .await
            .unwrap();
        workspace.close().await.unwrap();
        let backup = parent.join("backup");
        create(&config, &config_path, &backup).await.unwrap();
        let destination = parent.join("restored");
        assert!(
            restore_inner(
                &backup,
                &destination,
                Some(RestoreFailpoint::TransactionBeforeCommit),
                None,
                None,
                None,
                None,
                None
            )
            .await
            .is_err()
        );
        assert!(!destination.exists());
        let stage = fs::read_dir(&parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .as_bytes()
                    .starts_with(b".notryx-restore-")
            })
            .unwrap();
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(stage.join("state/state.db"))
                .read_only(true),
        )
        .await
        .unwrap();
        let epoch: String = sqlx::query_scalar("SELECT epoch FROM workspaces WHERE singleton=1")
            .fetch_one(&mut db)
            .await
            .unwrap();
        let revoked: Option<i64> =
            sqlx::query_scalar("SELECT revoked_at FROM devices WHERE device_id=?")
                .bind(device.device_id)
                .fetch_one(&mut db)
                .await
                .unwrap();
        let unused: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pairing_secrets WHERE consumed_at IS NULL")
                .fetch_one(&mut db)
                .await
                .unwrap();
        assert_eq!(epoch, status.workspace_epoch.to_string());
        assert_eq!(revoked, None);
        assert_eq!(unused, 1);
        db.close().await.unwrap();
        assert!(verify(&backup).is_ok());
    }

    macro_rules! restore_failure_test {
        ($name:ident, $point:expr) => {
            #[tokio::test]
            async fn $name() {
                failed_restore_does_not_modify_backup_or_source($point).await;
            }
        };
    }

    restore_failure_test!(restore_copy_fsync_failure, RestoreFailpoint::CopyFsync);
    restore_failure_test!(
        restore_transaction_failure,
        RestoreFailpoint::TransactionBeforeCommit
    );
    restore_failure_test!(restore_checkpoint_failure, RestoreFailpoint::Checkpoint);
    restore_failure_test!(
        restore_database_fsync_failure,
        RestoreFailpoint::DatabaseFsync
    );
    restore_failure_test!(restore_stage_fsync_failure, RestoreFailpoint::StageFsync);
    restore_failure_test!(restore_rename_failure, RestoreFailpoint::Rename);
    restore_failure_test!(
        restore_parent_fsync_failure_is_indeterminate,
        RestoreFailpoint::ParentFsync
    );

    #[tokio::test]
    async fn restore_rejects_parent_replacement_without_touching_new_parent() {
        let (root, config, config_path, parent) = fixture().await;
        let backup = parent.join("backup");
        create(&config, &config_path, &backup).await.unwrap();
        let old_parent = root.path().join("old-output");
        let new_parent = parent.clone();
        let moved_parent = old_parent.clone();
        let destination = parent.join("restored");
        let result = restore_inner(
            &backup,
            &destination,
            None,
            Some(Box::new(move || {
                fs::rename(&new_parent, &moved_parent).unwrap();
                fs::create_dir(&new_parent).unwrap();
                fs::write(new_parent.join("sentinel"), b"unchanged").unwrap();
            })),
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read(parent.join("sentinel")).unwrap(), b"unchanged");
        assert!(verify(&old_parent.join("backup")).is_ok());
        assert_eq!(
            fs::read(config.workspace.notes_root.join("note.md")).unwrap(),
            b"before\n"
        );
    }

    #[tokio::test]
    async fn verification_rejects_backup_while_publication_lock_is_held() {
        let (_root, config, config_path, destination_parent) = fixture().await;
        let destination = destination_parent.join("backup");
        create(&config, &config_path, &destination).await.unwrap();
        let locked = File::open(&destination).unwrap();
        rfs::flock(&locked, rfs::FlockOperation::NonBlockingLockExclusive).unwrap();

        assert!(verify(&destination).is_err());
        drop(locked);
        assert!(verify(&destination).is_ok());
    }

    #[tokio::test]
    async fn failed_marker_cleanup_reports_indeterminate_publication() {
        let (_root, config, config_path, destination_parent) = fixture().await;
        let destination = destination_parent.join("backup");
        let error = create_inner(
            &config,
            &config_path,
            &destination,
            None,
            Some(BackupFailpoint::MarkerCleanupFailure),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "backup_publication_indeterminate");
        assert!(verify(&destination).is_ok());
    }

    #[tokio::test]
    async fn source_root_replacement_is_rejected_without_copying_replacement() {
        let (root, config, config_path, destination_parent) = fixture().await;
        let notes = config.workspace.notes_root.clone();
        let old_notes = root.path().join("old-notes");
        let destination = destination_parent.join("backup");
        let source_db = fs::read(config.workspace.state_root.join("state.db")).unwrap();
        let old_path = old_notes.clone();
        let new_path = notes.clone();
        let result = create_inner(
            &config,
            &config_path,
            &destination,
            Some(Box::new(move || {
                fs::rename(&new_path, &old_path).unwrap();
                fs::create_dir(&new_path).unwrap();
                fs::write(new_path.join("keep"), b"replacement\n").unwrap();
            })),
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read(notes.join("keep")).unwrap(), b"replacement\n");
        assert_eq!(fs::read(old_notes.join("note.md")).unwrap(), b"before\n");
        assert_eq!(
            fs::read(config.workspace.state_root.join("state.db")).unwrap(),
            source_db
        );
    }

    #[tokio::test]
    async fn destination_parent_replacement_is_rejected_without_touching_new_parent() {
        let (root, config, config_path, destination_parent) = fixture().await;
        let original_parent = root.path().join("old-output");
        let destination = destination_parent.join("backup");
        let old_path = original_parent.clone();
        let new_path = destination_parent.clone();
        let result = create_inner(
            &config,
            &config_path,
            &destination,
            Some(Box::new(move || {
                fs::rename(&new_path, &old_path).unwrap();
                fs::create_dir(&new_path).unwrap();
                fs::write(new_path.join("keep"), b"replacement\n").unwrap();
            })),
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(
            fs::read(destination_parent.join("keep")).unwrap(),
            b"replacement\n"
        );
        assert_eq!(
            fs::read(config.workspace.notes_root.join("note.md")).unwrap(),
            b"before\n"
        );
    }

    async fn failed_publish_is_never_verified(point: BackupFailpoint) {
        let (_root, config, config_path, destination_parent) = fixture().await;
        let destination = destination_parent.join("backup");
        let source_db = fs::read(config.workspace.state_root.join("state.db")).unwrap();
        let result = create_inner(&config, &config_path, &destination, None, Some(point)).await;
        assert!(result.is_err());
        if destination.exists() {
            assert!(!destination.join("COMPLETE").exists());
            assert!(verify(&destination).is_err());
        }
        for item in fs::read_dir(&destination_parent).unwrap() {
            let path = item.unwrap().path();
            if path
                .file_name()
                .unwrap()
                .as_bytes()
                .ends_with(b".incomplete")
            {
                assert!(!path.join("COMPLETE").exists());
                assert!(verify(&path).is_err());
            }
        }
        assert_eq!(
            fs::read(config.workspace.notes_root.join("note.md")).unwrap(),
            b"before\n"
        );
        assert_eq!(
            fs::read(config.workspace.state_root.join("state.db")).unwrap(),
            source_db
        );
    }

    macro_rules! failure_test {
        ($name:ident, $point:expr) => {
            #[tokio::test]
            async fn $name() {
                failed_publish_is_never_verified($point).await;
            }
        };
    }

    failure_test!(
        manifest_fsync_failure_is_incomplete,
        BackupFailpoint::ManifestFsync
    );
    failure_test!(
        stage_fsync_failure_is_incomplete,
        BackupFailpoint::StageFsync
    );
    failure_test!(rename_failure_is_incomplete, BackupFailpoint::Rename);
    failure_test!(
        parent_fsync_failure_is_incomplete,
        BackupFailpoint::ParentFsync
    );
    failure_test!(
        marker_fsync_failure_is_incomplete,
        BackupFailpoint::MarkerFsync
    );
    failure_test!(
        marker_rename_failure_is_incomplete,
        BackupFailpoint::MarkerRename
    );
    failure_test!(
        final_directory_fsync_failure_is_incomplete,
        BackupFailpoint::CompletedDirectoryFsync
    );
}
