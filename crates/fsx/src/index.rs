use crate::metadata::EntryKind;
use std::path::PathBuf;
use std::time::SystemTime;

#[cfg(feature = "index")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "index")]
use std::ffi::OsString;
#[cfg(feature = "index")]
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexFreshness {
    pub root: PathBuf,
    pub generation: u64,
    pub complete: bool,
    pub updated: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct IndexedEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub logical_size: Option<u64>,
    pub allocated_size: Option<u64>,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexAggregate {
    pub logical_size: u64,
    pub allocated_size: u64,
    pub files: u64,
    pub dirs: u64,
}

pub trait IndexQuery {
    type Error;

    fn freshness(&self, root: &std::path::Path) -> Result<Option<IndexFreshness>, Self::Error>;

    fn entries(&self, root: &std::path::Path) -> Result<Vec<IndexedEntry>, Self::Error>;

    fn aggregate(&self, root: &std::path::Path) -> Result<Option<IndexAggregate>, Self::Error>;
}

#[cfg(feature = "index")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecursiveStats {
    pub allocated_size: u64,
    pub files: u64,
    pub dirs: u64,
}

#[cfg(feature = "index")]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecursiveStatsBatch {
    pub root: RecursiveStats,
    pub children: HashMap<OsString, RecursiveStats>,
}

#[cfg(feature = "index")]
/// Query recursive allocated sizes from a clean, live fsxd index.
/// Returns `None` when the index is absent, stale, incomplete, or predates the
/// allocated-size column, allowing callers to use their live scanner safely.
pub fn query_recursive_stats(root: &Path) -> Option<RecursiveStats> {
    query_recursive_stats_batch(root, false).map(|batch| batch.root)
}

#[cfg(feature = "index")]
/// Query a root and all of its immediate directory children in one index pass.
/// When requested, hardlinks are deduplicated globally for the root and
/// independently inside each child, matching a live recursive scan.
pub fn query_recursive_stats_batch(
    root: &Path,
    dedupe_hardlinks: bool,
) -> Option<RecursiveStatsBatch> {
    query_recursive_stats_batch_with_metrics(root, dedupe_hardlinks, true)
}

#[cfg(feature = "index")]
/// Query recursive counts without requiring size or hard-link metadata.
/// Counts remain usable while an index is being refreshed for size facts.
pub fn query_recursive_counts_batch(
    root: &Path,
    dedupe_hardlinks: bool,
) -> Option<RecursiveStatsBatch> {
    query_recursive_stats_batch_with_metrics(root, dedupe_hardlinks, false)
}

