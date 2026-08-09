//! Source scanning, destination indexing, and transfer-plan construction.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::field_reassign_with_default)]

use crate::domain::{
    ChangeItem, ChangeKind, DstObjKind, FileRelationBreakdown, ManifestDeleteDirEntry,
    ManifestDeleteEntry, ManifestDirTimeEntry, ManifestFileEntry, MediaKind, MergeCollisionPolicy,
    PreScan, TransferManifest,
};
use crate::plan::{
    classify_file_relation, realpath_allow_missing, regular_file_collision_change,
    sync_regular_file_change,
};
use crate::runtime::{dev_media_kind, symlink_targets_equal};
use filetime::FileTime;
use jwalk::WalkDir;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[derive(Default, Clone, Copy)]
pub(crate) struct TreeCounts {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) dirs: u64,
}

pub(crate) fn count_tree_any(path: &Path, include_root_dir: bool) -> TreeCounts {
    let root_meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return TreeCounts::default(),
    };
    if root_meta.is_file() {
        return TreeCounts {
            files: 1,
            bytes: root_meta.len(),
            dirs: 0,
        };
    }
    if !root_meta.is_dir() {
        return TreeCounts::default();
    }
    let mut counts = TreeCounts {
        dirs: u64::from(include_root_dir),
        ..TreeCounts::default()
    };
    for entry in WalkDir::new(path)
        .sort(false)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() > 0)
    {
        let file_type = entry.file_type();
        if file_type.is_dir() {
            counts.dirs = counts.dirs.saturating_add(1);
        } else if file_type.is_file() {
            counts.files = counts.files.saturating_add(1);
            if let Ok(meta) = entry.metadata() {
                counts.bytes = counts.bytes.saturating_add(meta.len());
            }
        }
    }
    counts
}

