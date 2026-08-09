use super::model::{ContainsAllSpec, MountInfo, Options, SearchDirMode, SearchResult, TypeFlag};
use super::patterns::parse_search_dir;
use super::{NTFS_FS_TYPES, ROOT_SIZE_SKIP_TREES};
use crossbeam_channel::Sender;
use jwalk::{Parallelism, WalkDir};
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
pub(crate) struct PathInfo {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
}

#[derive(Clone)]
pub(crate) struct SimpleIgnoreRules {
    matcher: fsx::ignore::IgnoreMatcher,
    root: PathBuf,
}

pub(crate) fn load_simple_ignore_rules(dir: &Path) -> SimpleIgnoreRules {
    let root = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let matcher = fsx::ignore::IgnoreMatcher::from_search_root(&root)
        .expect("fsx ignore matcher construction cannot fail");
    SimpleIgnoreRules { matcher, root }
}

pub(crate) fn is_simple_ignored_name(path: &Path, is_dir: bool, rules: &SimpleIgnoreRules) -> bool {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        rules.root.join(path)
    };
    rules.matcher.is_ignored(&path, is_dir)
}

type VisitedDirs = Option<Arc<Vec<PathBuf>>>;

fn enter_directory(path: &Path, follow_links: bool, visited: &VisitedDirs) -> (bool, VisitedDirs) {
    if !follow_links {
        return (true, None);
    }
    let Ok(canonical) = fs::canonicalize(path) else {
        return (true, visited.clone());
    };
    if visited
        .as_ref()
        .is_some_and(|paths| paths.iter().any(|seen| seen == &canonical))
    {
        return (false, visited.clone());
    }
    let mut next = visited.as_deref().map_or_else(Vec::new, ToOwned::to_owned);
    next.push(canonical);
    (true, Some(Arc::new(next)))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_fast(
    dir: PathBuf,
    re: &Regex,
    is_catch_all: bool,
    tx: &Sender<Vec<PathInfo>>,
    visible_only: bool,
    respect_ignore: bool,
    no_recurse: bool,
    follow_links: bool,
    type_flag: Option<TypeFlag>,
    full_path_match: bool,
    serial_subtree: bool,
    timeout_flag: &Arc<AtomicBool>,
) {
    let visited = None;
    walk_fast_inner(
        dir,
        re,
        is_catch_all,
        tx,
        visible_only,
        respect_ignore,
        no_recurse,
        follow_links,
        type_flag,
        full_path_match,
        serial_subtree,
        timeout_flag,
        None,
        visited,
    );
}

#[allow(clippy::too_many_arguments)]
fn walk_fast_inner(
    dir: PathBuf,
    re: &Regex,
    is_catch_all: bool,
    tx: &Sender<Vec<PathInfo>>,
    visible_only: bool,
    respect_ignore: bool,
    no_recurse: bool,
    follow_links: bool,
    type_flag: Option<TypeFlag>,
    full_path_match: bool,
    serial_subtree: bool,
    timeout_flag: &Arc<AtomicBool>,
    inherited_ignore: Option<Arc<SimpleIgnoreRules>>,
    visited: VisitedDirs,
) {
    if timeout_flag.load(Ordering::Relaxed) {
        return;
    }
    let (should_enter, visited) = enter_directory(&dir, follow_links, &visited);
    if !should_enter {
        return;
    }
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return;
    };
    let ignore_rules = if respect_ignore {
        Some(inherited_ignore.unwrap_or_else(|| Arc::new(load_simple_ignore_rules(&dir))))
    } else {
        None
    };
    let mut subdirs = Vec::new();
    let mut local_buf = Vec::with_capacity(512);
    for entry_res in read_dir {
        let Ok(entry) = entry_res else { continue };
        let name = entry.file_name();
        let name_bytes = name.as_bytes();
        if visible_only && name_bytes.starts_with(b".") {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let is_symlink = file_type.is_symlink();
        let mut is_dir = file_type.is_dir();
        if is_symlink && follow_links {
            if let Ok(meta) = fs::metadata(&path) {
                if meta.is_dir() {
                    is_dir = true;
                }
            }
        }
        if let Some(rules) = &ignore_rules {
            if is_simple_ignored_name(&path, is_dir, rules) {
                continue;
            }
        }
        let skip_type = match type_flag {
            Some(TypeFlag::File) => is_dir,
            Some(TypeFlag::Dir) => !is_dir,
            None => false,
        };
        if !skip_type {
            let is_match = if is_catch_all {
                true
            } else {
                let match_target = if full_path_match {
                    path.to_string_lossy()
                } else {
                    name.to_string_lossy()
                };
                re.is_match(&match_target)
            };
            if is_match {
                local_buf.push(PathInfo {
                    path: path.clone(),
                    is_dir,
                });
                if local_buf.len() >= 512 {
                    if tx.send(std::mem::take(&mut local_buf)).is_err() {
                        return;
                    }
                    local_buf.reserve(512);
                }
            }
        }
        if is_dir
            && !no_recurse
            && !(dir.as_os_str().as_bytes() == b"/"
                && (name_bytes == b"proc"
                    || name_bytes == b"sys"
                    || name_bytes == b"dev"
                    || name_bytes == b"run"))
        {
            if is_symlink && !follow_links {
                continue;
            }
            subdirs.push(path);
        }
    }
    if !local_buf.is_empty() && tx.send(local_buf).is_err() {
        return;
    }
    if serial_subtree {
        for subdir in subdirs {
            walk_fast_inner(
                subdir,
                re,
                is_catch_all,
                tx,
                visible_only,
                respect_ignore,
                no_recurse,
                follow_links,
                type_flag,
                full_path_match,
                true,
                timeout_flag,
                ignore_rules.clone(),
                visited.clone(),
            );
        }
    } else {
        subdirs
            .into_par_iter()
            .for_each_with(tx.clone(), |tx_clone, subdir| {
                let next_serial = root_prefers_single_thread(&subdir);
                walk_fast_inner(
                    subdir,
                    re,
                    is_catch_all,
                    tx_clone,
                    visible_only,
                    respect_ignore,
                    no_recurse,
                    follow_links,
                    type_flag,
                    full_path_match,
                    next_serial,
                    timeout_flag,
                    ignore_rules.clone(),
                    visited.clone(),
                );
            });
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_rayon_worker(
    dir: PathBuf,
    re: &Regex,
    is_catch_all: bool,
    tx: &Sender<Vec<SearchResult>>,
    opts: &Options,
    type_flag: Option<TypeFlag>,
    full_path_match: bool,
    prune_matched_dir_subtrees: bool,
    needs_metadata: bool,
    serial_subtree: bool,
    timeout_flag: &Arc<AtomicBool>,
) {
    let visited = None;
    walk_rayon_worker_inner(
        dir,
        re,
        is_catch_all,
        tx,
        opts,
        type_flag,
        full_path_match,
        prune_matched_dir_subtrees,
        needs_metadata,
        serial_subtree,
        timeout_flag,
        None,
        visited,
    );
}

#[allow(clippy::too_many_arguments)]
fn walk_rayon_worker_inner(
    dir: PathBuf,
    re: &Regex,
    is_catch_all: bool,
    tx: &Sender<Vec<SearchResult>>,
    opts: &Options,
    type_flag: Option<TypeFlag>,
    full_path_match: bool,
    prune_matched_dir_subtrees: bool,
    needs_metadata: bool,
    serial_subtree: bool,
    timeout_flag: &Arc<AtomicBool>,
    inherited_ignore: Option<Arc<SimpleIgnoreRules>>,
    visited: VisitedDirs,
) {
    if timeout_flag.load(Ordering::Relaxed) {
        return;
    }
    let (should_enter, visited) = enter_directory(&dir, opts.follow_links, &visited);
    if !should_enter {
        return;
    }
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return;
    };
    let ignore_rules = if opts.respect_ignore {
        Some(inherited_ignore.unwrap_or_else(|| Arc::new(load_simple_ignore_rules(&dir))))
    } else {
        None
    };
    let mut subdirs = Vec::new();
    let mut local_buf = Vec::with_capacity(256);
    for entry_res in read_dir {
        let Ok(entry) = entry_res else { continue };
        let name = entry.file_name();
        let name_lossy = name.to_string_lossy();
        if opts.visible_only && name_lossy.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let is_symlink = file_type.is_symlink();
        let mut is_dir = file_type.is_dir();
        if is_symlink && opts.follow_links {
            if let Ok(meta) = fs::metadata(&path) {
                if meta.is_dir() {
                    is_dir = true;
                }
            }
        }
        if let Some(rules) = &ignore_rules {
            if is_simple_ignored_name(&path, is_dir, rules) {
                continue;
            }
        }
        let skip_type = match type_flag {
            Some(TypeFlag::File) => is_dir,
            Some(TypeFlag::Dir) => !is_dir,
            None => false,
        };
        let mut matched_dir_pruned = false;
        if !skip_type {
            let is_match = if is_catch_all {
                true
            } else if full_path_match {
                if re.is_match(name_lossy.as_ref()) {
                    true
                } else {
                    let match_target = path.to_string_lossy();
                    re.is_match(&match_target)
                }
            } else {
                re.is_match(name_lossy.as_ref())
            };
            if is_match {
                let mut p_str = path
                    .clone()
                    .into_os_string()
                    .into_string()
                    .unwrap_or_else(|os| os.to_string_lossy().into_owned());
                if is_dir && !p_str.ends_with('/') {
                    p_str.push('/');
                }
                local_buf.push(SearchResult {
                    path: p_str,
                    path_encoded: false,
                    is_dir,
                    is_symlink,
                    metadata: if needs_metadata {
                        fs::symlink_metadata(&path).ok()
                    } else {
                        None
                    },
                    indexed_activity_nanos: None,
                    indexed_size: None,
                });

                if local_buf.len() >= 256 {
                    if tx.send(std::mem::take(&mut local_buf)).is_err() {
                        return;
                    }
                    local_buf.reserve(256);
                }
                if is_dir && prune_matched_dir_subtrees {
                    matched_dir_pruned = true;
                }
            }
        }
        if is_dir && !opts.no_recurse {
            if matched_dir_pruned {
                continue;
            }
            if dir.to_str() == Some("/")
                && (name_lossy == "proc"
                    || name_lossy == "sys"
                    || name_lossy == "dev"
                    || name_lossy == "run")
            {
                continue;
            }
            if is_symlink && !opts.follow_links {
                continue;
            }
            let child = entry.path();
            subdirs.push(child);
        }
    }
    if !local_buf.is_empty() && tx.send(local_buf).is_err() {
        return;
    }
    if serial_subtree {
        for subdir in subdirs {
            walk_rayon_worker_inner(
                subdir,
                re,
                is_catch_all,
                tx,
                opts,
                type_flag,
                full_path_match,
                prune_matched_dir_subtrees,
                needs_metadata,
                true,
                timeout_flag,
                ignore_rules.clone(),
                visited.clone(),
            );
        }
    } else {
        subdirs
            .into_par_iter()
            .for_each_with(tx.clone(), |tx_clone, subdir| {
                let next_serial = root_prefers_single_thread(&subdir);
                walk_rayon_worker_inner(
                    subdir,
                    re,
                    is_catch_all,
                    tx_clone,
                    opts,
                    type_flag,
                    full_path_match,
                    prune_matched_dir_subtrees,
                    needs_metadata,
                    next_serial,
                    timeout_flag,
                    ignore_rules.clone(),
                    visited.clone(),
                );
            });
    }
}

pub(crate) fn detect_mount_info(path: &Path) -> Option<MountInfo> {
    fsx::mount::mount_for_path(path).map(|mount| MountInfo {
        device: mount.source,
        mount_point: mount.mount_point,
        fs_type: mount.filesystem,
    })
}

pub(crate) fn ntfs_file_logical_size(file: &ntfs::NtfsFile, device: &mut fs::File) -> u64 {
    fsx::ntfs::data_logical_size(file, device)
}

pub(crate) fn ntfs_find_subdir_record(
    ntfs: &ntfs::Ntfs,
    device: &mut fs::File,
    start_record: u64,
    rel_path: &Path,
) -> Option<u64> {
    let mut current_record = start_record;
    if rel_path.as_os_str().is_empty() {
        return Some(current_record);
    }
    for component in rel_path.components() {
        let name = match component {
            Component::Normal(name) => name.to_string_lossy().to_string(),
            _ => continue,
        };
        let dir_file = ntfs.file(device, current_record).ok()?;
        let index = dir_file.directory_index(device).ok()?;
        let mut entries = index.entries();
        let mut seen_records = HashSet::<u64>::new();
        let mut next_record: Option<u64> = None;
        while let Some(entry_result) = entries.next(device) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let entry_name = match fsx::ntfs::best_filename(&entry) {
                Some(n) => n,
                None => continue,
            };
            if entry_name == "." || entry_name == ".." {
                continue;
            }
            let child_record = entry.file_reference().file_record_number();
            if !seen_records.insert(child_record) {
                continue;
            }
            if entry_name == name {
                next_record = Some(child_record);
                break;
            }
        }
        current_record = next_record?;
    }
    Some(current_record)
}

pub(crate) fn ntfs_scan_subtree_record(
    ntfs: &ntfs::Ntfs,
    device: &mut fs::File,
    top_record: u64,
    count_files: bool,
) -> (u64, u64) {
    let mut total_size = 0u64;
    let mut total_files = 0u64;
    let mut stack = vec![top_record];
    let mut seen_dirs = HashSet::<u64>::new();
    while let Some(current_record) = stack.pop() {
        if !seen_dirs.insert(current_record) {
            continue;
        }
        let dir_file = match ntfs.file(device, current_record) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let index = match dir_file.directory_index(device) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let mut entries = index.entries();
        let mut seen_records = HashSet::<u64>::new();
        while let Some(entry_result) = entries.next(device) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = match fsx::ntfs::best_filename(&entry) {
                Some(n) => n,
                None => continue,
            };
            if name == "." || name == ".." {
                continue;
            }
            let child_record = entry.file_reference().file_record_number();
            if !seen_records.insert(child_record) {
                continue;
            }
            let child_file = match ntfs.file(device, child_record) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let child_is_dir = child_file.is_directory();
            let child_is_reparse = fsx::ntfs::is_reparse_point(&child_file, device);
            if child_is_dir && !child_is_reparse {
                stack.push(child_record);
            } else if !child_is_reparse {
                total_size = total_size.saturating_add(ntfs_file_logical_size(&child_file, device));
                if count_files {
                    total_files = total_files.saturating_add(1);
                }
            }
        }
    }
    (total_size, total_files)
}

