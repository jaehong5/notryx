use super::{RuntimeError, WorkspaceHandle};
use notryx_domain::NodeId;
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

const MAX_FOLDERS: usize = 10_000;
const MAX_PENDING: usize = 256;
const MAX_EVENTS_PER_TURN: usize = 128;
const DEBOUNCE: Duration = Duration::from_millis(300);
const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(600);

struct WatchTarget {
    node_id: NodeId,
    identity: notryx_server_storage::FolderIdentity,
}

type FileJob = tokio::task::JoinHandle<((NodeId, String, bool), u8, Result<(), RuntimeError>)>;
type FolderJob = tokio::task::JoinHandle<(
    NodeId,
    Option<i32>,
    Result<Option<WatchTarget>, RuntimeError>,
    Option<Instant>,
)>;
type FolderRecoveryJob = tokio::task::JoinHandle<Result<(), RuntimeError>>;

async fn recover_created_folders(
    handle: WorkspaceHandle,
    known: std::collections::HashSet<NodeId>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    #[cfg(test)]
    {
        let pause = handle
            .known_file_watch_recovery_pause
            .lock()
            .unwrap()
            .take();
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
    let mut after = None;
    loop {
        if *stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        let page = match handle.watch_folder_page(after).await {
            Ok(page) => page,
            Err(RuntimeError::Busy) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if page.incomplete {
            return Err(RuntimeError::Worker);
        }
        for folder in page.folders {
            let node_id = folder.node_id;
            drop(folder);
            if !known.contains(&node_id) {
                tokio::select! {
                    result = handle.watch_folder_requests.send(node_id) => result.map_err(|_| RuntimeError::Closed)?,
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() { return Err(RuntimeError::Closed); }
                    }
                }
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(()),
        }
    }
}

fn watch_flags() -> WatchFlags {
    WatchFlags::ONLYDIR
        | WatchFlags::MODIFY
        | WatchFlags::ATTRIB
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::CREATE
        | WatchFlags::DELETE
        | WatchFlags::MOVED_FROM
        | WatchFlags::MOVED_TO
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF
}

async fn register_created_folder(
    handle: WorkspaceHandle,
    fd: std::os::fd::OwnedFd,
    stop: watch::Receiver<bool>,
    node_id: NodeId,
) -> (Option<i32>, Result<Option<WatchTarget>, RuntimeError>) {
    let folder = loop {
        if *stop.borrow() {
            return (None, Err(RuntimeError::Closed));
        }
        match handle.watch_folder(node_id).await {
            Ok(folder) => break folder,
            Err(RuntimeError::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(error) => {
                return (
                    None,
                    folder_registration_failure(&handle, &stop, node_id, error).await,
                );
            }
        }
    };
    #[cfg(test)]
    if handle.test_take_watch_folder_failure(node_id) {
        return (None, Err(RuntimeError::Worker));
    }
    #[cfg(test)]
    {
        let pause = handle.known_file_watch_folder_pause.lock().unwrap().take();
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
    let path = format!("/proc/self/fd/{}", folder.file.as_raw_fd());
    let wd = match inotify::add_watch(&fd, path.as_str(), watch_flags()) {
        Ok(wd) => wd,
        Err(_) => {
            return (
                None,
                folder_registration_failure(&handle, &stop, node_id, RuntimeError::Worker).await,
            );
        }
    };
    let result = loop {
        if *stop.borrow() {
            break Err(RuntimeError::Closed);
        }
        match handle
            .watch_lookup(folder.node_id, folder.identity, None)
            .await
        {
            Ok(_) => {
                #[cfg(test)]
                {
                    let pause = {
                        let mut slot = handle.successful_folder_registration_pause.lock().unwrap();
                        if slot
                            .as_ref()
                            .is_some_and(|(target, _, _)| *target == node_id)
                        {
                            slot.take()
                        } else {
                            None
                        }
                    };
                    if let Some((_, entered, release)) = pause {
                        let _ = entered.send(wd);
                        let _ = release.await;
                    }
                }
                break Ok(Some(WatchTarget {
                    node_id: folder.node_id,
                    identity: folder.identity,
                }));
            }
            Err(RuntimeError::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(error) => break folder_registration_failure(&handle, &stop, node_id, error).await,
        }
    };
    (Some(wd), result)
}

async fn folder_registration_failure(
    handle: &WorkspaceHandle,
    stop: &watch::Receiver<bool>,
    node_id: NodeId,
    error: RuntimeError,
) -> Result<Option<WatchTarget>, RuntimeError> {
    loop {
        if *stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        match handle.watch_folder_deleted(node_id).await {
            Ok(true) => {
                #[cfg(test)]
                {
                    let pause = {
                        let mut slot = handle.deleted_folder_registration_pause.lock().unwrap();
                        if slot
                            .as_ref()
                            .is_some_and(|(target, _, _, _)| *target == node_id)
                        {
                            slot.take()
                        } else {
                            None
                        }
                    };
                    if let Some((_, entered, release, observed)) = pause {
                        let _ = entered.send(());
                        let _ = release.await;
                        *handle
                            .deleted_folder_registration_observation
                            .lock()
                            .unwrap() = Some((node_id, observed));
                    }
                }
                return Ok(None);
            }
            Ok(false) => return Err(error),
            Err(RuntimeError::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(other) => return Err(other),
        }
    }
}

fn retry_file(
    pending: &mut HashMap<(NodeId, String, bool), (Instant, u8)>,
    key: (NodeId, String, bool),
    failures: u8,
) -> bool {
    if pending.len() >= MAX_PENDING && !pending.contains_key(&key) {
        return false;
    }
    pending
        .entry(key)
        .or_insert((Instant::now() + DEBOUNCE, failures));
    true
}

#[cfg(test)]
fn take_scan_panic(handle: &WorkspaceHandle) -> bool {
    use std::sync::atomic::Ordering;
    let mut count = handle.known_file_watch_scan_panic.load(Ordering::Acquire);
    while count > 0 {
        match handle.known_file_watch_scan_panic.compare_exchange_weak(
            count,
            count - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(current) => count = current,
        }
    }
    false
}

#[cfg(test)]
fn take_read_error(handle: &WorkspaceHandle) -> bool {
    use std::sync::atomic::Ordering;
    let mut count = handle
        .known_file_watch_injected_read_errors
        .load(Ordering::Acquire);
    while count > 0 {
        match handle
            .known_file_watch_injected_read_errors
            .compare_exchange_weak(count, count - 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                handle
                    .known_file_watch_observed_read_errors
                    .fetch_add(1, Ordering::AcqRel);
                return true;
            }
            Err(current) => count = current,
        }
    }
    false
}

#[cfg(test)]
type RegistrationPause = (oneshot::Sender<()>, oneshot::Receiver<()>);
#[cfg(not(test))]
type RegistrationPause = ();

async fn register(
    handle: &WorkspaceHandle,
    fd: &std::os::fd::OwnedFd,
    stop: &watch::Receiver<bool>,
    mut _pause: Option<RegistrationPause>,
) -> Result<HashMap<i32, WatchTarget>, RuntimeError> {
    let mut after = None;
    let mut folders = HashMap::new();
    loop {
        if *stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        let page = match handle.watch_folder_page(after).await {
            Ok(page) => page,
            Err(RuntimeError::Busy) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if page.incomplete {
            handle.mark_known_file_watch_degraded();
        }
        for folder in page.folders {
            if folders.len() >= MAX_FOLDERS {
                return Err(RuntimeError::Busy);
            }
            let path = format!("/proc/self/fd/{}", folder.file.as_raw_fd());
            let wd = inotify::add_watch(fd, path.as_str(), watch_flags())
                .map_err(|_| RuntimeError::Worker)?;
            if folders.contains_key(&wd) {
                return Err(RuntimeError::Worker);
            }
            #[cfg(test)]
            if let Some((entered, release)) = _pause.take() {
                let _ = entered.send(());
                let _ = release.await;
            }
            loop {
                if *stop.borrow() {
                    return Err(RuntimeError::Closed);
                }
                match handle
                    .watch_lookup(folder.node_id, folder.identity, None)
                    .await
                {
                    Ok(_) => break,
                    Err(RuntimeError::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Err(error) => return Err(error),
                }
            }
            folders.insert(
                wd,
                WatchTarget {
                    node_id: folder.node_id,
                    identity: folder.identity,
                },
            );
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(folders),
        }
    }
}

pub(super) fn excluded(name: &str) -> bool {
    let key = notryx_domain::mutation::normalized_name_key(name);
    matches!(key.as_str(), ".git" | ".obsidian" | "node_modules") || key.starts_with(".mdw-")
}

pub(super) async fn run(
    handle: WorkspaceHandle,
    ready: oneshot::Sender<
        Option<std::sync::Arc<HashMap<NodeId, notryx_server_storage::FolderIdentity>>>,
    >,
    mut stop: watch::Receiver<bool>,
    test_sweep_interval: Option<Duration>,
    mut folder_requests: mpsc::Receiver<NodeId>,
) {
    let fd = match inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK) {
        Ok(fd) => fd,
        Err(_) => {
            handle.mark_known_file_watch_degraded();
            let _ = ready.send(None);
            return;
        }
    };
    let mut folders = match register(&handle, &fd, &stop, None).await {
        Ok(folders) => folders,
        Err(_) => {
            if !*stop.borrow() {
                handle.mark_known_file_watch_degraded();
            }
            let _ = ready.send(None);
            return;
        }
    };
    let mut scan_folders = std::sync::Arc::new(
        folders
            .values()
            .map(|folder| (folder.node_id, folder.identity))
            .collect::<HashMap<_, _>>(),
    );
    // SAFETY: AsyncFd owns this OwnedFd for the lifetime of the registration.
    let fd = match unsafe { AsyncFd::register(fd) } {
        Ok(fd) => fd,
        Err(_) => {
            handle.mark_known_file_watch_degraded();
            let _ = ready.send(None);
            return;
        }
    };
    handle.log_known_file_scan_event("known_file_watch_started");
    let _ = ready.send(Some(scan_folders.clone()));
    let mut pending: HashMap<(NodeId, String, bool), (Instant, u8)> = HashMap::new();
    let mut scan_needed = false;
    let mut diagnostic_scan_needed = false;
    let mut scan_retry_delay = false;
    let mut scan_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut scan_failures = 0u8;
    let mut read_failures = 0u8;
    let mut read_retry_at: Option<Instant> = None;
    let mut file_job: Option<FileJob> = None;
    let mut folder_job: Option<FolderJob> = None;
    let mut folder_recovery_job: Option<FolderRecoveryJob> = None;
    let mut folder_recovery_epoch = None;
    let mut folder_recovery_walked = None;
    let mut folder_registration_failed = false;
    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut reader = inotify::Reader::new(fd.get_ref(), &mut buffer);
    let sweep_interval = test_sweep_interval.unwrap_or(FULL_SWEEP_INTERVAL);
    let mut sweep = tokio::time::interval_at(Instant::now() + sweep_interval, sweep_interval);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let retry_poll = (handle.watch_scan_retry_interval / 2).max(Duration::from_millis(1));
    let mut watch_retry = tokio::time::interval_at(Instant::now() + retry_poll, retry_poll);
    watch_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *stop.borrow() {
            break;
        }
        #[cfg(test)]
        handle
            .known_file_watch_max_pending
            .fetch_max(pending.len(), std::sync::atomic::Ordering::AcqRel);
        if let Some(epoch) = folder_recovery_walked
            && folder_recovery_job.is_none()
            && folder_job.is_none()
            && folder_requests.is_empty()
            && !folder_registration_failed
        {
            handle
                .watch_folder_recovery_completed
                .store(epoch, std::sync::atomic::Ordering::Release);
            folder_recovery_walked = None;
            scan_needed = true;
        }
        if (scan_needed || diagnostic_scan_needed)
            && scan_task.is_none()
            && folder_job.is_none()
            && folder_recovery_job.is_none()
            && folder_requests.is_empty()
        {
            let diagnostic_only = !scan_needed;
            scan_needed = false;
            diagnostic_scan_needed = false;
            if !diagnostic_only {
                handle
                    .known_file_scan_incomplete
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            let scan_handle = handle.clone();
            let scan_folders = scan_folders.clone();
            let failures = scan_failures;
            let retry_delay = std::mem::take(&mut scan_retry_delay);
            scan_task = Some(tokio::spawn(async move {
                if diagnostic_only {
                    tokio::time::sleep(Duration::from_millis(350)).await;
                } else if retry_delay {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                if failures > 0 {
                    tokio::time::sleep(Duration::from_millis(50 * u64::from(failures))).await;
                }
                #[cfg(test)]
                assert!(!take_scan_panic(&scan_handle), "injected scan task panic");
                #[cfg(test)]
                scan_handle
                    .known_file_watch_scan_waiting
                    .store(true, std::sync::atomic::Ordering::Release);
                scan_handle
                    .scan_known_files(Some(scan_folders), diagnostic_only)
                    .await;
                #[cfg(test)]
                scan_handle
                    .known_file_watch_scan_waiting
                    .store(false, std::sync::atomic::Ordering::Release);
            }));
        }
        let deadline = pending
            .values()
            .map(|(deadline, _)| *deadline)
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        tokio::select! {
            _ = handle.diagnostic_scan_retry.notified() => { diagnostic_scan_needed = true; }
            _ = handle.watch_folder_recovery.notified(), if folder_recovery_job.is_none() => {
                folder_registration_failed = false;
                folder_recovery_epoch = Some(handle.watch_folder_recovery_epoch.load(std::sync::atomic::Ordering::Acquire));
                folder_recovery_walked = None;
                let known = folders.values().map(|folder| folder.node_id).collect();
                folder_recovery_job = Some(tokio::spawn(recover_created_folders(handle.clone(), known, stop.clone())));
            }
            result = async { folder_recovery_job.as_mut().expect("guarded folder recovery").await }, if folder_recovery_job.is_some() => {
                folder_recovery_job = None;
                match result {
                    Ok(Ok(())) => folder_recovery_walked = folder_recovery_epoch.take(),
                    Ok(Err(RuntimeError::Closed)) if *stop.borrow() => {},
                    _ => {
                        folder_registration_failed = true;
                        handle.mark_watch_folder_registration_problem("known_file_watch_registration_failed");
                    }
                }
            }
            Some(node_id) = folder_requests.recv(), if folder_job.is_none() => {
                handle.known_file_scan_incomplete.store(true, std::sync::atomic::Ordering::Release);
                if folders.len() >= MAX_FOLDERS {
                    folder_registration_failed = true;
                    handle.mark_watch_folder_registration_problem("known_file_watch_registration_failed");
                } else {
                    match fd.get_ref().try_clone() {
                        Ok(copy) => {
                            let job_handle = handle.clone();
                            let job_stop = stop.clone();
                            let attempted_at = handle.watch_scan_registration_attempts.lock().ok().and_then(|attempts| attempts.get(&node_id).copied());
                            folder_job = Some(tokio::spawn(async move {
                                let (wd, result) = register_created_folder(job_handle, copy, job_stop, node_id).await;
                                (node_id, wd, result, attempted_at)
                            }));
                        }
                        Err(_) => {
                            folder_registration_failed = true;
                            handle.mark_watch_folder_registration_problem("known_file_watch_registration_failed");
                        }
                    }
                }
            }
            result = async { folder_job.as_mut().expect("guarded folder job").await }, if folder_job.is_some() => {
                folder_job = None;
                #[cfg(test)]
                handle.known_file_watch_folder_jobs_done.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                let mut registered = false;
                let result = match result {
                    Ok((node_id, Some(wd), Ok(Some(target)), attempted_at)) => {
                        match handle.watch_lookup(target.node_id, target.identity, None).await {
                            Ok(_) => Ok((node_id, Some(wd), Ok(Some(target)), attempted_at)),
                            Err(error) => {
                                scan_needed = true;
                                Ok((node_id, Some(wd), Err(error), attempted_at))
                            }
                        }
                    }
                    other => other,
                };
                match result {
                    Ok((node_id, Some(wd), Ok(Some(target)), attempted_at)) if !folders.contains_key(&wd) => {
                        if let Ok(mut attempts) = handle.watch_scan_registration_attempts.lock()
                            && attempts.get(&node_id).copied() == attempted_at { attempts.remove(&node_id); }
                        registered = true;
                        folders.insert(wd, target);
                        scan_folders = std::sync::Arc::new(folders.values().map(|folder| (folder.node_id, folder.identity)).collect());
                        handle.known_file_watch_generation.fetch_add(4, std::sync::atomic::Ordering::AcqRel);
                        scan_needed = true;
                    }
                    Ok((node_id, Some(wd), Ok(Some(target)), attempted_at)) if folders.get(&wd).is_some_and(|known| known.node_id == target.node_id && known.identity == target.identity) => {
                        if let Ok(mut attempts) = handle.watch_scan_registration_attempts.lock()
                            && attempts.get(&node_id).copied() == attempted_at { attempts.remove(&node_id); }
                        registered = true;
                        scan_needed = true;
                    }
                    Ok((_node_id, wd, Ok(None), _)) => {
                        #[cfg(test)]
                        if let Some((target, observed)) = handle.deleted_folder_registration_observation.lock().unwrap().take()
                            && target == _node_id {
                                let pending = handle.watch_scan_registration_attempts.lock().unwrap().get(&_node_id).copied();
                                let _ = observed.send(pending);
                            }
                        registered = true;
                        if let Some(wd) = wd && !folders.contains_key(&wd) { let _ = inotify::remove_watch(fd.get_ref(), wd); }
                        scan_needed = true;
                    }
                    Ok((_, Some(wd), Err(_), _)) => {
                        if !folders.contains_key(&wd) { let _ = inotify::remove_watch(fd.get_ref(), wd); }
                        folder_registration_failed = true;
                        handle.mark_watch_folder_registration_problem("known_file_watch_registration_failed");
                    }
                    _ => {
                        folder_registration_failed = true;
                        handle.mark_watch_folder_registration_problem("known_file_watch_registration_failed");
                    }
                }
                if registered && folder_registration_failed
                    && handle.watch_folder_recovery_epoch.load(std::sync::atomic::Ordering::Acquire)
                        != handle.watch_folder_recovery_completed.load(std::sync::atomic::Ordering::Acquire)
                {
                    handle.watch_folder_recovery.notify_one();
                }
            }
            _ = handle.known_file_scan_retry.notified() => {
                scan_needed = true;
                scan_retry_delay = true;
            }
            _ = async {
                #[cfg(test)]
                handle.known_file_watch_test_read_error.notified().await;
                #[cfg(not(test))]
                std::future::pending::<()>().await;
            } => {
                #[cfg(test)]
                {
                    let (readable_error, ack) = handle.known_file_watch_test_read_error_ack.lock().unwrap().take().unwrap();
                    handle.mark_known_file_watch_read_error();
                    let _ = ack.send(());
                    if readable_error { break; }
                    read_failures = read_failures.saturating_add(1).min(4);
                    read_retry_at = Some(Instant::now() + Duration::from_millis(250 << (read_failures - 1)));
                }
            }
            _ = async {
                #[cfg(test)]
                handle.known_file_watch_test_overflow.notified().await;
                #[cfg(not(test))]
                std::future::pending::<()>().await;
            } => {
                handle.known_file_watch_generation.fetch_add(4, std::sync::atomic::Ordering::AcqRel);
                handle.mark_known_file_watch_degraded();
                scan_needed = true;
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { break; }
            }
            _ = sweep.tick() => { scan_needed = true; }
            _ = watch_retry.tick() => {
                if handle.watch_scan_registration_attempts.lock().map(|attempts| {
                    let now = Instant::now();
                    let delay = handle.watch_scan_retry_delay();
                    attempts.values().any(|previous| now.duration_since(*previous) >= delay)
                }).unwrap_or(true) {
                    scan_needed = true;
                }
            }
            _ = tokio::time::sleep_until(read_retry_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400))), if read_retry_at.is_some() => {
                read_retry_at = None;
            }
            result = async { scan_task.as_mut().expect("guarded scan task").await }, if scan_task.is_some() => {
                scan_task = None;
                if result.is_err() {
                    handle.mark_known_file_watch_degraded();
                    handle.known_file_scan_incomplete.store(true, std::sync::atomic::Ordering::Release);
                    if scan_failures < 2 {
                        scan_failures += 1;
                        scan_needed = true;
                    }
                } else {
                    scan_failures = 0;
                }
            }
            result = async { file_job.as_mut().expect("guarded file job").await }, if file_job.is_some() => {
                file_job = None;
                match result {
                    Ok((_, _, Ok(()))) => {},
                    Ok((_, _, Err(RuntimeError::Closed))) if *stop.borrow() => {},
                    Ok((_, _, Err(RuntimeError::Closed))) => {
                        handle.mark_known_file_watch_degraded();
                        scan_needed = true;
                    },
                    Ok((key, failures, Err(RuntimeError::Busy))) => {
                        if !retry_file(&mut pending, key, failures) { handle.mark_known_file_watch_degraded(); scan_needed = true; }
                    },
                    Ok((key, failures, Err(_))) if failures < 2 => {
                        if !retry_file(&mut pending, key, failures + 1) { handle.mark_known_file_watch_degraded(); scan_needed = true; }
                    },
                    _ => { handle.mark_known_file_watch_degraded(); scan_needed = true; },
                }
            }
            ready = fd.readable(), if read_retry_at.is_none() => {
                let Ok(mut ready) = ready else { handle.mark_known_file_watch_read_error(); break; };
                #[cfg(test)]
                let mut batch_size = 0usize;
                for _ in 0..MAX_EVENTS_PER_TURN {
                    #[cfg(test)]
                    let result = if take_read_error(&handle) { Err(rustix::io::Errno::IO) } else { reader.next() };
                    #[cfg(not(test))]
                    let result = reader.next();
                    let event = match result {
                        Ok(event) => event,
                        Err(rustix::io::Errno::AGAIN) => {
                            read_failures = 0;
                            handle.mark_known_file_watch_read_recovered();
                            #[cfg(test)]
                            {
                                let pause = handle.known_file_watch_recovered_pause.lock().unwrap().take();
                                if let Some((entered, release)) = pause {
                                    let _ = entered.send(());
                                    let _ = release.await;
                                }
                            }
                            ready.clear_ready();
                            break;
                        }
                        Err(_) => {
                            handle.mark_known_file_watch_read_error();
                            read_failures = read_failures.saturating_add(1).min(4);
                            read_retry_at = Some(Instant::now() + Duration::from_millis(250 << (read_failures - 1)));
                            break;
                        }
                    };
                    #[cfg(test)]
                    {
                        let pause = handle.known_file_watch_read_pause.lock().unwrap().take();
                        if let Some((entered, release)) = pause {
                            let _ = entered.send(());
                            let _ = release.await;
                        }
                    }
                    #[cfg(test)]
                    { batch_size += 1; }
                    let flags = event.events();
                    if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                        handle.known_file_watch_generation.fetch_add(4, std::sync::atomic::Ordering::AcqRel);
                        handle.mark_known_file_watch_degraded();
                        scan_needed = true;
                        continue;
                    }
                    if flags.intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::IGNORED | ReadFlags::UNMOUNT) {
                        #[cfg(test)]
                        {
                            let notify = {
                                let mut slot = handle.watch_self_event.lock().unwrap();
                                if slot.as_ref().is_some_and(|(wd, _)| *wd == event.wd()) { slot.take() } else { None }
                            };
                            if let Some((_, sender)) = notify { let _ = sender.send(()); }
                        }
                        handle.known_file_watch_generation.fetch_add(4, std::sync::atomic::Ordering::AcqRel);
                        if folders.remove(&event.wd()).is_some() {
                            scan_folders = std::sync::Arc::new(folders.values().map(|folder| (folder.node_id, folder.identity)).collect());
                        }
                        handle.mark_known_file_watch_degraded();
                        scan_needed = true;
                        continue;
                    }
                    let Some(folder) = folders.get(&event.wd()) else { continue; };
                    let Some(raw_name) = event.file_name() else { continue; };
                    let Ok(name) = raw_name.to_str() else {
                        handle.known_file_watch_generation.fetch_add(4, std::sync::atomic::Ordering::AcqRel);
                        handle.mark_known_file_watch_degraded();
                        continue;
                    };
                    #[cfg(test)]
                    if handle.known_file_watch_suppressed_name.lock().unwrap().as_deref() == Some(name) { continue; }
                    if excluded(name) { continue; }
                    handle.known_file_watch_generation.fetch_add(4, std::sync::atomic::Ordering::AcqRel);
                    #[cfg(test)]
                    {
                        let mut signal = handle.watch_name_signal.lock().unwrap();
                        if signal.as_ref().is_some_and(|(expected, _)| expected == name)
                            && let Some((_, sender)) = signal.take() { let _ = sender.send(()); }
                    }
                    let is_directory = flags.contains(ReadFlags::ISDIR);
                    if is_directory { scan_needed = true; }
                    if notryx_domain::mutation::validate_name(name).is_err() {
                        handle.mark_known_file_watch_degraded();
                        continue;
                    }
                    let key = (folder.node_id, name.to_owned(), is_directory);
                    if pending.len() >= MAX_PENDING && !pending.contains_key(&key) {
                        handle.mark_known_file_watch_degraded();
                        scan_needed = true;
                    } else {
                        pending.insert(key, (Instant::now() + DEBOUNCE, 0));
                    }
                }
                #[cfg(test)]
                handle.known_file_watch_max_batch.fetch_max(batch_size, std::sync::atomic::Ordering::AcqRel);
                drop(ready);
                tokio::task::yield_now().await;
            }
            _ = tokio::time::sleep_until(deadline), if !pending.is_empty() && file_job.is_none() => {
                let now = Instant::now();
                let due = pending.iter().find(|(_, (deadline, _))| *deadline <= now).map(|(key, _)| key.clone());
                if let Some(key) = due {
                    let (_, failures) = pending.remove(&key).expect("pending due key");
                    let Some(folder) = folders.values().find(|folder| folder.node_id == key.0) else {
                        handle.mark_known_file_watch_degraded();
                        scan_needed = true;
                        continue;
                    };
                    let identity = folder.identity;
                    let file_handle = handle.clone();
                    file_job = Some(tokio::spawn(async move {
                        let lookup = if key.2 {
                            match file_handle.reconcile_watched_folder(key.0, key.1.clone(), identity).await {
                                Ok(_) | Err(RuntimeError::Workspace(crate::Error::NotFound)) => Ok(None),
                                Err(error) => Err(error),
                            }
                        } else {
                            file_handle.watch_lookup(key.0, identity, Some(key.1.clone())).await
                        };
                        #[cfg(test)]
                        let lookup = if !key.2 { match file_handle.known_file_watch_job_fault.swap(0, std::sync::atomic::Ordering::AcqRel) {
                            1 => Err(RuntimeError::Closed),
                            2 => Err(RuntimeError::Busy),
                            _ => lookup,
                        }} else { lookup };
                        #[cfg(test)]
                        let done = {
                            let pause = if !key.2 && lookup.is_ok() {
                                file_handle.known_file_watch_lookup_pause.lock().unwrap().take()
                            } else {
                                None
                            };
                            if let Some((entered, release, done)) = pause {
                                let _ = entered.send(());
                                let _ = release.await;
                                Some(done)
                            } else { None }
                        };
                        let result = match lookup {
                            Ok(Some(node_id)) => file_handle.reconcile_watched_file(node_id, key.0, identity).await.map(|_| ()),
                            Ok(None) if !key.2 && key.1.to_ascii_lowercase().ends_with(".md") => {
                                match file_handle.reconcile_watched_new_file(key.0, key.1.clone(), identity).await {
                                    Ok(_) | Err(RuntimeError::Workspace(crate::Error::NotFound)) => Ok(()),
                                    Err(error) => Err(error),
                                }
                            },
                            Ok(None) => Ok(()),
                            Err(error) => Err(error),
                        };
                        #[cfg(test)]
                        if let Some(done) = done { let _ = done.send(()); }
                        (key, failures, result)
                    }));
                }
            }
        }
    }
    if let Some(scan) = scan_task {
        let _ = scan.await;
    }
    if let Some(job) = file_job {
        let _ = job.await;
    }
    if let Some(job) = folder_job {
        let _ = job.await;
    }
    if let Some(job) = folder_recovery_job {
        let _ = job.await;
    }
    handle.log_known_file_scan_event("known_file_watch_stopped");
}

#[cfg(test)]
pub(super) async fn test_register_with_pause(
    handle: WorkspaceHandle,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) -> Result<usize, RuntimeError> {
    let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)
        .map_err(|_| RuntimeError::Worker)?;
    let (_stop, receiver) = watch::channel(false);
    register(&handle, &fd, &receiver, Some((entered, release)))
        .await
        .map(|folders| folders.len())
}
