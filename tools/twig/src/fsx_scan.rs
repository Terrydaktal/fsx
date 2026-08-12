use super::*;

pub(crate) type RecursiveStats = (
    HashMap<OsString, u64>,
    HashMap<OsString, (u64, u64)>,
    Option<u64>,
    Option<(u64, u64)>,
);

pub(crate) fn collect_recursive_stats_checked(
    base_path: &Path,
    show_hidden: bool,
    dedupe_hardlinks: bool,
    need_sizes: bool,
    need_counts: bool,
) -> Option<RecursiveStats> {
    if !need_sizes && !need_counts {
        return Some((HashMap::new(), HashMap::new(), None, None));
    }
    let canonical_base = fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
    if is_ntfs_like_filesystem(&canonical_base) {
        return Some(collect_recursive_stats_legacy(
            &canonical_base,
            show_hidden,
            dedupe_hardlinks,
            need_sizes,
            need_counts,
        ));
    }

    let request = fsx::scan::ScanRequest {
        root: canonical_base.clone(),
        size_mode: if need_sizes {
            fsx::scan::SizeMode::Allocated
        } else {
            fsx::scan::SizeMode::None
        },
        count_files: need_counts,
        count_dirs: need_counts,
        // Keep each child aggregate complete.  The root aggregate is reduced
        // separately below so a hardlink can appear in every child while only
        // contributing once to the total, matching Twig's longstanding UI.
        hardlinks: fsx::scan::HardlinkMode::CountEveryEntry,
        symlinks: fsx::scan::SymlinkMode::DoNotFollow,
        show_hidden,
        threads: std::thread::available_parallelism()
            .map(|value| value.get().min(4))
            .unwrap_or(1),
        ..fsx::scan::ScanRequest::default()
    };
    let snapshot = fsx::scan::scan(&request);
    if !snapshot.complete {
        RECURSIVE_SCAN_INCOMPLETE.store(true, Ordering::Relaxed);
        eprintln!(
            "twig: recursive scan of {} skipped {} unreadable entr{}",
            canonical_base.display(),
            snapshot.errors,
            if snapshot.errors == 1 { "y" } else { "ies" }
        );
        return None;
    }
    let mut sizes = HashMap::new();
    let mut counts = HashMap::new();
    for entry in snapshot.entries.iter().filter(|entry| entry.depth == 1) {
        let Some(name) = entry.path.file_name() else {
            continue;
        };
        if entry.metadata.kind == fsx::EntryKind::Directory {
            if need_sizes {
                sizes.insert(
                    name.to_os_string(),
                    snapshot
                        .aggregates
                        .get(&entry.path)
                        .map(|aggregate| aggregate.allocated_size)
                        .unwrap_or(entry.metadata.allocated_size),
                );
            }
            if need_counts {
                let aggregate = snapshot.aggregates.get(&entry.path);
                counts.insert(
                    name.to_os_string(),
                    aggregate
                        .map(|aggregate| (aggregate.dirs, aggregate.files))
                        .unwrap_or_default(),
                );
            }
        } else if need_sizes {
            sizes.insert(name.to_os_string(), entry.metadata.allocated_size);
        }
    }
    let root_size = if need_sizes {
        let mut total = fs::symlink_metadata(&canonical_base)
            .map(|metadata| fsx::metadata::allocated_size(&metadata))
            .unwrap_or(0);
        let mut seen = HashSet::new();
        for entry in &snapshot.entries {
            let include = if entry.metadata.kind == fsx::EntryKind::Directory {
                true
            } else if dedupe_hardlinks {
                entry
                    .metadata
                    .hardlink_key()
                    .map(|key| seen.insert(key))
                    .unwrap_or(true)
            } else {
                true
            };
            if include {
                total = total.saturating_add(entry.metadata.allocated_size);
            }
        }
        Some(total)
    } else {
        None
    };
    let root_aggregate = snapshot.aggregates.get(&canonical_base);
    Some((
        sizes,
        counts,
        root_size,
        need_counts.then(|| {
            root_aggregate
                .map(|value| (value.dirs.saturating_add(1), value.files))
                .unwrap_or((1, 0))
        }),
    ))
}
