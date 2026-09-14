//! Invocation-local path hierarchies. No filesystem I/O or persistent cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A deduplicated set of requested paths, with the nearest requested parent.
/// Ancestor lookup is bounded by path depth, not the number of requests.
pub struct PathRequests {
    paths: Vec<Arc<Path>>,
    ids: HashMap<Arc<Path>, usize>,
    parents: Vec<Option<usize>>,
}

impl PathRequests {
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut paths: Vec<Arc<Path>> = paths.into_iter().map(Arc::from).collect();
        paths.sort_unstable();
        paths.dedup();
        let ids: HashMap<_, _> = paths.iter().cloned().zip(0..).collect();
        let parents = paths
            .iter()
            .map(|path| {
                path.ancestors()
                    .skip(1)
                    .find_map(|parent| ids.get(parent).copied())
            })
            .collect();
        Self {
            paths,
            ids,
            parents,
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
    pub fn paths(&self) -> impl DoubleEndedIterator<Item = (usize, &Path)> {
        self.paths
            .iter()
            .enumerate()
            .map(|(id, path)| (id, path.as_ref()))
    }
    pub fn roots(&self) -> impl Iterator<Item = (usize, &Path)> {
        self.paths().filter(|(id, _)| self.parents[*id].is_none())
    }
    pub fn parent(&self, id: usize) -> Option<usize> {
        self.parents[id]
    }
    pub fn owner(&self, path: &Path) -> Option<usize> {
        path.ancestors()
            .find_map(|ancestor| self.ids.get(ancestor).copied())
    }
    /// Children always precede their requested parent in this traversal.
    pub fn fold_children(&self, mut merge: impl FnMut(usize, usize)) {
        for id in (0..self.paths.len()).rev() {
            if let Some(parent) = self.parents[id] {
                merge(parent, id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlaps_are_folded_once_and_prefix_siblings_stay_separate() {
        let requests = PathRequests::new(["/a/b/c", "/a", "/ab", "/a/b", "/a"].map(PathBuf::from));
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests.roots().map(|(_, p)| p).collect::<Vec<_>>(),
            [Path::new("/a"), Path::new("/ab")]
        );
        assert_eq!(requests.owner(Path::new("/a/b/file")), Some(1));
        assert_eq!(requests.owner(Path::new("/abc/file")), None);
        let mut counts = vec![1; requests.len()];
        requests.fold_children(|parent, child| counts[parent] += counts[child]);
        assert_eq!(counts, [3, 2, 1, 1]);
    }
}
