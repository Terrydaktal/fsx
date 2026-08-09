use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Gitignore matcher that loads rules as directories are encountered.
///
/// The previous implementation recursively discovered every `.gitignore` before
/// scanning began. This keeps matching lazy and avoids a second full traversal.
#[derive(Clone)]
pub struct IgnoreMatcher {
    root: PathBuf,
    cache: Arc<Mutex<HashMap<PathBuf, Arc<Gitignore>>>>,
}

impl IgnoreMatcher {
    pub fn from_root(root: &Path) -> Option<Self> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Some(Self {
            root,
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Construct a matcher rooted at the repository when a search starts in a
    /// subdirectory. This keeps parent `.gitignore` rules visible to every
    /// consumer without requiring each CLI to rediscover repository roots.
    pub fn from_search_root(root: &Path) -> Option<Self> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let matcher_root = root
            .ancestors()
            .find(|ancestor| ancestor.join(".git").exists())
            .map(Path::to_path_buf)
            .unwrap_or(root);
        Self::from_root(&matcher_root)
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };

        let mut directories = vec![self.root.clone()];
        let mut current = self.root.clone();
        for component in relative
            .parent()
            .into_iter()
            .flat_map(|parent| parent.components())
        {
            current.push(component.as_os_str());
            directories.push(current.clone());
        }

        let mut ignored = false;
        for directory in directories {
            let matcher = self.matcher_for(&directory);
            let relative_to_rules = path.strip_prefix(&directory).unwrap_or(relative);
            match matcher.matched_path_or_any_parents(relative_to_rules, is_dir) {
                ignore::Match::Ignore(_) => ignored = true,
                ignore::Match::Whitelist(_) => ignored = false,
                ignore::Match::None => {}
            }
        }
        ignored
    }

    fn matcher_for(&self, directory: &Path) -> Arc<Gitignore> {
        if let Ok(cache) = self.cache.lock()
            && let Some(matcher) = cache.get(directory)
        {
            return Arc::clone(matcher);
        }

        let mut builder = GitignoreBuilder::new(directory);
        for name in [".gitignore", ".ignore", ".fdignore"] {
            let ignore_file = directory.join(name);
            if ignore_file.is_file() {
                let _ = builder.add(ignore_file);
            }
        }
        if directory == self.root {
            let exclude_file = directory.join(".git").join("info").join("exclude");
            if exclude_file.is_file() {
                let _ = builder.add(exclude_file);
            }
            for global in global_ignore_files() {
                if global.is_file() {
                    let _ = builder.add(global);
                }
            }
        }
        let matcher = Arc::new(builder.build().unwrap_or_else(|_| Gitignore::empty()));
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(directory.to_path_buf(), Arc::clone(&matcher));
        }
        matcher
    }
}

fn global_ignore_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        files.push(PathBuf::from(path).join("git/ignore"));
    } else if let Some(home) = std::env::var_os("HOME") {
        files.push(PathBuf::from(home).join(".config/git/ignore"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        files.push(PathBuf::from(home).join(".gitignore_global"));
    }
    files
}