pub(crate) fn top_level_rel_component(rel: &str) -> Option<&str> {
    let trimmed = rel.trim_start_matches("./").trim_start_matches('/');
    let first = trimmed.split('/').next().unwrap_or("");
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

pub(crate) fn rel_matches_prefix(rel: &str, prefix: &str) -> bool {
    rel == prefix
        || rel
            .strip_prefix(prefix)
            .map(|suffix| suffix.starts_with('/'))
            .unwrap_or(false)
}

pub(crate) fn destination_file_counts(
    destination_root: &Path,
    source_rel_files: &HashSet<String>,
) -> (u64, u64) {
    if !destination_root.is_dir() {
        return (0, 0);
    }
    let idx = build_destination_index(destination_root);
    let total = idx
        .entries
        .values()
        .filter(|entry| entry.kind == DestinationKind::Regular)
        .count() as u64;
    let uncollided = idx
        .entries
        .iter()
        .filter(|(rel, entry)| {
            entry.kind == DestinationKind::Regular && !source_rel_files.contains(*rel)
        })
        .count() as u64;
    (total, uncollided)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestinationKind {
    Regular,
    Directory,
    Symlink,
}

#[derive(Clone)]
pub(crate) struct DestinationEntry {
    pub(crate) kind: DestinationKind,
    pub(crate) size: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) mtime: Option<SystemTime>,
    pub(crate) link_target: Option<PathBuf>,
}

pub(crate) struct DestinationIndex {
    pub(crate) entries: FxHashMap<String, DestinationEntry>,
    pub(crate) complete: bool,
}

impl Default for DestinationIndex {
    fn default() -> Self {
        Self {
            entries: FxHashMap::default(),
            complete: true,
        }
    }
}

impl DestinationIndex {
    pub(crate) fn path_exists(&self, rel: &str) -> bool {
        self.entries.contains_key(rel)
    }
}

pub(crate) fn build_destination_index(destination_root: &Path) -> DestinationIndex {
    if !destination_root.is_dir() {
        return DestinationIndex::default();
    }

    let mut entries: FxHashMap<String, DestinationEntry> = FxHashMap::default();
    let mut complete = true;

    for result in WalkDir::new(destination_root)
        .sort(false)
        .skip_hidden(false)
        .into_iter()
    {
        let ent = match result {
            Ok(ent) => ent,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let rel = ent
            .path()
            .strip_prefix(destination_root)
            .ok()
            .filter(|p| path_components_are_utf8(p))
            .map(normalize_rel)
            .unwrap_or_default();
        if rel.is_empty() {
            if ent.depth() > 0 && !path_components_are_utf8(&ent.path()) {
                complete = false;
            }
            continue;
        }

        let fty = ent.file_type();
        if fty.is_file() {
            let metadata = ent
                .metadata()
                .or_else(|_| fs::symlink_metadata(ent.path()))
                .ok();
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = metadata.as_ref().and_then(|m| m.modified().ok());
            entries.insert(
                rel,
                DestinationEntry {
                    kind: DestinationKind::Regular,
                    size,
                    dev: metadata.as_ref().map(MetadataExt::dev).unwrap_or(0),
                    ino: metadata.as_ref().map(MetadataExt::ino).unwrap_or(0),
                    mtime,
                    link_target: None,
                },
            );
        } else if fty.is_dir() && ent.depth() > 0 {
            let metadata = ent
                .metadata()
                .or_else(|_| fs::symlink_metadata(ent.path()))
                .ok();
            entries.insert(
                rel,
                DestinationEntry {
                    kind: DestinationKind::Directory,
                    size: 0,
                    dev: metadata.as_ref().map(MetadataExt::dev).unwrap_or(0),
                    ino: metadata.as_ref().map(MetadataExt::ino).unwrap_or(0),
                    mtime: metadata.as_ref().and_then(|m| m.modified().ok()),
                    link_target: None,
                },
            );
        } else if fty.is_symlink() {
            entries.insert(
                rel,
                DestinationEntry {
                    kind: DestinationKind::Symlink,
                    size: 0,
                    dev: 0,
                    ino: 0,
                    mtime: None,
                    link_target: fs::read_link(ent.path()).ok(),
                },
            );
        } else {
            complete = false;
        }
    }

    DestinationIndex { entries, complete }
}

pub(crate) fn add_parent_dir_chain(rel: &str, include_root: bool, out: &mut FxHashSet<String>) {
    if rel.is_empty() {
        return;
    }
    let mut cur = rel;
    loop {
        match cur.rfind('/') {
            Some(idx) => {
                let parent = &cur[..idx];
                if parent.is_empty() {
                    if include_root {
                        out.insert(String::new());
                    }
                    break;
                }
                out.insert(parent.to_string());
                cur = parent;
            }
            None => {
                if include_root {
                    out.insert(String::new());
                }
                break;
            }
        }
    }
}

pub(crate) fn normalize_rel(path: &Path) -> String {
    // Join actual path components so a literal backslash in a Unix filename
    // is preserved instead of being mistaken for a directory separator.
    path.iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn path_components_are_utf8(path: &Path) -> bool {
    path.iter().all(|component| component.to_str().is_some())
}

pub(crate) fn map_dir_dest_path(
    include_root: bool,
    src_base: &str,
    rel: &str,
    dst_base: &Path,
) -> PathBuf {
    if include_root {
        if rel.is_empty() {
            dst_base.join(src_base)
        } else {
            dst_base.join(src_base).join(rel)
        }
    } else if rel.is_empty() {
        dst_base.to_path_buf()
    } else {
        dst_base.join(rel)
    }
}

pub(crate) fn map_display_rel(include_root: bool, src_base: &str, rel: &str) -> String {
    if include_root {
        if rel.is_empty() {
            format!("{src_base}/")
        } else {
            format!("{src_base}/{rel}")
        }
    } else if rel.is_empty() {
        String::new()
    } else {
        rel.to_string()
    }
}

pub(crate) fn bounded_preview_change(
    rel: String,
    kind: ChangeKind,
    depth: Option<usize>,
) -> (String, ChangeKind) {
    let Some(depth) = depth.filter(|depth| *depth > 0) else {
        return (rel, kind);
    };
    let components: Vec<&str> = rel.trim_end_matches('/').split('/').collect();
    if components.len() <= depth {
        return (rel, kind);
    }
    let mut bounded = components[..depth].join("/");
    bounded.push('/');
    let bounded_kind = match kind {
        ChangeKind::NewFile => ChangeKind::NewDir,
        ChangeKind::RemovedFile => ChangeKind::RemovedDir,
        other => other,
    };
    (bounded, bounded_kind)
}

pub(crate) fn insert_preview_change(
    changes: &mut FxHashMap<String, ChangeKind>,
    rel: String,
    kind: ChangeKind,
    depth: Option<usize>,
) {
    let (rel, kind) = bounded_preview_change(rel, kind, depth);
    changes
        .entry(rel)
        .and_modify(|existing| {
            if matches!(kind, ChangeKind::ModFile) {
                *existing = kind;
            }
        })
        .or_insert(kind);
}

pub(crate) fn map_dir_dest(
    include_root: bool,
    src_base: &str,
    rel: &str,
    dst_base: &Path,
) -> (PathBuf, String) {
    (
        map_dir_dest_path(include_root, src_base, rel, dst_base),
        map_display_rel(include_root, src_base, rel),
    )
}

pub(crate) fn ensure_dst_file_path<'a>(
    dst_file: &'a mut Option<PathBuf>,
    include_root: bool,
    src_base: &str,
    rel: &str,
    dst_base: &Path,
) -> &'a Path {
    dst_file
        .get_or_insert_with(|| map_dir_dest_path(include_root, src_base, rel, dst_base))
        .as_path()
}

pub(crate) fn parent_rel_in_set(rel: &str, set: &FxHashSet<String>) -> bool {
    if set.is_empty() {
        return false;
    }
    match rel.rfind('/') {
        Some(idx) => set.contains(&rel[..idx]),
        None => false,
    }
}

pub(crate) fn pre_scan_new_tree_lite(
    src_root: &Path,
    include_root: bool,
    src_base: &str,
    exclude_rel: Option<&str>,
    build_source_display_paths: bool,
    bounded_preview_depth: Option<usize>,
) -> PreScan {
    let mut out = PreScan {
        planned_bytes_exact: false,
        ..PreScan::default()
    };
    let mut preview: FxHashMap<String, ChangeKind> = FxHashMap::default();
    if include_root && !src_base.is_empty() {
        insert_preview_change(
            &mut preview,
            format!("{src_base}/"),
            ChangeKind::NewDir,
            bounded_preview_depth,
        );
    }

    let walker = WalkDir::new(src_root)
        .sort(false)
        .skip_hidden(false)
        .parallelism(jwalk::Parallelism::RayonDefaultPool {
            busy_timeout: Duration::from_secs(1),
        });
    let mut files = 0u64;
    let mut dirs = u64::from(include_root);
    let mut scan_complete = true;
    for result in walker.into_iter() {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                scan_complete = false;
                continue;
            }
        };
        if !path_components_are_utf8(&entry.path()) {
            scan_complete = false;
            continue;
        }
        if entry.depth() == 0 {
            continue;
        }
        let needs_rel = exclude_rel.is_some()
            || build_source_display_paths
            || bounded_preview_depth
                .map(|depth| entry.depth() <= depth + usize::from(include_root))
                .unwrap_or(true);
        let rel = needs_rel
            .then(|| entry.path().strip_prefix(src_root).ok().map(normalize_rel))
            .flatten();
        if rel
            .as_deref()
            .zip(exclude_rel)
            .map(|(rel, prefix)| rel_matches_prefix(rel, prefix))
            .unwrap_or(false)
        {
            continue;
        }
        let file_type = entry.file_type();
        let is_dir = file_type.is_dir();
        let is_symlink = file_type.is_symlink();
        if is_dir {
            dirs = dirs.saturating_add(1);
        } else if !is_symlink {
            files = files.saturating_add(1);
        }
        if !is_dir && !is_symlink && !file_type.is_file() {
            scan_complete = false;
            continue;
        }
        if let Some(rel) = rel {
            let display = map_display_rel(include_root, src_base, &rel);
            if build_source_display_paths {
                out.source_display_paths
                    .insert(display.trim_end_matches('/').to_string());
            }
            insert_preview_change(
                &mut preview,
                if is_dir {
                    format!("{}/", display.trim_end_matches('/'))
                } else {
                    display
                },
                if is_dir {
                    ChangeKind::NewDir
                } else {
                    ChangeKind::NewFile
                },
                bounded_preview_depth,
            );
        }
    }
    out.scan_complete = scan_complete;
    out.total_regular_files = Some(files);
    out.total_regular_bytes = None;
    out.total_dirs = Some(dirs);
    out.add_files = files;
    out.add_dirs = dirs;
    out.has_itemized_changes = !preview.is_empty();
    out.change_preview.extend(
        preview
            .into_iter()
            .map(|(rel, kind)| ChangeItem { kind, rel }),
    );
    out
}

