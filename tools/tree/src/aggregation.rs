use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) type Totals = [u64; 3];

/// Fold one compact totals vector by parent ID. Paths are owned just once here.
pub(crate) fn fold(mut entries: Vec<(PathBuf, Totals)>) -> (Vec<(PathBuf, Totals)>, bool) {
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let ids: HashMap<&Path, usize> = entries
        .iter()
        .enumerate()
        .map(|(id, (path, _))| (path.as_path(), id))
        .collect();
    let parents: Vec<_> = entries
        .iter()
        .map(|(path, _)| path.ancestors().skip(1).find_map(|p| ids.get(p).copied()))
        .collect();
    drop(ids);
    let mut overflowed = false;
    for id in (0..entries.len()).rev() {
        if let Some(parent) = parents[id] {
            let values = entries[id].1;
            for (slot, value) in entries[parent].1.iter_mut().zip(values) {
                overflowed |= fsx::overflow::checked_add_u64(slot, value);
            }
        }
    }
    (entries, overflowed)
}

/// Keep the omitted suffix for its counts/sizes, but only sort displayed rows.
pub(crate) fn sort_prefix<T>(
    entries: &mut [T],
    limit: usize,
    mut compare: impl FnMut(&T, &T) -> std::cmp::Ordering,
) {
    let limit = limit.min(entries.len());
    if limit == 0 {
        return;
    }
    if limit < entries.len() {
        entries.select_nth_unstable_by(limit, &mut compare);
    }
    entries[..limit].sort_unstable_by(compare);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_folding_preserves_all_metrics_and_sibling_boundaries() {
        let (entries, overflowed) = fold(vec![
            ("/a/b/c".into(), [7, 2, 0]),
            ("/a/b".into(), [3, 0, 1]),
            ("/a/bc".into(), [11, 1, 0]),
            ("/a".into(), [5, 0, 2]),
        ]);
        assert!(!overflowed);
        let values: HashMap<_, _> = entries.into_iter().collect();
        assert_eq!(values[Path::new("/a")], [26, 3, 3]);
        assert_eq!(values[Path::new("/a/b")], [10, 2, 1]);
        let (_, overflowed) = fold(vec![
            ("/a".into(), [u64::MAX, 0, 0]),
            ("/a/b".into(), [1, 0, 0]),
        ]);
        assert!(overflowed);
    }

    #[test]
    fn prefix_matches_full_sort_and_preserves_omitted_entries() {
        let source: Vec<_> = (0..1000).rev().collect();
        for limit in [0, 1, 9, 999, 1000, 2000] {
            let mut selected = source.clone();
            let mut full = source.clone();
            full.sort_unstable();
            sort_prefix(&mut selected, limit, Ord::cmp);
            let n = limit.min(source.len());
            assert_eq!(selected[..n], full[..n]);
            assert_eq!(selected[n..].iter().sum::<usize>(), full[n..].iter().sum());
        }
    }
}
