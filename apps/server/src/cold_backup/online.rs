use super::*;
use crate::runtime::{RuntimeError, WorkspaceHandle, backup::Snapshot};
use notryx_protocol::AdminResult;
use std::ffi::OsStr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const JOB_LIMIT: Duration = Duration::from_secs(30 * 60);

fn mapped(error: Box<dyn std::error::Error>) -> RuntimeError {
    let code = match error.to_string().as_str() {
        "backup_target_exists" => "backup_target_exists",
        "backup_timeout" => "backup_timeout",
        "backup_cancelled" => "backup_cancelled",
        "backup_manifest_too_large" => "backup_manifest_too_large",
        "backup_recovery_required" => "backup_recovery_required",
        "backup_insufficient_space" => "backup_insufficient_space",
        "backup_publication_indeterminate" => "backup_publication_indeterminate",
        _ => "backup_failed",
    };
    RuntimeError::Backup(code)
}

#[derive(Clone)]
struct JobControl {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn check_time(control: &JobControl) -> BackupResult<()> {
    if control.cancelled.load(Ordering::Acquire) {
        return Err("backup_cancelled".into());
    }
    if Instant::now() >= control.deadline {
        return Err("backup_timeout".into());
    }
    Ok(())
}

fn write(root: &File, name: &str, bytes: &[u8]) -> BackupResult<()> {
    #[cfg(test)]
    {
        write_synced(root, name, bytes, false)
    }
    #[cfg(not(test))]
    {
        write_synced(root, name, bytes)
    }
}

struct Prepared {
    config: Config,
    config_bytes: Vec<u8>,
    destination: PathBuf,
    parent: File,
    stage: File,
    stage_name: String,
    state: File,
}

impl Prepared {
    fn new(config_path: &Path, destination: &Path) -> BackupResult<Self> {
        if !config_path.is_absolute() || !destination.is_absolute() {
            return Err(invalid());
        }
        let config_parent = open_dir(config_path.parent().ok_or_else(invalid)?)?;
        let mut source = open_child(
            &config_parent,
            config_path.file_name().ok_or_else(invalid)?,
            false,
        )?;
        if !source.metadata()?.is_file() {
            return Err(invalid());
        }
        let mut config_bytes = Vec::new();
        (&mut source).take(65537).read_to_end(&mut config_bytes)?;
        let config = Config::from_toml(std::str::from_utf8(&config_bytes)?)?;
        let parent_path = destination.parent().ok_or_else(invalid)?;
        let parent = open_dir(parent_path)?;
        check_private_parent(&parent)?;
        let name = destination.file_name().ok_or_else(invalid)?;
        if name.as_bytes().starts_with(b".notryx-backup-") {
            return Err(invalid());
        }
        match rfs::statat(&parent, name, rfs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => return Err("backup_target_exists".into()),
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let notes = open_dir(&config.workspace.notes_root)?;
        let state_source = open_dir(&config.workspace.state_root)?;
        if contains_directory(&notes, &parent)? || contains_directory(&state_source, &parent)? {
            return Err(invalid());
        }
        let source_db = open_child(&state_source, OsStr::new("state.db"), false)?;
        check_space(&parent, source_db.metadata()?.len().saturating_mul(3))?;
        let stage_name = format!(".notryx-backup-{}.incomplete", uuid::Uuid::new_v4());
        let stage = create_dir(&parent, OsStr::new(&stage_name))?;
        rfs::flock(&stage, rfs::FlockOperation::NonBlockingLockExclusive)?;
        let state = create_dir(&stage, OsStr::new("state"))?;
        Ok(Self {
            config,
            config_bytes,
            destination: destination.to_owned(),
            parent,
            stage,
            stage_name,
            state,
        })
    }

    async fn finish(self, snapshot: Snapshot, control: JobControl) -> BackupResult<AdminResult> {
        check_time(&control)?;
        if !same_inode(
            &snapshot.notes,
            &open_dir(&self.config.workspace.notes_root)?,
        )? || !same_inode(
            &snapshot.state,
            &open_dir(&self.config.workspace.state_root)?,
        )? {
            return Err(invalid());
        }
        let source_objects_parent = open_child(&snapshot.state, OsStr::new("objects"), true)?;
        let source_objects = open_child(&source_objects_parent, OsStr::new("sha256"), true)?;
        let objects_parent = create_dir(&self.state, OsStr::new("objects"))?;
        let objects = create_dir(&objects_parent, OsStr::new("sha256"))?;
        let staging = create_dir(&self.state, OsStr::new("staging"))?;
        let notes = create_dir(&self.stage, OsStr::new("notes"))?;
        let config = create_dir(&self.stage, OsStr::new("config"))?;
        write(&config, "server.toml", &self.config_bytes)?;
        config.sync_all()?;
        write(
            &notes,
            ".mdw-workspace.json",
            &serde_json::to_vec(&serde_json::json!({
                "format_version": 1, "workspace_id": snapshot.workspace_id,
            }))?,
        )?;
        let db_path = PathBuf::from(format!("/proc/self/fd/{}/state.db", self.state.as_raw_fd()));
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .read_only(true),
        )
        .await?;
        let hashes: Vec<String> = sqlx::query_scalar("SELECT content_hash FROM nodes WHERE content_hash IS NOT NULL UNION SELECT hash FROM recovery_objects")
            .fetch_all(&mut db).await?;
        let mut required = open_child(&self.state, OsStr::new("state.db"), false)?
            .metadata()?
            .len()
            .saturating_mul(3);
        for hash in &hashes {
            validate_hash(hash)?;
            required = required
                .checked_add(
                    open_child(&source_objects, OsStr::new(hash), false)?
                        .metadata()?
                        .len(),
                )
                .ok_or_else(invalid)?;
        }
        let sizes: Vec<String> =
            sqlx::query_scalar("SELECT size FROM nodes WHERE kind='file' AND deleted=0")
                .fetch_all(&mut db)
                .await?;
        for size in sizes {
            required = required.checked_add(size.parse()?).ok_or_else(invalid)?;
        }
        for (_, source) in &snapshot.ready_uploads {
            required = required
                .checked_add(source.metadata()?.len())
                .ok_or_else(invalid)?;
        }
        check_space(&self.parent, required)?;
        for hash in hashes {
            let mut source = open_child(&source_objects, OsStr::new(&hash), false)?;
            copy(&mut source, &objects, &hash, Some(&hash), &control)?;
        }
        for (name, mut source) in snapshot.ready_uploads {
            check_time(&control)?;
            copy(&mut source, &staging, &name, None, &control)?;
        }
        materialize_notes(&mut db, &notes, &objects, &control).await?;
        db.close().await?;
        for directory in [&objects, &objects_parent, &staging, &self.state, &notes] {
            directory.sync_all()?;
        }
        let (epoch, root_id, head_seq, schema_version) =
            database_info(&self.stage, &snapshot.workspace_id).await?;
        check_time(&control)?;
        let mut entries = Vec::new();
        for name in ["notes", "state", "config"] {
            let root = open_child(&self.stage, OsStr::new(name), true)?;
            entries.push(Entry {
                path: encode_path(Path::new(name)),
                kind: "directory".into(),
                mode: 0o700,
                size: 0,
                sha256: None,
            });
            walk(&root, Path::new(name), None, None, &mut entries)?;
            check_time(&control)?;
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let manifest = Manifest {
            format: 1,
            workspace_id: snapshot.workspace_id,
            epoch,
            root_id,
            head_seq: head_seq.clone(),
            schema_version,
            app_version: env!("CARGO_PKG_VERSION").into(),
            observation: snapshot.observation.clone(),
            deployment: "not_included".into(),
            filesystem_metadata:
                "logical_tree_modes_only;unobserved_and_excluded_files_not_included".into(),
            entries,
        };
        let bytes = serde_json::to_vec(&manifest)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err("backup_manifest_too_large".into());
        }
        write(&self.stage, "manifest.json", &bytes)?;
        self.stage.sync_all()?;
        check_time(&control)?;
        if !same_inode(
            &self.parent,
            &open_dir(self.destination.parent().ok_or_else(invalid)?)?,
        )? {
            return Err(invalid());
        }
        rfs::renameat_with(
            &self.parent,
            &self.stage_name,
            &self.parent,
            self.destination.file_name().ok_or_else(invalid)?,
            rfs::RenameFlags::NOREPLACE,
        )?;
        self.parent.sync_all()?;
        let completion = write(
            &self.stage,
            ".COMPLETE.tmp",
            hex(&Sha256::digest(&bytes)).as_bytes(),
        )
        .and_then(|()| {
            rfs::renameat_with(
                &self.stage,
                ".COMPLETE.tmp",
                &self.stage,
                "COMPLETE",
                rfs::RenameFlags::NOREPLACE,
            )?;
            self.stage.sync_all()?;
            Ok(())
        });
        if let Err(error) = completion {
            for name in ["COMPLETE", ".COMPLETE.tmp"] {
                match rfs::unlinkat(&self.stage, name, rfs::AtFlags::empty()) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(_) => return Err("backup_publication_indeterminate".into()),
                }
            }
            self.stage
                .sync_all()
                .map_err(|_| "backup_publication_indeterminate")?;
            return Err(error);
        }
        Ok(AdminResult::BackupCreated {
            head_seq: notryx_domain::ChangeSeq::new(head_seq.parse()?),
            observation: snapshot.observation,
        })
    }
}

