use super::*;

type RecursiveStatsValues = (
    HashMap<OsString, u64>,
    HashMap<OsString, (u64, u64)>,
    Option<u64>,
    Option<(u64, u64)>,
);

pub(crate) struct RecursiveStats {
    pub(crate) sizes: HashMap<OsString, u64>,
    pub(crate) counts: HashMap<OsString, (u64, u64)>,
    pub(crate) root_size: Option<u64>,
    pub(crate) root_counts: Option<(u64, u64)>,
    pub(crate) complete: bool,
}

impl Default for RecursiveStats {
    fn default() -> Self {
        Self {
            sizes: HashMap::new(),
            counts: HashMap::new(),
            root_size: None,
            root_counts: None,
            complete: true,
        }
    }
}

impl RecursiveStats {
    fn complete((sizes, counts, root_size, root_counts): RecursiveStatsValues) -> Self {
        Self {
            sizes,
            counts,
            root_size,
            root_counts,
            complete: true,
        }
    }
}

fn live_scan_threads() -> usize {
    let available = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    std::env::var("TWIG_SCAN_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| available.clamp(1, 16))
}

pub(crate) fn collect_recursive_stats_checked(
    base_path: &Path,
    show_hidden: bool,
    dedupe_hardlinks: bool,
    need_sizes: bool,
    need_counts: bool,
) -> RecursiveStats {
    if !need_sizes && !need_counts {
        return RecursiveStats::default();
    }
    let canonical_base = fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
    if show_hidden
        && !is_ntfs_like_filesystem(&canonical_base)
        && let Some(indexed) = collect_recursive_stats_from_index(
            &canonical_base,
            dedupe_hardlinks,
            need_sizes,
            need_counts,
        )
    {
        return RecursiveStats::complete(indexed);
    }
    if is_ntfs_like_filesystem(&canonical_base) {
        return RecursiveStats::complete(collect_recursive_stats_legacy(
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
        hardlinks: if dedupe_hardlinks {
            fsx::scan::HardlinkMode::DeduplicateCandidates
        } else {
            fsx::scan::HardlinkMode::CountEveryEntry
        },
        symlinks: fsx::scan::SymlinkMode::DoNotFollow,
        show_hidden,
        // A 16-worker foreground cap restored the pre-refactor latency without
        // the syscall contention measured at the full 32 logical CPUs.  The
        // override makes unusually slow or remote mounts independently tunable.
        threads: live_scan_threads(),
        ..fsx::scan::ScanRequest::default()
    };
    let snapshot = fsx::scan::scan_top_level(&request);
    if !snapshot.complete {
        eprintln!(
            "twig: recursive scan of {} is partial; skipped {} unreadable entr{}",
            canonical_base.display(),
            snapshot.errors,
            if snapshot.errors == 1 { "y" } else { "ies" }
        );
    }
    if snapshot.overflowed {
        eprintln!(
            "twig: warning: one or more filesystem aggregates overflowed u64 and were saturated"
        );
    }
    let mut sizes = HashMap::new();
    let mut counts = HashMap::new();
    for (name, aggregate) in snapshot.children {
        if need_sizes {
            sizes.insert(name.clone(), aggregate.allocated_size);
        }
        if need_counts {
            counts.insert(name, (aggregate.dirs.saturating_add(1), aggregate.files));
        }
    }
    let root_size = if need_sizes {
        Some(snapshot.root.allocated_size)
    } else {
        None
    };
    RecursiveStats {
        sizes,
        counts,
        root_size,
        root_counts: need_counts
            .then(|| (snapshot.root.dirs.saturating_add(1), snapshot.root.files)),
        complete: snapshot.complete,
    }
}