#[cfg(feature = "index")]
fn query_recursive_stats_batch_with_metrics(
    root: &Path,
    dedupe_hardlinks: bool,
    require_sizes: bool,
) -> Option<RecursiveStatsBatch> {
    let root = std::fs::canonicalize(root).ok()?;
    let root_key = root.to_string_lossy().into_owned();
    let prefix = if root_key == "/" {
        "/".to_string()
    } else {
        format!("{root_key}/")
    };
    let prefix_end = format!("{}0", prefix.trim_end_matches('/'));
    let db_path = database_read_path()?;
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    if !watcher_covers_root(&conn, &root_key).ok()? {
        return None;
    }
    let stats_ready: bool = conn
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'dir_stats'
             ) AND EXISTS(
                 SELECT 1 FROM index_meta
                 WHERE key = 'dir_stats_version' AND value = '1'
             )",
            [],
            |row| row.get(0),
        )
        .ok()?;
    if !stats_ready {
        return None;
    }

    let mut stmt = conn
        .prepare(
            "SELECT d.path,
                    COALESCE(s.allocated_size, 0),
                    COALESCE(s.files, 0),
                    COALESCE(s.dirs, 0),
                    COALESCE(s.missing_sizes, 0),
                    COALESCE(s.missing_hardlink_metadata, 0)
             FROM dirs d
             LEFT JOIN dir_stats s ON s.dir_id = d.id
             WHERE d.path = ?1 OR (d.path >= ?2 AND d.path < ?3)",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![&root_key, &prefix, &prefix_end], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .ok()?;
    let mut batch = RecursiveStatsBatch::default();
    let mut missing_hardlink_metadata = 0u64;
    let mut saw_root = false;
    for row in rows {
        let (path, bytes, files, dirs, missing_sizes, missing_hardlinks) = row.ok()?;
        if bytes < 0
            || files < 0
            || dirs < 0
            || require_sizes && missing_sizes != 0
            || missing_hardlinks < 0
        {
            return None;
        }
        let stats = RecursiveStats {
            allocated_size: bytes as u64,
            files: files as u64,
            dirs: dirs as u64,
        };
        batch.root.allocated_size = batch
            .root
            .allocated_size
            .saturating_add(stats.allocated_size);
        batch.root.files = batch.root.files.saturating_add(stats.files);
        batch.root.dirs = batch.root.dirs.saturating_add(stats.dirs);
        missing_hardlink_metadata =
            missing_hardlink_metadata.saturating_add(missing_hardlinks as u64);
        if path == root_key {
            saw_root = true;
            continue;
        }
        let relative = path.strip_prefix(&prefix)?;
        let child_name = relative.split('/').next()?;
        if child_name.is_empty() {
            return None;
        }
        let child = batch
            .children
            .entry(OsString::from(child_name))
            .or_default();
        child.allocated_size = child.allocated_size.saturating_add(stats.allocated_size);
        child.files = child.files.saturating_add(stats.files);
        child.dirs = child.dirs.saturating_add(stats.dirs);
    }
    drop(stmt);
    if !saw_root || require_sizes && dedupe_hardlinks && missing_hardlink_metadata != 0 {
        return None;
    }

    let root_size = std::fs::symlink_metadata(&root)
        .ok()
        .map(|metadata| crate::metadata::allocated_size(&metadata))
        .unwrap_or(0);
    batch.root.allocated_size = batch.root.allocated_size.saturating_add(root_size);
    batch.root.dirs = batch.root.dirs.saturating_add(1);

    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
        let child = batch.children.entry(entry.file_name()).or_default();
        child.allocated_size = child
            .allocated_size
            .saturating_add(crate::metadata::allocated_size(&metadata));
        child.dirs = child.dirs.saturating_add(1);
    }

    if require_sizes && dedupe_hardlinks {
        deduct_duplicate_hardlinks(&conn, &root_key, &prefix, &prefix_end, &mut batch)?;
    }
    Some(batch)
}

#[cfg(feature = "index")]
fn deduct_duplicate_hardlinks(
    conn: &rusqlite::Connection,
    root: &str,
    prefix: &str,
    prefix_end: &str,
    batch: &mut RecursiveStatsBatch,
) -> Option<()> {
    let mut stmt = conn
        .prepare(
            "SELECT d.path, e.device, e.inode, e.allocated_size
             FROM entries e INDEXED BY idx_entries_hardlink_candidates
             CROSS JOIN dirs d ON d.id = e.dir_id
             WHERE e.link_count > 1 AND e.kind <> 1
               AND (d.path = ?1 OR (d.path >= ?2 AND d.path < ?3))",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![root, prefix, prefix_end], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .ok()?;
    let mut root_seen = HashSet::new();
    let mut child_seen: HashMap<OsString, HashSet<(i64, i64)>> = HashMap::new();
    for row in rows {
        let (parent, device, inode, allocated_size) = row.ok()?;
        let (device, inode, allocated_size) = (device?, inode?, allocated_size?);
        if device < 0 || inode < 0 || allocated_size < 0 {
            return None;
        }
        let identity = (device, inode);
        if !root_seen.insert(identity) {
            batch.root.allocated_size = batch
                .root
                .allocated_size
                .saturating_sub(allocated_size as u64);
        }
        if parent == root {
            continue;
        }
        let relative = parent.strip_prefix(prefix)?;
        let name = OsString::from(relative.split('/').next()?);
        let seen = child_seen.entry(name.clone()).or_default();
        if !seen.insert(identity) {
            let child = batch.children.get_mut(&name)?;
            child.allocated_size = child.allocated_size.saturating_sub(allocated_size as u64);
        }
    }
    Some(())
}

/// Return the canonical fsx cache directory.
#[cfg(feature = "index")]
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(cache).join("fsx"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/fsx"))
}

/// Return the legacy Unearth cache directory for migration and read fallback.
#[cfg(feature = "index")]
pub fn legacy_cache_dir() -> Option<PathBuf> {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(cache).join("unearth"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/unearth"))
}

/// Return the canonical shared index database path.
#[cfg(feature = "index")]
pub fn database_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FSX_INDEX_DB") {
        return Some(PathBuf::from(path));
    }
    Some(cache_dir()?.join("index/fsx.db"))
}