fn validate_hash(hash: &str) -> BackupResult<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid());
    }
    Ok(())
}

fn check_space(parent: &File, required: u64) -> BackupResult<()> {
    let space = rfs::fstatvfs(parent)?;
    let available = space.f_bavail.saturating_mul(space.f_frsize);
    if available < required.saturating_add(1024 * 1024) {
        return Err("backup_insufficient_space".into());
    }
    Ok(())
}

fn copy(
    source: &mut File,
    parent: &File,
    name: &str,
    hash: Option<&str>,
    control: &JobControl,
) -> BackupResult<()> {
    if !source.metadata()?.is_file() {
        return Err(invalid());
    }
    let mut target = create_file(parent, OsStr::new(name))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_time(control)?;
        let length = source.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        target.write_all(&buffer[..length])?;
        digest.update(&buffer[..length]);
    }
    if hash.is_some_and(|expected| expected != hex(&digest.finalize())) {
        return Err(invalid());
    }
    target.sync_all()?;
    Ok(())
}

async fn materialize_notes(
    db: &mut SqliteConnection,
    notes: &File,
    objects: &File,
    control: &JobControl,
) -> BackupResult<()> {
    let root: String = sqlx::query_scalar("SELECT root_id FROM workspaces WHERE singleton=1")
        .fetch_one(&mut *db)
        .await?;
    let rows =
        sqlx::query("SELECT node_id,parent_id,name,kind,content_hash FROM nodes WHERE deleted=0")
            .fetch_all(&mut *db)
            .await?;
    let mut pending = BTreeMap::new();
    for row in rows {
        let id: String = row.try_get("node_id")?;
        if id == root {
            continue;
        }
        let name: String = row.try_get("name")?;
        notryx_domain::mutation::validate_name(&name).map_err(|_| invalid())?;
        pending.insert(
            id,
            (
                row.try_get::<String, _>("parent_id")?,
                name,
                row.try_get::<String, _>("kind")?,
                row.try_get::<Option<String>, _>("content_hash")?,
            ),
        );
    }
    let mut paths = BTreeMap::from([(root, PathBuf::new())]);
    while !pending.is_empty() {
        check_time(control)?;
        let before = pending.len();
        let ready: Vec<_> = pending
            .iter()
            .filter(|(_, item)| paths.contains_key(&item.0))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let (parent_id, name, kind, hash) = pending.remove(&id).ok_or_else(invalid)?;
            let parent_path = &paths[&parent_id];
            let parent = if parent_path.as_os_str().is_empty() {
                notes.try_clone()?
            } else {
                open_child(notes, parent_path.as_os_str(), true)?
            };
            let path = parent_path.join(&name);
            if path.components().count() > MAX_DIRECTORY_DEPTH
                || path.as_os_str().as_bytes().len() > 1024
            {
                return Err(invalid());
            }
            match kind.as_str() {
                "folder" if hash.is_none() => {
                    create_dir(&parent, OsStr::new(&name))?.sync_all()?;
                    paths.insert(id, path);
                }
                "file" => {
                    let hash = hash.ok_or_else(invalid)?;
                    validate_hash(&hash)?;
                    copy(
                        &mut open_child(objects, OsStr::new(&hash), false)?,
                        &parent,
                        &name,
                        Some(&hash),
                        control,
                    )?;
                }
                _ => return Err(invalid()),
            }
            parent.sync_all()?;
        }
        if pending.len() == before {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(crate) async fn create(
    handle: &WorkspaceHandle,
    config_path: &Path,
    destination: &Path,
) -> Result<AdminResult, RuntimeError> {
    let permit = handle
        .backup_permit
        .clone()
        .try_acquire_owned()
        .map_err(|_| RuntimeError::Busy)?;
    let control = JobControl {
        deadline: Instant::now() + JOB_LIMIT,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let _cancel = CancelOnDrop(Arc::clone(&control.cancelled));
    let config_path = config_path.to_owned();
    let destination = destination.to_owned();
    let prepared = tokio::task::spawn_blocking(move || {
        Prepared::new(&config_path, &destination).map_err(mapped)
    })
    .await
    .map_err(|_| RuntimeError::Worker)??;
    check_time(&control).map_err(mapped)?;
    let snapshot = handle
        .backup_snapshot(
            prepared
                .state
                .try_clone()
                .map_err(|_| RuntimeError::Backup("backup_failed"))?,
        )
        .await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| RuntimeError::Worker)?
            .block_on(prepared.finish(snapshot, control))
            .map_err(mapped)
    })
    .await
    .map_err(|_| RuntimeError::Worker)?
}

#[cfg(test)]
mod tests;
