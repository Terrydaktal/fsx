use crate::entry::EntrySnapshot;
use crate::metadata::{EntryKind, HardlinkKey, MetadataSnapshot, metadata_snapshot};
use jwalk::WalkDir;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeMode {
    None,
    Logical,
    Allocated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardlinkMode {
    CountEveryEntry,
    DeduplicateCandidates,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymlinkMode {
    DoNotFollow,
    Follow,
}

#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub root: PathBuf,
    pub max_depth: Option<usize>,
    pub retain_depth: Option<usize>,
    pub size_mode: SizeMode,
    pub count_files: bool,
    pub count_dirs: bool,
    pub hardlinks: HardlinkMode,
    pub symlinks: SymlinkMode,
    pub show_hidden: bool,
    pub threads: usize,
}

impl Default for ScanRequest {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            max_depth: None,
            retain_depth: None,
            size_mode: SizeMode::None,
            count_files: false,
            count_dirs: false,
            hardlinks: HardlinkMode::CountEveryEntry,
            symlinks: SymlinkMode::DoNotFollow,
            show_hidden: true,
            threads: 1,
        }
    }
}

pub type ScannedEntry = EntrySnapshot;

#[derive(Clone, Debug, Default)]
pub struct Aggregate {
    pub logical_size: u64,
    pub allocated_size: u64,
    pub files: u64,
    pub dirs: u64,
    pub overflowed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ScanSnapshot {
    pub entries: Vec<ScannedEntry>,
    pub aggregates: std::collections::HashMap<PathBuf, Aggregate>,
    pub complete: bool,
    /// Number of entries that could not be read or inspected.  `complete`
    /// remains a cheap compatibility flag while callers that need diagnostics
    /// can report the magnitude of a partial walk.
    pub errors: u64,
    pub overflowed: bool,
}

pub fn scan(request: &ScanRequest) -> ScanSnapshot {
    let mut snapshot = ScanSnapshot {
        complete: true,
        ..ScanSnapshot::default()
    };
    if request.size_mode == SizeMode::Allocated {
        match std::fs::symlink_metadata(&request.root) {
            Ok(metadata) => {
                snapshot
                    .aggregates
                    .entry(request.root.clone())
                    .or_default()
                    .allocated_size = metadata_snapshot(&metadata).allocated_size;
            }
            Err(_) => {
                snapshot.complete = false;
                snapshot.errors = snapshot.errors.saturating_add(1);
            }
        }
    }
    let seen_dirs = Arc::new(std::sync::Mutex::new(HashSet::<HardlinkKey>::new()));
    let seen_files = Arc::new(std::sync::Mutex::new(HashSet::<HardlinkKey>::new()));
    let visited_dirs = Arc::new(std::sync::Mutex::new(HashSet::<HardlinkKey>::new()));
    if request.symlinks == SymlinkMode::Follow
        && let Ok(metadata) = std::fs::symlink_metadata(&request.root)
    {
        let snapshot = metadata_snapshot(&metadata);
        if let Some(key) = snapshot.hardlink_key() {
            let _ = visited_dirs.lock().map(|mut seen| seen.insert(key));
        }
    }
    let max_depth = request.max_depth.unwrap_or(usize::MAX);
    let walker = WalkDir::new(&request.root)
        .sort(false)
        .skip_hidden(!request.show_hidden)
        .follow_links(request.symlinks == SymlinkMode::Follow)
        .max_depth(max_depth)
        .parallelism(if request.threads > 1 {
            jwalk::Parallelism::RayonNewPool(request.threads)
        } else {
            jwalk::Parallelism::Serial
        });
    let visited_dirs_for_scan = Arc::clone(&visited_dirs);

    for result in walker.into_iter() {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                snapshot.complete = false;
                snapshot.errors = snapshot.errors.saturating_add(1);
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata_snapshot(&metadata),
            Err(_) => {
                snapshot.complete = false;
                snapshot.errors = snapshot.errors.saturating_add(1);
                continue;
            }
        };
        if metadata.kind == EntryKind::Directory && request.symlinks == SymlinkMode::Follow {
            if let Some(key) = metadata.hardlink_key()
                && !visited_dirs_for_scan
                    .lock()
                    .map(|mut seen| seen.insert(key))
                    .unwrap_or(true)
            {
                continue;
            }
            if !allow_hardlink(&metadata, request.hardlinks, &seen_dirs) {
                continue;
            }
        }
        let include_in_aggregate = metadata.kind != EntryKind::Directory
            && allow_hardlink(&metadata, request.hardlinks, &seen_files);
        let scanned = ScannedEntry {
            name: entry.file_name().to_os_string(),
            path: path.clone(),
            depth: entry.depth(),
            metadata: metadata.clone(),
        };
        if request
            .retain_depth
            .is_none_or(|depth| entry.depth() <= depth)
        {
            snapshot.entries.push(scanned);
        }
        if request.size_mode != SizeMode::None || request.count_files || request.count_dirs {
            if metadata.kind == EntryKind::Directory && request.size_mode == SizeMode::Allocated {
                snapshot
                    .aggregates
                    .entry(path.clone())
                    .or_default()
                    .allocated_size = metadata.allocated_size;
            }
            let contributes = metadata.kind == EntryKind::Directory || include_in_aggregate;
            if contributes
                && add_to_ancestors(
                    &mut snapshot.aggregates,
                    &request.root,
                    &path,
                    &metadata,
                    request,
                )
            {
                snapshot.overflowed = true;
            }
        }
    }
    snapshot
}

