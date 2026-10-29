use super::{RuntimeError, WorkspaceHandle};
use notryx_protocol::admin::{
    AdminResponse, AdminResult, AdminUnsupportedItem, UnsupportedItemReason,
};
use std::fmt::Write;
use std::sync::{Arc, Mutex, atomic::Ordering};

const MAX_ITEMS: usize = 256;
const MAX_BYTES: usize = 128 * 1024;
const MAX_FRAME: usize = 16 * 1024;

#[derive(Default)]
pub(super) struct State {
    snapshot: Option<Arc<Snapshot>>,
    last_complete: Option<Arc<Snapshot>>,
    scanning: bool,
    stale: bool,
}

struct Snapshot {
    id: String,
    entries: Vec<AdminUnsupportedItem>,
    coverage_complete: bool,
    truncated: bool,
    watch_generation: u64,
}

#[derive(Default)]
pub(super) struct Collector {
    entries: Vec<AdminUnsupportedItem>,
    bytes: usize,
    truncated: bool,
}

impl Collector {
    pub(super) fn add(&mut self, parent_path: &str, name: &[u8], reason: UnsupportedItemReason) {
        let size = parent_path
            .len()
            .saturating_add(name.len().saturating_mul(3))
            .saturating_add(128);
        if self.entries.len() >= MAX_ITEMS || self.bytes.saturating_add(size) > MAX_BYTES {
            self.truncated = true;
            return;
        }
        let mut name_hex = String::with_capacity(name.len() * 2);
        for byte in name {
            let _ = write!(&mut name_hex, "{byte:02x}");
        }
        self.entries.push(AdminUnsupportedItem {
            parent_path: parent_path.to_owned(),
            name_hex,
            name_utf8: std::str::from_utf8(name).ok().map(str::to_owned),
            reason,
        });
        self.bytes += size;
    }
}

pub(super) struct ScanGuard {
    state: Arc<Mutex<State>>,
    finished: bool,
}

impl ScanGuard {
    pub(super) fn begin(state: Arc<Mutex<State>>) -> Self {
        if let Ok(mut value) = state.lock() {
            value.scanning = true;
            value.stale = value.snapshot.is_some();
        }
        Self {
            state,
            finished: false,
        }
    }

    pub(super) fn finish(
        mut self,
        mut collector: Collector,
        coverage_complete: bool,
        watch_generation: u64,
    ) {
        collector
            .entries
            .sort_by(|a, b| (&a.parent_path, &a.name_hex).cmp(&(&b.parent_path, &b.name_hex)));
        if let Ok(mut value) = self.state.lock() {
            if coverage_complete
                || value.snapshot.is_none()
                || !collector.entries.is_empty()
                || collector.truncated
            {
                let snapshot = Arc::new(Snapshot {
                    id: uuid::Uuid::now_v7().to_string(),
                    entries: collector.entries,
                    coverage_complete,
                    truncated: collector.truncated,
                    watch_generation,
                });
                if coverage_complete {
                    value.last_complete = Some(snapshot.clone());
                }
                value.snapshot = Some(snapshot);
            }
            value.scanning = false;
            value.stale = !coverage_complete;
        }
        self.finished = true;
    }
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        if !self.finished
            && let Ok(mut value) = self.state.lock()
        {
            value.scanning = false;
            value.stale = true;
        }
    }
}

pub(super) fn page(
    handle: &WorkspaceHandle,
    snapshot_id: Option<&str>,
    offset: u16,
    limit: u8,
) -> Result<AdminResult, RuntimeError> {
    if !(1..=20).contains(&limit) || (snapshot_id.is_none() && offset != 0) {
        return Err(RuntimeError::Workspace(crate::Error::InvalidRequest));
    }
    let value = handle
        .unsupported_items
        .lock()
        .map_err(|_| RuntimeError::Worker)?;
    let Some(snapshot) = &value.snapshot else {
        if snapshot_id.is_some() {
            return Err(RuntimeError::Workspace(crate::Error::ExternalChange));
        }
        return Ok(AdminResult::UnsupportedItems {
            snapshot_id: None,
            previous_snapshot_id: None,
            entries: Vec::new(),
            coverage_complete: false,
            stale: value.stale || value.scanning,
            truncated: false,
            next_offset: None,
        });
    };
    let latest = snapshot;
    let snapshot = if snapshot_id.is_some_and(|id| id != latest.id) {
        value
            .last_complete
            .as_ref()
            .filter(|previous| snapshot_id == Some(previous.id.as_str()))
            .ok_or(RuntimeError::Workspace(crate::Error::ExternalChange))?
    } else {
        latest
    };
    let is_previous = !Arc::ptr_eq(snapshot, latest);
    let previous_snapshot_id = if is_previous {
        None
    } else {
        value
            .last_complete
            .as_ref()
            .filter(|previous| !Arc::ptr_eq(previous, latest))
            .map(|previous| previous.id.clone())
    };
    let start = usize::from(offset);
    if start > snapshot.entries.len() {
        return Err(RuntimeError::Workspace(crate::Error::InvalidRequest));
    }
    let stale = is_previous
        || value.stale
        || value.scanning
        || snapshot.watch_generation >> 2
            != handle.known_file_watch_generation.load(Ordering::Acquire) >> 2
        || handle.watch_folder_recovery_epoch.load(Ordering::Acquire)
            != handle
                .watch_folder_recovery_completed
                .load(Ordering::Acquire);
    let mut entries = Vec::new();
    for item in snapshot.entries.iter().skip(start).take(usize::from(limit)) {
        entries.push(item.clone());
        let next = start + entries.len();
        let probe = AdminResponse::Ok {
            result: AdminResult::UnsupportedItems {
                snapshot_id: Some(snapshot.id.clone()),
                previous_snapshot_id: previous_snapshot_id.clone(),
                entries: entries.clone(),
                coverage_complete: snapshot.coverage_complete,
                stale,
                truncated: snapshot.truncated,
                next_offset: (next < snapshot.entries.len()).then_some(next as u16),
            },
        };
        if serde_json::to_vec(&probe)
            .map_err(|_| RuntimeError::Worker)?
            .len()
            + 1
            > MAX_FRAME
        {
            entries.pop();
            if entries.is_empty() {
                return Err(RuntimeError::Workspace(crate::Error::InvalidFormat));
            }
            break;
        }
    }
    let next = start + entries.len();
    Ok(AdminResult::UnsupportedItems {
        snapshot_id: Some(snapshot.id.clone()),
        previous_snapshot_id,
        entries,
        coverage_complete: snapshot.coverage_complete,
        stale,
        truncated: snapshot.truncated,
        next_offset: (next < snapshot.entries.len()).then_some(next as u16),
    })
}