pub(crate) struct ScannedFileEntry {
    rel: Arc<str>,
    source_path: Option<PathBuf>,
    size: u64,
    is_symlink: bool,
    dev: u64,
    ino: u64,
    nlink: u64,
    mtime: Option<SystemTime>,
}

type SrcScanEntries = (
    Vec<String>,
    Vec<ScannedFileEntry>,
    Vec<ManifestDirTimeEntry>,
    bool,
);

pub(crate) fn scan_source_entries(
    src_root: &Path,
    exclude_rel: Option<&str>,
    parallel_directory_walk: bool,
    collect_dir_times: bool,
) -> SrcScanEntries {
    if parallel_directory_walk {
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut dir_times = Vec::new();
        let mut scan_complete = true;
        for result in WalkDir::new(src_root)
            .sort(false)
            .skip_hidden(false)
            .parallelism(jwalk::Parallelism::RayonDefaultPool {
                busy_timeout: Duration::from_secs(1),
            })
            .into_iter()
        {
            let entry = match result {
                Ok(entry) => entry,
                Err(_) => {
                    scan_complete = false;
                    continue;
                }
            };
            let path = entry.path();
            if !path_components_are_utf8(&path) {
                scan_complete = false;
                continue;
            }
            let rel = match path.strip_prefix(src_root) {
                Ok(rel) => normalize_rel(rel),
                Err(_) => {
                    scan_complete = false;
                    continue;
                }
            };
            if exclude_rel
                .map(|prefix| rel_matches_prefix(&rel, prefix))
                .unwrap_or(false)
            {
                continue;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => {
                    scan_complete = false;
                    continue;
                }
            };
            if meta.is_dir() {
                if collect_dir_times {
                    dir_times.push(ManifestDirTimeEntry {
                        rel: rel.clone(),
                        atime: FileTime::from_last_access_time(&meta),
                        mtime: FileTime::from_last_modification_time(&meta),
                    });
                }
                if !rel.is_empty() {
                    dirs.push(rel);
                }
            } else if meta.is_file() {
                files.push(ScannedFileEntry {
                    rel: rel.into(),
                    source_path: None,
                    size: meta.len(),
                    is_symlink: false,
                    dev: meta.dev(),
                    ino: meta.ino(),
                    nlink: meta.nlink(),
                    mtime: meta.modified().ok(),
                });
            } else if meta.file_type().is_symlink() {
                files.push(ScannedFileEntry {
                    rel: rel.into(),
                    source_path: Some(path),
                    size: 0,
                    is_symlink: true,
                    dev: meta.dev(),
                    ino: meta.ino(),
                    nlink: meta.nlink(),
                    mtime: None,
                });
            } else {
                scan_complete = false;
            }
        }
        return (dirs, files, dir_times, scan_complete);
    }
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<ScannedFileEntry> = Vec::new();
    let mut dir_times: Vec<ManifestDirTimeEntry> = Vec::new();

    fn walk_source_entries(
        current: &Path,
        current_meta: Option<fs::Metadata>,
        rel: &str,
        exclude_rel: Option<&str>,
        dirs: &mut Vec<String>,
        files: &mut Vec<ScannedFileEntry>,
        dir_times: &mut Vec<ManifestDirTimeEntry>,
        collect_dir_times: bool,
    ) -> bool {
        let mut complete = true;
        if !path_components_are_utf8(current) {
            return false;
        }
        let meta = match current_meta {
            Some(meta) => meta,
            None => match fs::symlink_metadata(current) {
                Ok(m) => m,
                Err(_) => return false,
            },
        };
        if meta.is_dir() {
            if collect_dir_times {
                dir_times.push(ManifestDirTimeEntry {
                    rel: rel.to_string(),
                    atime: FileTime::from_last_access_time(&meta),
                    mtime: FileTime::from_last_modification_time(&meta),
                });
            }
            if !rel.is_empty() {
                dirs.push(rel.to_string());
            }
            let rd = match fs::read_dir(current) {
                Ok(v) => v,
                Err(_) => return false,
            };
            for result in rd {
                let entry = match result {
                    Ok(entry) => entry,
                    Err(_) => {
                        complete = false;
                        continue;
                    }
                };
                let child_path = entry.path();
                let child_name = match child_path.file_name().and_then(|name| name.to_str()) {
                    Some(n) => n.to_string(),
                    None => {
                        complete = false;
                        continue;
                    }
                };
                let child_rel = if rel.is_empty() {
                    child_name.clone()
                } else {
                    format!("{rel}/{child_name}")
                };
                if exclude_rel
                    .map(|prefix| rel_matches_prefix(&child_rel, prefix))
                    .unwrap_or(false)
                {
                    continue;
                }
                let child_meta = match fs::symlink_metadata(&child_path) {
                    Ok(m) => m,
                    Err(_) => {
                        complete = false;
                        continue;
                    }
                };
                if child_meta.is_dir() {
                    complete &= walk_source_entries(
                        &child_path,
                        Some(child_meta),
                        &child_rel,
                        exclude_rel,
                        dirs,
                        files,
                        dir_times,
                        collect_dir_times,
                    );
                } else if child_meta.is_file() {
                    files.push(ScannedFileEntry {
                        rel: child_rel.into(),
                        source_path: None,
                        size: child_meta.len(),
                        is_symlink: false,
                        dev: child_meta.dev(),
                        ino: child_meta.ino(),
                        nlink: child_meta.nlink(),
                        mtime: child_meta.modified().ok(),
                    });
                } else if child_meta.file_type().is_symlink() {
                    files.push(ScannedFileEntry {
                        rel: child_rel.into(),
                        source_path: Some(child_path),
                        size: 0,
                        is_symlink: true,
                        dev: child_meta.dev(),
                        ino: child_meta.ino(),
                        nlink: child_meta.nlink(),
                        mtime: None,
                    });
                } else {
                    complete = false;
                }
            }
        }
        complete
    }

    let scan_complete = walk_source_entries(
        src_root,
        None,
        "",
        exclude_rel,
        &mut dirs,
        &mut files,
        &mut dir_times,
        collect_dir_times,
    );

    (dirs, files, dir_times, scan_complete)
}

