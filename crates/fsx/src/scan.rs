use crate::entry::EntrySnapshot;
use crate::metadata::{EntryKind, HardlinkKey, MetadataSnapshot, metadata_snapshot};
use jwalk::WalkDir;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

type ScanPoolCache = Mutex<Option<(usize, Arc<rayon::ThreadPool>)>>;

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

/// A memory-bounded recursive snapshot for callers that only need totals for
/// the root and each of its immediate children.
///
/// Unlike [`scan`], this does not retain every descendant or create an
/// aggregate for every directory. Directory counts exclude the aggregate's
/// own directory; sizes include the directory inode when `size_mode` is
/// [`SizeMode::Allocated`].
#[derive(Clone, Debug)]
pub struct TopLevelScanSnapshot {
    pub root: Aggregate,
    pub children: HashMap<OsString, Aggregate>,
    pub complete: bool,
    pub errors: u64,
    pub overflowed: bool,
}

impl Default for TopLevelScanSnapshot {
    fn default() -> Self {
        Self {
            root: Aggregate::default(),
            children: HashMap::new(),
            complete: true,
            errors: 0,
            overflowed: false,
        }
    }
}

#[derive(Default)]
struct TopLevelScanState {
    root: Aggregate,
    children: HashMap<OsString, Aggregate>,
    overflowed: bool,
}

