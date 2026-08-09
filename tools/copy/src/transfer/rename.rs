//! Same-filesystem rename accelerators for merge and move operations.

use crate::domain::TransferManifest;
use crate::plan::{can_fast_rename_same_fs, top_level_rel_component};
use rustc_hash::FxHashMap;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default, Clone, Copy)]
pub(crate) struct PreMergeFastRenameStats {
    pub(crate) moved_entries: u64,
    pub(crate) moved_files: u64,
    pub(crate) moved_bytes: u64,
    pub(crate) removed_copy_bytes: u64,
}

pub(crate) fn premerge_fast_rename_noncolliding_children(
    src_root: &Path,
    dst_root: &Path,
    manifest: Option<&mut TransferManifest>,
    exclude_top_level_name: Option<&str>,
) -> PreMergeFastRenameStats {
    if !src_root.is_dir() || !dst_root.is_dir() {
        return PreMergeFastRenameStats::default();
    }

    let mut aggregate_by_top: FxHashMap<String, (u64, u64, u64)> = FxHashMap::default();
    if let Some(manifest) = manifest.as_deref() {
        for entry in &manifest.identical_files {
            if let Some(top) = top_level_rel_component(&entry.rel) {
                let aggregate = aggregate_by_top.entry(top.to_string()).or_default();
                aggregate.0 = aggregate.0.saturating_add(1);
                aggregate.1 = aggregate.1.saturating_add(entry.size);
            }
        }
        for entry in &manifest.copy_files {
            if let Some(top) = top_level_rel_component(&entry.rel) {
                let aggregate = aggregate_by_top.entry(top.to_string()).or_default();
                aggregate.0 = aggregate.0.saturating_add(1);
                aggregate.1 = aggregate.1.saturating_add(entry.size);
                aggregate.2 = aggregate.2.saturating_add(entry.size);
            }
        }
    }

    let mut children: Vec<PathBuf> = match fs::read_dir(src_root) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect(),
        Err(_) => return PreMergeFastRenameStats::default(),
    };
    children.sort();

    let mut moved_names = HashSet::new();
    let mut stats = PreMergeFastRenameStats::default();
    for src_child in children {
        let name = match src_child.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => continue,
        };
        if exclude_top_level_name == Some(name.as_str()) {
            continue;
        }
        let dst_child = dst_root.join(&name);
        if dst_child.exists() || src_child == dst_child {
            continue;
        }
        if !can_fast_rename_same_fs(&src_child, &dst_child) {
            continue;
        }

        let (child_files, child_bytes, removed_copy_bytes) =
            aggregate_by_top.get(&name).copied().unwrap_or_default();
        if fs::rename(&src_child, &dst_child).is_ok() {
            moved_names.insert(name);
            stats.moved_entries = stats.moved_entries.saturating_add(1);
            stats.moved_files = stats.moved_files.saturating_add(child_files);
            stats.moved_bytes = stats.moved_bytes.saturating_add(child_bytes);
            stats.removed_copy_bytes = stats.removed_copy_bytes.saturating_add(removed_copy_bytes);
        }
    }

    if moved_names.is_empty() {
        return stats;
    }

    if let Some(manifest) = manifest {
        manifest.dirs.retain(|rel| {
            top_level_rel_component(rel)
                .map(|top| !moved_names.contains(top))
                .unwrap_or(true)
        });
        manifest.copy_files.retain(|entry| {
            top_level_rel_component(&entry.rel)
                .map(|top| !moved_names.contains(top))
                .unwrap_or(true)
        });
        manifest.identical_files.retain(|entry| {
            top_level_rel_component(&entry.rel)
                .map(|top| !moved_names.contains(top))
                .unwrap_or(true)
        });
    }

    stats
}