pub(crate) fn pre_scan_directory(
    src_path: &str,
    dst_path: &str,
    src_mnt: &Path,
    build_manifest: bool,
    retain_identical_manifest: bool,
    build_source_display_paths: bool,
    collect_file_relation_breakdown: bool,
    sync_mode: bool,
    replace_dest_symlink: bool,
    merge_collision_policy: MergeCollisionPolicy,
    exclude_rel: Option<&str>,
    bounded_preview_depth: Option<usize>,
    preview_lite: bool,
) -> PreScan {
    let src_no_trailing = src_path.trim_end_matches('/');
    let include_root = !src_path.ends_with('/');
    let src_root = Path::new(src_no_trailing);
    let dst_base = Path::new(dst_path.trim_end_matches('/'));
    let src_base = match src_mnt.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => {
            return PreScan {
                scan_complete: false,
                ..PreScan::default()
            }
        }
    };

    let destination_root = if include_root {
        dst_base.join(&src_base)
    } else {
        dst_base.to_path_buf()
    };
    let destination_missing = !destination_root.exists();

    if preview_lite && destination_missing {
        return pre_scan_new_tree_lite(
            src_root,
            include_root,
            &src_base,
            exclude_rel,
            build_source_display_paths,
            bounded_preview_depth,
        );
    }

    // Strict fast-path for non-verbose preview when destination root is missing:
    // all source content is guaranteed "new", so skip destination path construction/stat checks.
    let src_dev = fs::metadata(src_root).ok().map(|m| m.dev());
    let dst_dev = fs::metadata(&destination_root).ok().map(|m| m.dev());
    let source_media = dev_media_kind(src_root);
    let destination_media = dev_media_kind(&destination_root);
    // Directory atime must be captured before read_dir updates it. Parallel
    // walkers may enumerate a directory before yielding its parent entry, so
    // use the ordered walker whenever metadata preservation is requested.
    let parallel_source_scan = source_media != MediaKind::Hdd && !build_manifest;
    let can_parallel_scans = !destination_missing
        && src_dev.is_some()
        && dst_dev.is_some()
        && (src_dev != dst_dev
            || (source_media == MediaKind::Nvme && destination_media == MediaKind::Nvme));
    let (mut dirs, files, mut dir_times, source_scan_complete, destination_index): (
        Vec<String>,
        Vec<ScannedFileEntry>,
        Vec<ManifestDirTimeEntry>,
        bool,
        Option<DestinationIndex>,
    ) = if destination_missing {
        let (d, f, t, scan_complete) =
            scan_source_entries(src_root, exclude_rel, parallel_source_scan, build_manifest);
        (d, f, t, scan_complete, None)
    } else if can_parallel_scans {
        std::thread::scope(|scope| {
            let idx_handle = scope.spawn(|| build_destination_index(&destination_root));
            let (d, f, t, scan_complete) =
                scan_source_entries(src_root, exclude_rel, parallel_source_scan, build_manifest);
            let idx = idx_handle
                .join()
                .unwrap_or_else(|_| build_destination_index(&destination_root));
            (d, f, t, scan_complete, Some(idx))
        })
    } else {
        let (d, f, t, scan_complete) =
            scan_source_entries(src_root, exclude_rel, parallel_source_scan, build_manifest);
        let idx = build_destination_index(&destination_root);
        (d, f, t, scan_complete, Some(idx))
    };

    let mut out = PreScan::default();
    out.scan_complete = source_scan_complete
        && destination_index
            .as_ref()
            .map(|index| index.complete)
            .unwrap_or(true);
    out.total_regular_files = Some(files.iter().filter(|entry| !entry.is_symlink).count() as u64);
    out.total_regular_bytes = Some(
        files
            .iter()
            .filter(|entry| !entry.is_symlink)
            .map(|entry| entry.size)
            .sum(),
    );
    if build_source_display_paths {
        let mut source_display_paths: FxHashSet<String> = FxHashSet::default();
        let within_display_depth = |key: &str| {
            bounded_preview_depth
                .map(|depth| key.split('/').count() <= depth)
                .unwrap_or(true)
        };
        if include_root && !src_base.is_empty() {
            source_display_paths.insert(src_base.clone());
        }
        for rel in &dirs {
            let mapped = map_display_rel(include_root, &src_base, rel);
            let key = mapped.trim_end_matches('/').to_string();
            if !key.is_empty() && within_display_depth(&key) {
                source_display_paths.insert(key);
            }
        }
        for entry in &files {
            let mapped = map_display_rel(include_root, &src_base, &entry.rel);
            let key = mapped.trim_end_matches('/').to_string();
            if !key.is_empty() && within_display_depth(&key) {
                source_display_paths.insert(key);
            }
        }
        out.source_display_paths = source_display_paths;
    }
    let source_rel_dirs: FxHashSet<String> = dirs.iter().cloned().collect();
    let source_rel_files: FxHashSet<String> = if sync_mode {
        files.iter().map(|entry| entry.rel.to_string()).collect()
    } else {
        FxHashSet::default()
    };
    let root_new_dir = include_root && !destination_root.is_dir();
    out.total_dirs = Some(dirs.len() as u64 + u64::from(include_root));

    let mut directory_preview_changes: FxHashMap<String, ChangeKind> = FxHashMap::default();
    if include_root && destination_missing {
        let (dst_root, display_rel) = map_dir_dest(true, &src_base, "", dst_base);
        if !dst_root.is_dir() && !display_rel.is_empty() {
            insert_preview_change(
                &mut directory_preview_changes,
                display_rel,
                ChangeKind::NewDir,
                bounded_preview_depth,
            );
            out.has_itemized_changes = true;
        }
    }

    let mut missing_dir_prefixes: FxHashSet<String> = FxHashSet::default();
    dirs.sort_by_cached_key(|rel| {
        (
            rel.bytes().filter(|byte| *byte == b'/').count(),
            rel.clone(),
        )
    });

    for rel in &dirs {
        let parent_missing = match rel.rfind('/') {
            Some(idx) => missing_dir_prefixes.contains(&rel[..idx]),
            None => false,
        };
        let dir_missing = if destination_missing || parent_missing {
            true
        } else if let Some(idx) = destination_index.as_ref() {
            !matches!(
                idx.entries.get(rel.as_str()).map(|entry| entry.kind),
                Some(DestinationKind::Directory)
            )
        } else {
            !map_dir_dest_path(include_root, &src_base, rel, dst_base).is_dir()
        };
        if dir_missing {
            missing_dir_prefixes.insert(rel.clone());
            let display_rel = map_display_rel(include_root, &src_base, rel);
            let rel_dir = format!("{display_rel}/").replace("//", "/");
            insert_preview_change(
                &mut directory_preview_changes,
                rel_dir,
                ChangeKind::NewDir,
                bounded_preview_depth,
            );
            out.has_itemized_changes = true;
        }
    }

    let source_dir_count = dirs.len();
    let mut manifest_dirs = if build_manifest { Some(dirs) } else { None };

    type FileReduce = (
        u64,
        u64,
        u64,
        FxHashMap<String, ChangeKind>,
        Vec<ManifestFileEntry>,
        Vec<ManifestFileEntry>,
        FxHashSet<String>,
        u64,
        FileRelationBreakdown,
    );
    let has_missing_subtrees = !missing_dir_prefixes.is_empty();
    let (
        add_files,
        mod_files,
        planned_bytes,
        detailed_changes,
        mut manifest_copy_files,
        mut manifest_identical_files,
        mut changed_parent_dirs,
        _overlap_count,
        file_relation_breakdown,
    ): FileReduce = files
        .par_iter()
        .fold(
            || {
                (
                    0,
                    0,
                    0,
                    FxHashMap::default(),
                    Vec::new(),
                    Vec::new(),
                    FxHashSet::default(),
                    0,
                    FileRelationBreakdown::default(),
                )
            },
            |mut acc, entry| {
                let rel = &entry.rel;
                let src_file = &entry.source_path;
                let size = entry.size;
                let is_symlink = entry.is_symlink;
                let dst_idx = destination_index.as_ref();
                let mut dst_file: Option<PathBuf> = None;
                let change = if destination_missing
                    || (has_missing_subtrees && parent_rel_in_set(rel, &missing_dir_prefixes))
                {
                    Some(ChangeKind::NewFile)
                } else if is_symlink {
                    match src_file.as_deref() {
                        Some(src_link) => {
                            if let Some(idx) = dst_idx {
                                if matches!(
                                    idx.entries.get(rel.as_ref()).map(|entry| entry.kind),
                                    Some(DestinationKind::Symlink)
                                ) {
                                    if symlink_targets_equal(
                                        src_link,
                                        ensure_dst_file_path(
                                            &mut dst_file,
                                            include_root,
                                            &src_base,
                                            rel,
                                            dst_base,
                                        ),
                                    ) {
                                        None
                                    } else {
                                        Some(ChangeKind::ModFile)
                                    }
                                } else if idx.path_exists(rel.as_ref()) {
                                    Some(ChangeKind::ModFile)
                                } else {
                                    Some(ChangeKind::NewFile)
                                }
                            } else {
                                match fs::symlink_metadata(ensure_dst_file_path(
                                    &mut dst_file,
                                    include_root,
                                    &src_base,
                                    rel,
                                    dst_base,
                                )) {
                                    Ok(dm)
                                        if dm.file_type().is_symlink()
                                            && symlink_targets_equal(
                                                src_link,
                                                ensure_dst_file_path(
                                                    &mut dst_file,
                                                    include_root,
                                                    &src_base,
                                                    rel,
                                                    dst_base,
                                                ),
                                            ) =>
                                    {
                                        None
                                    }
                                    Ok(_) => Some(ChangeKind::ModFile),
                                    Err(_) => Some(ChangeKind::NewFile),
                                }
                            }
                        }
                        None => Some(ChangeKind::ModFile),
                    }
                } else {
                    let needs_mtime = sync_mode
                        || merge_collision_policy.requires_mtime()
                        || collect_file_relation_breakdown;
                    let src_mtime = needs_mtime.then_some(entry.mtime).flatten();
                    let dst_exists = if let Some(idx) = dst_idx {
                        idx.path_exists(rel.as_ref())
                    } else {
                        fs::symlink_metadata(ensure_dst_file_path(
                            &mut dst_file,
                            include_root,
                            &src_base,
                            rel,
                            dst_base,
                        ))
                        .is_ok()
                    };
                    let dst_path =
                        ensure_dst_file_path(&mut dst_file, include_root, &src_base, rel, dst_base);
                    let dst_entry = dst_idx.and_then(|idx| idx.entries.get(rel.as_ref()));
                    let dst_is_symlink = if let Some(entry) = dst_entry {
                        entry.kind == DestinationKind::Symlink
                    } else {
                        fs::symlink_metadata(dst_path)
                            .map(|dm| dm.file_type().is_symlink())
                            .unwrap_or(false)
                    };
                    let dst_size = if replace_dest_symlink && dst_is_symlink {
                        None
                    } else if let Some(entry) = dst_entry {
                        (entry.kind == DestinationKind::Regular).then_some(entry.size)
                    } else {
                        match fs::symlink_metadata(dst_path) {
                            Ok(dm) if dm.is_file() => fs::metadata(dst_path).ok().map(|m| m.len()),
                            Ok(_) => None,
                            Err(_) => None,
                        }
                    };
                    let dst_mtime = if replace_dest_symlink && dst_is_symlink {
                        None
                    } else if needs_mtime {
                        dst_entry.and_then(|entry| entry.mtime).or_else(|| {
                            if dst_idx.is_none() {
                                fs::metadata(dst_path).ok().and_then(|m| m.modified().ok())
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    };
                    if collect_file_relation_breakdown && dst_exists && !dst_is_symlink {
                        if let Some(breakdown) =
                            classify_file_relation(size, src_mtime, dst_size, dst_mtime)
                        {
                            acc.8.add_assign(breakdown);
                        }
                    }
                    if sync_mode {
                        sync_regular_file_change(
                            size,
                            src_mtime,
                            dst_exists && !dst_is_symlink && dst_size.is_some(),
                            dst_size,
                            dst_mtime,
                        )
                    } else {
                        regular_file_collision_change(
                            merge_collision_policy,
                            size,
                            src_mtime,
                            dst_exists,
                            dst_size,
                            dst_mtime,
                        )
                    }
                };
                let is_overlap = !matches!(change, Some(ChangeKind::NewFile));
                if is_overlap {
                    acc.7 += 1;
                }
                if let Some(kind) = change {
                    if !is_symlink {
                        match kind {
                            ChangeKind::NewFile => acc.0 += 1,
                            _ => acc.1 += 1,
                        }
                        acc.2 += size;
                    }
                    if build_manifest {
                        acc.4.push(ManifestFileEntry {
                            rel: rel.clone(),
                            size,
                            dev: entry.dev,
                            ino: entry.ino,
                            nlink: entry.nlink,
                            is_symlink,
                            mtime: entry.mtime,
                        });
                    }
                    {
                        let display_rel = map_display_rel(include_root, &src_base, rel);
                        insert_preview_change(&mut acc.3, display_rel, kind, bounded_preview_depth);
                    }
                    add_parent_dir_chain(rel, include_root, &mut acc.6);
                } else if build_manifest && retain_identical_manifest {
                    acc.5.push(ManifestFileEntry {
                        rel: rel.clone(),
                        size,
                        dev: entry.dev,
                        ino: entry.ino,
                        nlink: entry.nlink,
                        is_symlink,
                        mtime: entry.mtime,
                    });
                }
                acc
            },
        )
        .reduce(
            || {
                (
                    0,
                    0,
                    0,
                    FxHashMap::default(),
                    Vec::new(),
                    Vec::new(),
                    FxHashSet::default(),
                    0,
                    FileRelationBreakdown::default(),
                )
            },
            |mut a, b| {
                a.0 += b.0;
                a.1 += b.1;
                a.2 += b.2;
                for (rel, kind) in b.3 {
                    insert_preview_change(&mut a.3, rel, kind, None);
                }
                a.4.extend(b.4);
                a.5.extend(b.5);
                a.6.extend(b.6);
                a.7 += b.7;
                a.8.add_assign(b.8);
                a
            },
        );

    out.add_files += add_files;
    out.mod_files += mod_files;
    out.planned_bytes += planned_bytes;

    if destination_missing {
        out.uncollided_files = 0;
    } else if let Some(idx) = destination_index.as_ref() {
        let dest_total_files = idx
            .entries
            .values()
            .filter(|entry| entry.kind == DestinationKind::Regular)
            .count() as u64;
        let source_regular_total = out.total_regular_files.unwrap_or(0);
        let overlap_files = source_regular_total.saturating_sub(add_files);
        let uncollided_by_overlap = dest_total_files.saturating_sub(overlap_files);
        out.uncollided_files = uncollided_by_overlap;
    } else {
        out.scan_complete = false;
        out.uncollided_files = 0;
    }

    out.add_dirs = missing_dir_prefixes.len() as u64 + u64::from(root_new_dir);
    for rel in &missing_dir_prefixes {
        add_parent_dir_chain(rel, include_root, &mut changed_parent_dirs);
    }
    let mod_dirs_count = changed_parent_dirs
        .iter()
        .filter(|rel| {
            if rel.is_empty() {
                include_root && !root_new_dir
            } else {
                source_rel_dirs.contains(rel.as_str())
                    && !missing_dir_prefixes.contains(rel.as_str())
            }
        })
        .count() as u64;
    let total_dirs = out.total_dirs.unwrap_or(0);
    out.mod_dirs = mod_dirs_count.min(total_dirs.saturating_sub(out.add_dirs));
    if destination_missing {
        out.uncollided_dirs = 0;
    } else if let Some(idx) = destination_index.as_ref() {
        let dest_total_dirs = idx
            .entries
            .values()
            .filter(|entry| entry.kind == DestinationKind::Directory)
            .count() as u64;
        let uncollided_dirs_by_scan = idx
            .entries
            .iter()
            .filter(|(rel, entry)| {
                entry.kind == DestinationKind::Directory && !source_rel_dirs.contains(rel.as_str())
            })
            .count() as u64;
        let source_dir_total_no_root = source_dir_count as u64;
        let source_dirs_not_new =
            source_dir_total_no_root.saturating_sub(missing_dir_prefixes.len() as u64);
        let uncollided_dirs_by_overlap = dest_total_dirs.saturating_sub(source_dirs_not_new);
        out.uncollided_dirs = uncollided_dirs_by_scan.max(uncollided_dirs_by_overlap);
    } else {
        out.scan_complete = false;
        out.uncollided_dirs = 0;
    }

    let mut sync_delete_files = Vec::new();
    let mut sync_delete_dirs = Vec::new();
    if sync_mode {
        if let Some(idx) = destination_index.as_ref() {
            for (rel, entry) in &idx.entries {
                if entry.kind == DestinationKind::Regular
                    && !source_rel_files.contains(rel)
                    && !source_rel_dirs.contains(rel)
                {
                    sync_delete_files.push(ManifestDeleteEntry {
                        rel: Arc::from(rel.as_str()),
                        size: entry.size,
                        dev: entry.dev,
                        ino: entry.ino,
                        mtime: entry.mtime,
                        is_symlink: false,
                        link_target: entry.link_target.clone(),
                    });
                    let display_rel = map_display_rel(include_root, &src_base, rel);
                    insert_preview_change(
                        &mut directory_preview_changes,
                        display_rel,
                        ChangeKind::RemovedFile,
                        bounded_preview_depth,
                    );
                }
            }
            for (rel, entry) in &idx.entries {
                if entry.kind == DestinationKind::Symlink
                    && !source_rel_files.contains(rel)
                    && !source_rel_dirs.contains(rel)
                {
                    sync_delete_files.push(ManifestDeleteEntry {
                        rel: Arc::from(rel.as_str()),
                        size: 0,
                        dev: 0,
                        ino: 0,
                        mtime: None,
                        is_symlink: true,
                        link_target: entry.link_target.clone(),
                    });
                    let display_rel = map_display_rel(include_root, &src_base, rel);
                    insert_preview_change(
                        &mut directory_preview_changes,
                        display_rel,
                        ChangeKind::RemovedFile,
                        bounded_preview_depth,
                    );
                }
            }
            for (rel, entry) in &idx.entries {
                if entry.kind == DestinationKind::Directory
                    && !source_rel_dirs.contains(rel)
                    && !source_rel_files.contains(rel)
                {
                    sync_delete_dirs.push(ManifestDeleteDirEntry {
                        rel: rel.clone(),
                        dev: entry.dev,
                        ino: entry.ino,
                    });
                    let display_rel = format!(
                        "{}/",
                        map_display_rel(include_root, &src_base, rel).trim_end_matches('/')
                    );
                    insert_preview_change(
                        &mut directory_preview_changes,
                        display_rel,
                        ChangeKind::RemovedDir,
                        bounded_preview_depth,
                    );
                }
            }
        }
        sync_delete_files.sort_by(|a, b| a.rel.cmp(&b.rel));
        sync_delete_dirs.sort_by_cached_key(|entry| {
            (
                std::cmp::Reverse(entry.rel.bytes().filter(|byte| *byte == b'/').count()),
                entry.rel.clone(),
            )
        });
    }

    if out.add_files > 0
        || out.mod_files > 0
        || !sync_delete_files.is_empty()
        || !sync_delete_dirs.is_empty()
    {
        out.has_itemized_changes = true;
    }

    directory_preview_changes.extend(detailed_changes);
    out.change_preview.extend(
        directory_preview_changes
            .into_iter()
            .map(|(rel, kind)| ChangeItem { kind, rel }),
    );
    out.file_relation_breakdown = file_relation_breakdown;

    if build_manifest {
        if let Some(d) = manifest_dirs.take() {
            manifest_copy_files.sort_by(|a, b| a.rel.cmp(&b.rel));
            if retain_identical_manifest {
                manifest_identical_files.sort_by(|a, b| a.rel.cmp(&b.rel));
            }
            dir_times.sort_by_cached_key(|entry| {
                (
                    std::cmp::Reverse(entry.rel.bytes().filter(|byte| *byte == b'/').count()),
                    entry.rel.clone(),
                )
            });
            out.transfer_manifest = Some(TransferManifest {
                dirs: d,
                dir_times,
                copy_files: manifest_copy_files,
                identical_files: manifest_identical_files,
                sync_delete_files,
                sync_delete_dirs,
            });
        }
    }

    out
}

pub(crate) fn pre_scan_file(
    src_mnt: &Path,
    dst_path: &str,
    dst_obj_kind: DstObjKind,
    build_source_display_paths: bool,
    collect_file_relation_breakdown: bool,
    replace_dest_symlink: bool,
    merge_collision_policy: MergeCollisionPolicy,
    destination_index: Option<&DestinationIndex>,
) -> PreScan {
    let mut out = PreScan::default();
    if !path_components_are_utf8(src_mnt) {
        out.scan_complete = false;
        return out;
    }
    let src_lmd = match fs::symlink_metadata(src_mnt) {
        Ok(m) => m,
        Err(_) => {
            out.scan_complete = false;
            return out;
        }
    };
    let src_is_symlink = src_lmd.file_type().is_symlink();
    let src_meta = if src_is_symlink {
        None
    } else {
        match fs::metadata(src_mnt) {
            Ok(m) => Some(m),
            Err(_) => {
                out.scan_complete = false;
                return out;
            }
        }
    };
    let size = src_meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let src_mtime = src_meta.as_ref().and_then(|m| m.modified().ok());
    out.total_regular_files = Some(if src_is_symlink { 0 } else { 1 });
    out.total_regular_bytes = Some(size);
    out.total_dirs = Some(0);

    let src_name = src_mnt
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "source".to_string());
    let src_name_key = src_name.clone();

    let (dst_file, display_rel) = match dst_obj_kind {
        DstObjKind::Dir | DstObjKind::DirExisting => {
            let base = Path::new(dst_path.trim_end_matches('/'));
            (base.join(&src_name), src_name.clone())
        }
        _ => {
            let p = Path::new(dst_path);
            let n = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(src_name.clone());
            (p.to_path_buf(), n)
        }
    };

    let destination_root = match dst_obj_kind {
        DstObjKind::Dir | DstObjKind::DirExisting => {
            Some(PathBuf::from(dst_path.trim_end_matches('/')))
        }
        _ => dst_file.parent().map(|p| {
            if p.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                p.to_path_buf()
            }
        }),
    };
    if let Some(root) = destination_root {
        if root.is_dir() {
            let mut source_rel_files = HashSet::new();
            let rel_key = display_rel.trim_end_matches('/').to_string();
            if !rel_key.is_empty() {
                source_rel_files.insert(rel_key);
            }
            // For file-to-file moves within the same destination directory (rename case),
            // exclude the source filename from uncollided totals. It is part of the move
            // operation and should not be treated as unrelated destination-only content.
            let src_parent = src_mnt.parent().unwrap_or_else(|| Path::new("."));
            if realpath_allow_missing(src_parent) == realpath_allow_missing(&root) {
                let src_rel = src_name_key.trim_end_matches('/').to_string();
                if !src_rel.is_empty() {
                    source_rel_files.insert(src_rel);
                }
            }
            let uncollided_by_scan = if let Some(index) = destination_index {
                if !index.complete {
                    out.scan_complete = false;
                }
                index
                    .entries
                    .iter()
                    .filter(|(rel, entry)| {
                        entry.kind == DestinationKind::Regular && !source_rel_files.contains(*rel)
                    })
                    .count() as u64
            } else {
                destination_file_counts(&root, &source_rel_files).1
            };
            out.uncollided_files = uncollided_by_scan;
        }
    }

    if build_source_display_paths && !display_rel.is_empty() {
        out.source_display_paths
            .insert(display_rel.trim_end_matches('/').to_string());
    }

    let change = if src_is_symlink {
        match fs::symlink_metadata(&dst_file) {
            Ok(dm) if dm.file_type().is_symlink() && symlink_targets_equal(src_mnt, &dst_file) => {
                None
            }
            Ok(_) => Some(ChangeKind::ModFile),
            Err(_) => Some(ChangeKind::NewFile),
        }
    } else {
        let dst_lmd = fs::symlink_metadata(&dst_file).ok();
        let dst_is_symlink = dst_lmd
            .as_ref()
            .map(|md| md.file_type().is_symlink())
            .unwrap_or(false);
        let dst_exists = dst_lmd.is_some();
        let dst_meta = if replace_dest_symlink && dst_is_symlink {
            None
        } else {
            fs::metadata(&dst_file).ok()
        };
        let dst_size = dst_meta.as_ref().map(|m| m.len());
        let dst_mtime = dst_meta.as_ref().and_then(|m| m.modified().ok());
        if collect_file_relation_breakdown && dst_meta.is_some() && !dst_is_symlink {
            if let Some(breakdown) = classify_file_relation(size, src_mtime, dst_size, dst_mtime) {
                out.file_relation_breakdown = breakdown;
            }
        }
        regular_file_collision_change(
            merge_collision_policy,
            size,
            src_mtime,
            dst_exists,
            dst_size,
            dst_mtime,
        )
    };

    if let Some(ch) = change {
        out.has_itemized_changes = true;
        out.planned_bytes = if src_is_symlink { 0 } else { size };
        match ch {
            ChangeKind::NewFile => {
                if !src_is_symlink {
                    out.add_files = 1;
                }
            }
            _ => {
                if !src_is_symlink {
                    out.mod_files = 1;
                }
            }
        }
        out.change_preview.push(ChangeItem {
            kind: ch,
            rel: display_rel,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DstObjKind, MergeCollisionPolicy};
    use std::collections::HashSet;
    use tempfile::tempdir;

    #[test]
    fn pre_scan_file_counts_uncollided_siblings_for_file_target() {
        let td = tempdir().expect("tempdir");
        let src = td.path().join("src").join("auth.json");
        let dst_dir = td.path().join("dst").join("accounts");
        let dst_file = dst_dir.join("personal2.json");
        fs::create_dir_all(src.parent().expect("src parent")).expect("mkdir src");
        fs::create_dir_all(&dst_dir).expect("mkdir dst");
        fs::write(&src, b"token\n").expect("write src");
        fs::write(dst_dir.join("other.json"), b"other\n").expect("write sibling");

        let mut src_rel_files = HashSet::new();
        src_rel_files.insert("personal2.json".to_string());
        let (_total, uncollided) = destination_file_counts(&dst_dir, &src_rel_files);
        assert_eq!(uncollided, 1);

        let ps = pre_scan_file(
            &src,
            &dst_file.display().to_string(),
            DstObjKind::File,
            false,
            false,
            false,
            MergeCollisionPolicy::default(),
            None,
        );
        assert_eq!(ps.uncollided_files, 1);
    }
}