/// Recursively aggregate only the root and its immediate children.
///
/// Hard-link candidates are deduplicated independently inside each immediate
/// child and globally for the root. This means a hard link present under two
/// different children contributes to both child totals but only once to the
/// root total.
pub fn scan_top_level(request: &ScanRequest) -> TopLevelScanSnapshot {
    let root = crate::path::full_path(&request.root);
    if request.symlinks == SymlinkMode::DoNotFollow
        && let Ok(metadata) = std::fs::symlink_metadata(&root)
        && metadata.file_type().is_symlink()
    {
        let mut snapshot = TopLevelScanSnapshot::default();
        let metadata = metadata_snapshot(&metadata);
        snapshot.overflowed =
            add_metadata_to_aggregate(&mut snapshot.root, &metadata, request, false, true);
        return snapshot;
    }
    let state = Arc::new(Mutex::new(TopLevelScanState::default()));
    let errors = Arc::new(AtomicU64::new(0));
    let dedupe_sizes = request.size_mode != SizeMode::None
        && request.hardlinks == HardlinkMode::DeduplicateCandidates;
    let root_seen = dedupe_sizes.then(|| Arc::new(Mutex::new(HashSet::<HardlinkKey>::new())));
    let child_seen = dedupe_sizes
        .then(|| Arc::new(Mutex::new(HashMap::<OsString, HashSet<HardlinkKey>>::new())));
    let followed_dirs = (request.symlinks == SymlinkMode::Follow)
        .then(|| Arc::new(Mutex::new(HashSet::<HardlinkKey>::new())));

    if let Ok(metadata) = root_metadata(&root, request.symlinks) {
        let metadata = metadata_snapshot(&metadata);
        if metadata.kind == EntryKind::Directory
            && let Some(seen) = followed_dirs.as_deref()
        {
            let _ = register_directory_identity(&metadata, seen);
        }
        if let Ok(mut state) = state.lock() {
            state.overflowed |=
                add_metadata_to_aggregate(&mut state.root, &metadata, request, false, true);
        }
    }

    let callback_root = root.clone();
    let callback_state = Arc::clone(&state);
    let callback_errors = Arc::clone(&errors);
    let callback_root_seen = root_seen.clone();
    let callback_child_seen = child_seen.clone();
    let callback_followed_dirs = followed_dirs.clone();
    let callback_request = request.clone();
    let max_depth = request.max_depth.unwrap_or(usize::MAX);
    let walker = WalkDir::new(&root)
        .sort(request.symlinks == SymlinkMode::Follow)
        .skip_hidden(!request.show_hidden)
        .follow_links(request.symlinks == SymlinkMode::Follow)
        .max_depth(max_depth)
        .parallelism(scan_parallelism(request))
        .process_read_dir(move |depth, path, _state, children| {
            let Some(depth) = depth else {
                return;
            };
            let current_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                callback_root.join(path)
            };
            let descendant_bucket = if depth == 0 {
                None
            } else {
                current_path
                    .strip_prefix(&callback_root)
                    .ok()
                    .and_then(|relative| relative.components().next())
                    .and_then(|component| match component {
                        Component::Normal(name) => Some(name.to_os_string()),
                        _ => None,
                    })
            };
            if depth != 0 && descendant_bucket.is_none() {
                record_scan_error(&callback_errors);
                return;
            }

            let mut root_delta = Aggregate::default();
            let mut child_deltas = HashMap::<OsString, Aggregate>::new();
            let mut descendant_delta = Aggregate::default();
            let mut local_overflowed = false;

            for child in children.iter_mut().filter_map(|entry| entry.as_mut().ok()) {
                let mut metadata = None;
                if child.file_type().is_dir()
                    && let Some(seen) = callback_followed_dirs.as_deref()
                {
                    let value = match child.metadata() {
                        Ok(metadata) => metadata_snapshot(&metadata),
                        Err(_) => {
                            record_scan_error(&callback_errors);
                            child.read_children_path = None;
                            continue;
                        }
                    };
                    if !register_directory_identity(&value, seen) {
                        child.read_children_path = None;
                        continue;
                    }
                    metadata = Some(value);
                }
                if callback_request.size_mode != SizeMode::None && metadata.is_none() {
                    metadata = match child.metadata() {
                        Ok(metadata) => Some(metadata_snapshot(&metadata)),
                        Err(_) => {
                            record_scan_error(&callback_errors);
                            continue;
                        }
                    };
                }

                let kind = metadata
                    .as_ref()
                    .map(|metadata| metadata.kind)
                    .unwrap_or_else(|| EntryKind::from_file_type(child.file_type()));
                let owned_bucket = descendant_bucket
                    .is_none()
                    .then(|| child.file_name().to_os_string());
                let bucket = descendant_bucket
                    .as_ref()
                    .or(owned_bucket.as_ref())
                    .expect("root entries always provide an owned bucket");
                let (root_includes_size, child_includes_size) = metadata
                    .as_ref()
                    .map(|metadata| {
                        (
                            include_hardlink_size(
                                metadata,
                                callback_request.hardlinks,
                                callback_root_seen.as_deref(),
                            ),
                            include_child_hardlink_size(
                                metadata,
                                callback_request.hardlinks,
                                bucket,
                                callback_child_seen.as_deref(),
                            ),
                        )
                    })
                    .unwrap_or((false, false));

                local_overflowed |= add_entry_to_aggregate(
                    &mut root_delta,
                    kind,
                    metadata.as_ref(),
                    &callback_request,
                    true,
                    root_includes_size,
                );
                let child_delta = if descendant_bucket.is_some() {
                    &mut descendant_delta
                } else {
                    child_deltas
                        .entry(owned_bucket.expect("root entry owns its bucket"))
                        .or_default()
                };
                local_overflowed |= add_entry_to_aggregate(
                    child_delta,
                    kind,
                    metadata.as_ref(),
                    &callback_request,
                    depth != 0,
                    child_includes_size,
                );
            }

            if let Ok(mut state) = callback_state.lock() {
                state.overflowed |= local_overflowed;
                state.overflowed |= merge_aggregate(&mut state.root, &root_delta);
                if let Some(name) = descendant_bucket {
                    let aggregate = state.children.entry(name).or_default();
                    state.overflowed |= merge_aggregate(aggregate, &descendant_delta);
                }
                for (name, delta) in child_deltas {
                    let aggregate = state.children.entry(name).or_default();
                    state.overflowed |= merge_aggregate(aggregate, &delta);
                }
            }
        });

    for result in walker {
        match result {
            Ok(entry) => {
                if entry.read_children_error.is_some() {
                    record_scan_error(&errors);
                }
            }
            Err(_) => record_scan_error(&errors),
        }
    }

    let error_count = errors.load(Ordering::Relaxed);
    let state = match Arc::try_unwrap(state) {
        Ok(state) => state.into_inner().unwrap_or_default(),
        Err(state) => state
            .lock()
            .map(|state| TopLevelScanState {
                root: state.root.clone(),
                children: state.children.clone(),
                overflowed: state.overflowed,
            })
            .unwrap_or_default(),
    };
    TopLevelScanSnapshot {
        root: state.root,
        children: state.children,
        complete: error_count == 0,
        errors: error_count,
        overflowed: state.overflowed,
    }
}