pub(crate) fn get_dir_stats_ntfs_mft(path: &Path, count_files: bool) -> io::Result<(u64, u64)> {
    let canonical_base = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mount = detect_mount_info(&canonical_base)
        .ok_or_else(|| io::Error::other("mount detection failed"))?;
    if !NTFS_FS_TYPES.iter().any(|t| mount.fs_type == *t) {
        return Err(io::Error::other("not ntfs"));
    }
    let mut device = fs::File::open(&mount.device)?;
    let ntfs = ntfs::Ntfs::new(&mut device).map_err(|err| io::Error::other(err.to_string()))?;
    let root_dir = ntfs
        .root_directory(&mut device)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let root_record = root_dir.file_record_number();
    let rel_path = canonical_base
        .strip_prefix(&mount.mount_point)
        .unwrap_or(Path::new(""));
    let base_record = ntfs_find_subdir_record(&ntfs, &mut device, root_record, rel_path)
        .ok_or_else(|| io::Error::other("base directory not found in mft"))?;
    Ok(ntfs_scan_subtree_record(
        &ntfs,
        &mut device,
        base_record,
        count_files,
    ))
}

pub(crate) fn get_dir_stats_walk(path: &Path, count_files: bool) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    for entry_res in WalkDir::new(path).follow_links(false) {
        let Ok(entry) = entry_res else { continue };
        let file_type = entry.file_type();
        if !file_type.is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            bytes = bytes.saturating_add(meta.len());
        }
        if count_files {
            files = files.saturating_add(1);
        }
    }
    if bytes == u64::MAX || files == u64::MAX {
        eprintln!("unearth: warning: filesystem aggregate overflowed u64 and was saturated");
    }
    (bytes, files)
}