/// Return the old database path when the caller needs to migrate or read it.
#[cfg(feature = "index")]
pub fn legacy_database_path() -> Option<PathBuf> {
    if std::env::var_os("FSX_INDEX_DB").is_some() {
        return None;
    }
    Some(legacy_cache_dir()?.join("index/unearth.db"))
}

/// Prefer the canonical database, falling back to the legacy database while
/// an installation is being migrated.
#[cfg(feature = "index")]
pub fn database_read_path() -> Option<PathBuf> {
    let canonical = database_path()?;
    if canonical.is_file() {
        return Some(canonical);
    }
    legacy_database_path()
        .filter(|legacy| legacy.is_file())
        .or(Some(canonical))
}

#[cfg(feature = "index")]
fn watcher_covers_root(conn: &rusqlite::Connection, root: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT root, watcher_pid, owner_boot_id, owner_starttime, heartbeat
         FROM watch_state
         WHERE status = 'running' AND dirty = 0 AND online = 1",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    })?;
    for row in rows {
        let (watched, pid, boot_id, starttime, heartbeat) = row?;
        let heartbeat_age = heartbeat.map(|value| unix_now().saturating_sub(value));
        let heartbeat_fresh = heartbeat_age.is_some_and(|age| (0..=30).contains(&age));
        if heartbeat_fresh
            && watcher_owner_is_alive(pid, boot_id.as_deref(), starttime)
            && (watched == "/"
                || watched == root
                || root.starts_with(&format!("{}/", watched.trim_end_matches('/'))))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(feature = "index")]
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(feature = "index")]
fn process_starttime(pid: i64) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(feature = "index")]
fn watcher_owner_is_alive(pid: Option<i64>, boot_id: Option<&str>, starttime: Option<i64>) -> bool {
    let (Some(pid), Some(expected_boot), Some(expected_start)) = (pid, boot_id, starttime) else {
        return false;
    };
    let current_boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    current_boot == expected_boot && process_starttime(pid) == Some(expected_start)
}

#[cfg(all(test, feature = "index"))]
mod tests {
    use super::*;

    #[test]
    fn hardlinks_are_deduped_globally_and_per_child() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE dirs(id INTEGER PRIMARY KEY, path TEXT NOT NULL);
             CREATE TABLE entries(
                 dir_id INTEGER NOT NULL,
                 kind INTEGER NOT NULL,
                 allocated_size INTEGER,
                 device INTEGER,
                 inode INTEGER,
                 link_count INTEGER
             );
             CREATE INDEX idx_entries_hardlink_candidates
                 ON entries(dir_id, device, inode, allocated_size)
                 WHERE link_count > 1 AND kind <> 1;
             INSERT INTO dirs(id, path) VALUES
                 (1, '/root'), (2, '/root/a'), (3, '/root/b');
             INSERT INTO entries(
                 dir_id, kind, allocated_size, device, inode, link_count
             ) VALUES
                 (2, 0, 100, 8, 42, 3),
                 (2, 0, 100, 8, 42, 3),
                 (3, 0, 100, 8, 42, 3);",
        )
        .unwrap();
        let mut batch = RecursiveStatsBatch {
            root: RecursiveStats {
                allocated_size: 300,
                files: 3,
                dirs: 3,
            },
            children: HashMap::from([
                (
                    OsString::from("a"),
                    RecursiveStats {
                        allocated_size: 200,
                        files: 2,
                        dirs: 1,
                    },
                ),
                (
                    OsString::from("b"),
                    RecursiveStats {
                        allocated_size: 100,
                        files: 1,
                        dirs: 1,
                    },
                ),
            ]),
        };

        deduct_duplicate_hardlinks(&conn, "/root", "/root/", "/root0", &mut batch).unwrap();

        assert_eq!(batch.root.allocated_size, 100);
        assert_eq!(batch.children[&OsString::from("a")].allocated_size, 100);
        assert_eq!(batch.children[&OsString::from("b")].allocated_size, 100);
    }
}