fn record_scan_error(errors: &AtomicU64) {
    let _ = errors.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

fn scan_parallelism(request: &ScanRequest) -> jwalk::Parallelism {
    if request.symlinks == SymlinkMode::Follow || request.threads <= 1 {
        // Followed-directory identity is deliberately resolved in stable walk
        // order so aliases and cycles have one deterministic representative.
        jwalk::Parallelism::Serial
    } else {
        static POOL: OnceLock<ScanPoolCache> = OnceLock::new();
        let cache = POOL.get_or_init(|| Mutex::new(None));
        let Ok(mut cached) = cache.lock() else {
            return jwalk::Parallelism::RayonNewPool(request.threads);
        };
        if let Some((threads, pool)) = cached.as_ref()
            && *threads == request.threads
        {
            return jwalk::Parallelism::RayonExistingPool {
                pool: Arc::clone(pool),
                busy_timeout: Some(Duration::from_secs(1)),
            };
        }
        let Ok(pool) = rayon::ThreadPoolBuilder::new()
            .num_threads(request.threads)
            .build()
        else {
            return jwalk::Parallelism::RayonNewPool(request.threads);
        };
        let pool = Arc::new(pool);
        *cached = Some((request.threads, Arc::clone(&pool)));
        jwalk::Parallelism::RayonExistingPool {
            pool,
            busy_timeout: Some(Duration::from_secs(1)),
        }
    }
}

fn register_directory_identity(
    metadata: &MetadataSnapshot,
    seen: &Mutex<HashSet<HardlinkKey>>,
) -> bool {
    let Some(key) = metadata.hardlink_key() else {
        return true;
    };
    seen.lock().map(|mut seen| seen.insert(key)).unwrap_or(true)
}

fn include_hardlink_size(
    metadata: &MetadataSnapshot,
    mode: HardlinkMode,
    seen: Option<&Mutex<HashSet<HardlinkKey>>>,
) -> bool {
    if mode == HardlinkMode::CountEveryEntry
        || metadata.kind == EntryKind::Directory
        || !metadata.is_hardlink_candidate()
    {
        return true;
    }
    let Some(key) = metadata.hardlink_key() else {
        return true;
    };
    let Some(seen) = seen else {
        return true;
    };
    seen.lock().map(|mut seen| seen.insert(key)).unwrap_or(true)
}

fn include_child_hardlink_size(
    metadata: &MetadataSnapshot,
    mode: HardlinkMode,
    child: &OsString,
    seen: Option<&Mutex<HashMap<OsString, HashSet<HardlinkKey>>>>,
) -> bool {
    if mode == HardlinkMode::CountEveryEntry
        || metadata.kind == EntryKind::Directory
        || !metadata.is_hardlink_candidate()
    {
        return true;
    }
    let Some(key) = metadata.hardlink_key() else {
        return true;
    };
    let Some(seen) = seen else {
        return true;
    };
    seen.lock()
        .map(|mut seen| seen.entry(child.clone()).or_default().insert(key))
        .unwrap_or(true)
}

fn add_metadata_to_aggregate(
    aggregate: &mut Aggregate,
    metadata: &MetadataSnapshot,
    request: &ScanRequest,
    include_count: bool,
    include_size: bool,
) -> bool {
    add_entry_to_aggregate(
        aggregate,
        metadata.kind,
        Some(metadata),
        request,
        include_count,
        include_size,
    )
}

fn add_entry_to_aggregate(
    aggregate: &mut Aggregate,
    kind: EntryKind,
    metadata: Option<&MetadataSnapshot>,
    request: &ScanRequest,
    include_count: bool,
    include_size: bool,
) -> bool {
    let mut overflowed = false;
    if include_count {
        match kind {
            EntryKind::Directory if request.count_dirs => {
                overflowed |= add_value(&mut aggregate.dirs, 1);
            }
            EntryKind::File | EntryKind::Symlink | EntryKind::Other if request.count_files => {
                overflowed |= add_value(&mut aggregate.files, 1);
            }
            _ => {}
        }
    }
    if include_size && let Some(metadata) = metadata {
        match request.size_mode {
            SizeMode::None => {}
            SizeMode::Logical if kind != EntryKind::Directory => {
                overflowed |= add_value(&mut aggregate.logical_size, metadata.logical_size);
            }
            SizeMode::Logical => {}
            SizeMode::Allocated => {
                overflowed |= add_value(&mut aggregate.allocated_size, metadata.allocated_size);
            }
        }
    }
    aggregate.overflowed |= overflowed;
    overflowed
}

fn merge_aggregate(target: &mut Aggregate, source: &Aggregate) -> bool {
    let mut overflowed = source.overflowed;
    overflowed |= add_value(&mut target.logical_size, source.logical_size);
    overflowed |= add_value(&mut target.allocated_size, source.allocated_size);
    overflowed |= add_value(&mut target.files, source.files);
    overflowed |= add_value(&mut target.dirs, source.dirs);
    target.overflowed |= overflowed;
    overflowed
}

fn add_value(target: &mut u64, value: u64) -> bool {
    crate::overflow::checked_add_u64(target, value)
}

pub fn scan(request: &ScanRequest) -> ScanSnapshot {
    let mut snapshot = ScanSnapshot {
        complete: true,
        ..ScanSnapshot::default()
    };
    if request.size_mode == SizeMode::Allocated {
        match root_metadata(&request.root, request.symlinks) {
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
    if request.symlinks == SymlinkMode::DoNotFollow
        && std::fs::symlink_metadata(&request.root)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return snapshot;
    }
    let seen_files = Arc::new(std::sync::Mutex::new(HashSet::<HardlinkKey>::new()));
    let visited_dirs = Arc::new(std::sync::Mutex::new(HashSet::<HardlinkKey>::new()));
    if request.symlinks == SymlinkMode::Follow
        && let Ok(metadata) = root_metadata(&request.root, request.symlinks)
    {
        let snapshot = metadata_snapshot(&metadata);
        if snapshot.kind == EntryKind::Directory {
            let _ = register_directory_identity(&snapshot, &visited_dirs);
        }
    }
    let max_depth = request.max_depth.unwrap_or(usize::MAX);
    let follow_links = request.symlinks == SymlinkMode::Follow;
    let callback_visited_dirs = Arc::clone(&visited_dirs);
    let walker = WalkDir::new(&request.root)
        .sort(follow_links)
        .skip_hidden(!request.show_hidden)
        .follow_links(follow_links)
        .max_depth(max_depth)
        .parallelism(scan_parallelism(request))
        .process_read_dir(move |depth, _path, _state, children| {
            if !follow_links || depth.is_none() {
                return;
            }
            for entry in children.iter_mut().filter_map(|entry| entry.as_mut().ok()) {
                if !entry.file_type().is_dir() {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !register_directory_identity(
                    &metadata_snapshot(&metadata),
                    &callback_visited_dirs,
                ) {
                    entry.read_children_path = None;
                }
            }
        });

    for result in walker.into_iter() {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                snapshot.complete = false;
                snapshot.errors = snapshot.errors.saturating_add(1);
                continue;
            }
        };
        if entry.read_children_error.is_some() {
            snapshot.complete = false;
            snapshot.errors = snapshot.errors.saturating_add(1);
        }
        if entry.depth() == 0 {
            continue;
        }
        if follow_links && entry.file_type().is_dir() && entry.read_children_path.is_none() {
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
                if request.count_dirs && crate::overflow::checked_add_u64(&mut aggregate.dirs, 1) {
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
                if request.count_files && crate::overflow::checked_add_u64(&mut aggregate.files, 1)
                {
                    aggregate.overflowed = true;
                    overflowed = true;
                }
                let size_overflowed = match request.size_mode {
                    SizeMode::None => false,
                    SizeMode::Logical => crate::overflow::checked_add_u64(
                        &mut aggregate.logical_size,
                        metadata.logical_size,
                    ),
                    SizeMode::Allocated => crate::overflow::checked_add_u64(
                        &mut aggregate.allocated_size,
                        metadata.allocated_size,
                    ),
                };
                if size_overflowed {
                    aggregate.overflowed = true;
                    overflowed = true;
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

fn root_metadata(path: &Path, symlinks: SymlinkMode) -> std::io::Result<std::fs::Metadata> {
    match symlinks {
        SymlinkMode::DoNotFollow => std::fs::symlink_metadata(path),
        SymlinkMode::Follow => std::fs::metadata(path),
    }
}

#[allow(dead_code)]
fn _scan_duration_hint() -> Duration {
    Duration::from_secs(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_metadata(logical_size: u64, allocated_size: u64) -> MetadataSnapshot {
        MetadataSnapshot {
            kind: EntryKind::File,
            logical_size,
            allocated_size,
            device: Some(1),
            inode: Some(1),
            links: Some(1),
            mode: Some(0o100644),
            modified: None,
        }
    }

    #[test]
    fn aggregate_updates_saturate_and_propagate_overflow() {
        let request = ScanRequest {
            size_mode: SizeMode::Allocated,
            count_files: true,
            ..ScanRequest::default()
        };
        let mut aggregate = Aggregate {
            allocated_size: u64::MAX - 1,
            files: u64::MAX,
            ..Aggregate::default()
        };
        let overflowed =
            add_metadata_to_aggregate(&mut aggregate, &file_metadata(7, 2), &request, true, true);
        assert!(overflowed);
        assert!(aggregate.overflowed);
        assert_eq!(aggregate.allocated_size, u64::MAX);
        assert_eq!(aggregate.files, u64::MAX);

        let mut merged = Aggregate {
            logical_size: u64::MAX - 2,
            dirs: u64::MAX,
            ..Aggregate::default()
        };
        let source = Aggregate {
            logical_size: 3,
            dirs: 1,
            overflowed: true,
            ..Aggregate::default()
        };
        assert!(merge_aggregate(&mut merged, &source));
        assert!(merged.overflowed);
        assert_eq!(merged.logical_size, u64::MAX);
        assert_eq!(merged.dirs, u64::MAX);

        let state = TopLevelScanState {
            root: merged,
            children: HashMap::new(),
            overflowed: overflowed || source.overflowed,
        };
        let snapshot = TopLevelScanSnapshot {
            root: state.root,
            children: state.children,
            complete: true,
            errors: 0,
            overflowed: state.overflowed,
        };
        assert!(snapshot.overflowed);
        assert!(snapshot.root.overflowed);
    }
}