pub(crate) fn get_dir_stats_native(path: &Path, count_files: bool) -> (u64, u64) {
    let ntfs_debug = env::var_os("UNEARTH_NTFS_DEBUG").is_some();
    match get_dir_stats_ntfs_mft(path, count_files) {
        Ok(stats) => {
            if ntfs_debug {
                eprintln!("unearth: NTFS MFT fast path enabled for {}", path.display());
            }
            stats
        }
        Err(err) => {
            if ntfs_debug
                && detect_mount_info(path)
                    .as_ref()
                    .map(|m| NTFS_FS_TYPES.iter().any(|t| m.fs_type == *t))
                    .unwrap_or(false)
            {
                eprintln!(
                    "unearth: NTFS MFT fast path unavailable for {}: {}",
                    path.display(),
                    err
                );
            }
            get_dir_stats_walk(path, count_files)
        }
    }
}

pub(crate) fn get_dir_bytes_native_serial(path: &Path) -> u64 {
    if let Ok((bytes, _)) = get_dir_stats_ntfs_mft(path, false) {
        return bytes;
    }
    let mut bytes = 0u64;
    for entry_res in WalkDir::new(path)
        .follow_links(false)
        .parallelism(Parallelism::Serial)
    {
        let Ok(entry) = entry_res else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            bytes = bytes.saturating_add(meta.len());
        }
    }
    if bytes == u64::MAX {
        eprintln!("unearth: warning: filesystem size overflowed u64 and was saturated");
    }
    bytes
}