fn allow_hardlink(
    metadata: &MetadataSnapshot,
    mode: HardlinkMode,
    seen: &Arc<std::sync::Mutex<HashSet<HardlinkKey>>>,
) -> bool {
    if mode == HardlinkMode::CountEveryEntry || !metadata.is_hardlink_candidate() {
        return true;
    }
    let Some(key) = metadata.hardlink_key() else {
        return true;
    };
    seen.lock().map(|mut set| set.insert(key)).unwrap_or(true)
}

fn add_to_ancestors(
    aggregates: &mut std::collections::HashMap<PathBuf, Aggregate>,
    root: &Path,
    path: &Path,
    metadata: &MetadataSnapshot,
    request: &ScanRequest,
) -> bool {
    let mut overflowed = false;
    let mut current = path.parent();
    while let Some(directory) = current {
        let aggregate = aggregates.entry(directory.to_path_buf()).or_default();
        match metadata.kind {
            EntryKind::Directory => {
                if crate::overflow::checked_add_u64(&mut aggregate.dirs, 1) {
                    aggregate.overflowed = true;
                    overflowed = true;
                }
                if request.size_mode == SizeMode::Allocated
                    && crate::overflow::checked_add_u64(
                        &mut aggregate.allocated_size,
                        metadata.allocated_size,
                    )
                {
                    aggregate.overflowed = true;
                    overflowed = true;
                }
            }
            EntryKind::File | EntryKind::Symlink | EntryKind::Other => {
                if crate::overflow::checked_add_u64(&mut aggregate.files, 1) {
                    aggregate.overflowed = true;
                    overflowed = true;
                }
                if request.size_mode != SizeMode::None {
                    if crate::overflow::checked_add_u64(
                        &mut aggregate.logical_size,
                        metadata.logical_size,
                    ) {
                        aggregate.overflowed = true;
                        overflowed = true;
                    }
                    if crate::overflow::checked_add_u64(
                        &mut aggregate.allocated_size,
                        metadata.allocated_size,
                    ) {
                        aggregate.overflowed = true;
                        overflowed = true;
                    }
                }
            }
        }
        if directory == root {
            break;
        }
        current = directory.parent();
    }
    overflowed
}

#[allow(dead_code)]
fn _scan_duration_hint() -> Duration {
    Duration::from_secs(0)
}
