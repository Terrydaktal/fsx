//! Aggregate a union of requested directory subtrees without rescanning overlaps.

use super::filesystem::get_dir_stats_ntfs_mft;
use fsx::hierarchy::PathRequests;
use jwalk::{Parallelism, WalkDir};
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(crate) fn live_directory_stats(paths: Vec<PathBuf>) -> Vec<(PathBuf, (u64, u64))> {
    let requests = Arc::new(PathRequests::new(paths));
    let totals = Arc::new(
        (0..requests.len())
            .map(|_| Mutex::new((0u64, 0u64)))
            .collect::<Vec<_>>(),
    );
    let mut has_children = vec![false; requests.len()];
    for (id, _) in requests.paths() {
        if let Some(parent) = requests.parent(id) {
            has_children[parent] = true;
        }
    }
    let roots: Vec<_> = requests
        .roots()
        .map(|(id, path)| (id, path.to_path_buf()))
        .collect();
    roots.into_par_iter().for_each(|(root_id, root)| {
        // Preserve the direct-MFT fast path for independent requests.
        if !has_children[root_id] {
            if let Ok(stats) = get_dir_stats_ntfs_mft(&root, true) {
                *totals[root_id].lock().unwrap() = stats;
                return;
            }
        }
        let requests = Arc::clone(&requests);
        let totals = Arc::clone(&totals);
        WalkDir::new(root)
            .follow_links(false)
            .skip_hidden(false)
            .parallelism(Parallelism::Serial)
            .process_read_dir(move |depth, path, _, children| {
                if depth.is_none() {
                    return;
                }
                let Some(owner) = requests.owner(path) else {
                    return;
                };
                let mut local = (0u64, 0u64);
                for entry in children.iter().filter_map(|entry| entry.as_ref().ok()) {
                    if entry.file_type().is_file() {
                        local.1 = local.1.saturating_add(1);
                        if let Ok(metadata) = entry.metadata() {
                            local.0 = local.0.saturating_add(metadata.len());
                        }
                    }
                }
                let mut total = totals[owner].lock().unwrap();
                total.0 = total.0.saturating_add(local.0);
                total.1 = total.1.saturating_add(local.1);
            })
            .into_iter()
            .for_each(drop);
    });
    let mut totals: Vec<_> = totals.iter().map(|total| *total.lock().unwrap()).collect();
    requests.fold_children(|parent, child| {
        totals[parent].0 = totals[parent].0.saturating_add(totals[child].0);
        totals[parent].1 = totals[parent].1.saturating_add(totals[child].1);
    });
    if totals
        .iter()
        .any(|&(bytes, files)| bytes == u64::MAX || files == u64::MAX)
    {
        eprintln!("unearth: warning: filesystem aggregate overflowed u64 and was saturated");
    }
    requests
        .paths()
        .map(|(id, path)| (path.to_path_buf(), totals[id]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn nested_sizes_include_hidden_files_but_not_symlink_targets() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("target/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.path().join("root-file"), b"12345").unwrap();
        std::fs::write(nested.join(".hidden"), b"1234567").unwrap();
        std::os::unix::fs::symlink(root.path(), nested.join("cycle")).unwrap();
        let stats: HashMap<_, _> =
            live_directory_stats(vec![root.path().into(), nested.clone(), nested.clone()])
                .into_iter()
                .collect();
        assert_eq!(stats[root.path()], (12, 2));
        assert_eq!(stats[&nested], (7, 1));
    }
}