pub(crate) fn normalize_dir_key(path: &str) -> String {
    let normalized = fsx::path::normalize_lexical(Path::new(path));
    if normalized == Path::new("/") {
        "/".to_string()
    } else {
        normalized
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string()
    }
}

pub(crate) fn should_skip_root_size_tree(path: &str) -> bool {
    let normalized = normalize_dir_key(path);
    ROOT_SIZE_SKIP_TREES
        .iter()
        .any(|prefix| normalized == *prefix || normalized.starts_with(&format!("{}/", prefix)))
}

pub(crate) fn root_prefers_single_thread(path: &Path) -> bool {
    path == Path::new("/media") || path.starts_with("/media/")
}

pub(crate) fn effective_search_root(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
) -> Option<PathBuf> {
    if let Some(spec) = content_spec {
        return fs::canonicalize(&spec.root)
            .ok()
            .or_else(|| Some(spec.root.clone()));
    }
    if opts.force_full {
        if opts.positional.len() > 1 {
            if let Some(last) = opts.positional.last() {
                if Path::new(last).is_dir() {
                    return fs::canonicalize(last)
                        .ok()
                        .or_else(|| Some(PathBuf::from(last)));
                }
            }
        }
        return env::current_dir()
            .ok()
            .and_then(|p| fs::canonicalize(p).ok().or(Some(PathBuf::from("."))));
    }
    if opts.positional.len() > 1 {
        match parse_search_dir(
            &opts.positional[1],
            opts.regex_mode,
            opts.force_pattern_mode,
        ) {
            SearchDirMode::Path(p) => {
                return fs::canonicalize(&p).ok().or_else(|| Some(PathBuf::from(p)));
            }
            SearchDirMode::Pattern(_) => return None,
        }
    }
    env::current_dir()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok().or(Some(PathBuf::from("."))))
}

pub(crate) fn effective_threads_override(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
) -> usize {
    if opts.threads_explicit {
        return opts.threads_override;
    }
    let Some(root) = effective_search_root(opts, content_spec) else {
        return opts.threads_override;
    };
    if root_prefers_single_thread(&root) {
        1
    } else {
        opts.threads_override
    }
}
