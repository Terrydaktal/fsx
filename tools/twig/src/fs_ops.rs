use jwalk::WalkDir;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const NTFS_FS_TYPES: [&str; 3] = ["ntfs", "ntfs3", "fuseblk"];

#[derive(Clone)]
pub(crate) struct MountInfo {
    pub(crate) device: PathBuf,
    pub(crate) mount_point: PathBuf,
    pub(crate) fs_type: String,
}

pub(crate) fn on_disk_size(metadata: &fs::Metadata) -> u64 {
    metadata.blocks() * 512
}

pub(crate) fn is_hidden_name(name: &OsStr) -> bool {
    name.as_bytes().first().copied() == Some(b'.')
}

pub(crate) fn to_full_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub(crate) fn to_full_path_with_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

pub(crate) fn normalize_path_lexical(path: &Path) -> PathBuf {
    let is_absolute = path.is_absolute();
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !is_absolute {
                    out.push("..");
                }
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                out.push(component.as_os_str())
            }
        }
    }
    out
}

fn unescape_proc_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let a = bytes[i + 1];
            let b = bytes[i + 2];
            let c = bytes[i + 3];
            let octal = (b'0'..=b'7').contains(&a)
                && (b'0'..=b'7').contains(&b)
                && (b'0'..=b'7').contains(&c);
            if octal {
                let value = ((a - b'0') << 6) | ((b - b'0') << 3) | (c - b'0');
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn detect_mount_info(path: &Path) -> Option<MountInfo> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let mut best: Option<(usize, MountInfo)> = None;

    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let (device_raw, mount_point_raw, fs_type) =
            match (parts.next(), parts.next(), parts.next()) {
                (Some(device), Some(mount), Some(fs_type)) => (device, mount, fs_type),
                _ => continue,
            };
        let device = PathBuf::from(unescape_proc_mount_field(device_raw));
        let mount_point = PathBuf::from(unescape_proc_mount_field(mount_point_raw));
        if !canonical.starts_with(&mount_point) {
            continue;
        }
        let mount_len = mount_point.as_os_str().as_bytes().len();
        if best
            .as_ref()
            .map(|(best_len, _)| mount_len > *best_len)
            .unwrap_or(true)
        {
            best = Some((
                mount_len,
                MountInfo {
                    device,
                    mount_point,
                    fs_type: fs_type.to_string(),
                },
            ));
        }
    }

    best.map(|(_, info)| info)
}

fn detect_filesystem_type(path: &Path) -> Option<String> {
    detect_mount_info(path).map(|info| info.fs_type)
}

fn is_ntfs_like_filesystem(path: &Path) -> bool {
    detect_filesystem_type(path)
        .map(|fs_type| NTFS_FS_TYPES.iter().any(|t| fs_type == *t))
        .unwrap_or(false)
}

fn ntfs_best_filename(
    entry: &ntfs::NtfsIndexEntry<'_, ntfs::indexes::NtfsFileNameIndex>,
) -> Option<String> {
    let file_name = entry.key()?.ok()?;
    match file_name.namespace() {
        ntfs::structured_values::NtfsFileNamespace::Dos => None,
        _ => Some(file_name.name().to_string_lossy().to_string()),
    }
}

fn ntfs_is_reparse_point(file: &ntfs::NtfsFile, device: &mut fs::File) -> bool {
    let mut attrs = file.attributes();
    while let Some(attr_result) = attrs.next(device) {
        if let Ok(attr_item) = attr_result {
            if let Ok(attr) = attr_item.to_attribute() {
                if let Ok(attr_ty) = attr.ty() {
                    if attr_ty == ntfs::NtfsAttributeType::ReparsePoint {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn ntfs_file_allocated_size(file: &ntfs::NtfsFile, device: &mut fs::File, block_size: u64) -> u64 {
    if let Some(data_attr) = file.data(device, "") {
        if let Ok(data_item) = data_attr {
            if let Ok(data_attr_obj) = data_item.to_attribute() {
                if data_attr_obj.is_resident() {
                    return round_up_to_block(data_attr_obj.value_length(), block_size);
                }
                if let Ok(value) = data_attr_obj.value(device) {
                    return match value {
                        ntfs::attribute_value::NtfsAttributeValue::NonResident(value) => value
                            .data_runs()
                            .flatten()
                            .filter(|run| run.data_position().value().is_some())
                            .map(|run| run.allocated_size())
                            .sum(),
                        // The crate does not expose the individual runs of an
                        // attribute-list value; its logical length is the safe
                        // fallback instead of reporting zero allocation.
                        ntfs::attribute_value::NtfsAttributeValue::AttributeListNonResident(
                            value,
                        ) => round_up_to_block(value.len(), block_size),
                        ntfs::attribute_value::NtfsAttributeValue::Resident(value) => {
                            round_up_to_block(value.len(), block_size)
                        }
                    };
                }
            }
        }
    }
    0
}

fn fs_block_size(path: &Path) -> u64 {
    let path_c = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return 4096,
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path_c.as_ptr(), &mut stat as *mut _) };
    if rc == 0 && stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        4096
    }
}

fn round_up_to_block(size: u64, block_size: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    if block_size <= 1 {
        return size;
    }
    size.div_ceil(block_size) * block_size
}

fn ntfs_find_subdir_record(
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
        let mut next_record: Option<u64> = None;

        while let Some(entry_result) = entries.next(device) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let entry_name = match ntfs_best_filename(&entry) {
                Some(n) => n,
                None => continue,
            };
            if entry_name == "." || entry_name == ".." {
                continue;
            }
            let child_record = entry.file_reference().file_record_number();
            if entry_name == name {
                next_record = Some(child_record);
                break;
            }
        }
        current_record = next_record?;
    }

    Some(current_record)
}

fn ntfs_scan_subtree_record(
    ntfs: &ntfs::Ntfs,
    device: &mut fs::File,
    top_record: u64,
    show_hidden: bool,
    need_sizes: bool,
    need_counts: bool,
    block_size: u64,
    shared_seen: Option<&Arc<Mutex<HashSet<u64>>>>,
) -> (u64, u64, u64, u64) {
    let mut local_size = 0u64;
    let mut global_size = 0u64;
    let mut total_dirs = 0u64;
    let mut total_files = 0u64;
    let mut stack = vec![top_record];
    let mut seen_dirs = HashSet::<u64>::new();
    let mut local_seen = shared_seen.map(|_| HashSet::<u64>::new());

    while let Some(current_record) = stack.pop() {
        if !seen_dirs.insert(current_record) {
            continue;
        }
        let dir_file = match ntfs.file(device, current_record) {
            Ok(file) => file,
            Err(_) => continue,
        };
        if need_sizes {
            local_size += block_size;
            global_size += block_size;
        }
        let index = match dir_file.directory_index(device) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let mut entries = index.entries();
        while let Some(entry_result) = entries.next(device) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = match ntfs_best_filename(&entry) {
                Some(n) => n,
                None => continue,
            };
            if name == "." || name == ".." {
                continue;
            }
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let child_record = entry.file_reference().file_record_number();
            let child_file = match ntfs.file(device, child_record) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let child_is_dir = child_file.is_directory();
            let child_is_reparse = ntfs_is_reparse_point(&child_file, device);

            if child_is_dir && !child_is_reparse {
                if need_counts {
                    total_dirs += 1;
                }
                stack.push(child_record);
            } else {
                if need_counts {
                    total_files += 1;
                }
                if need_sizes {
                    let include_local_size = local_seen
                        .as_mut()
                        .map(|seen| seen.insert(child_record))
                        .unwrap_or(true);
                    let mut include_global_size = true;
                    if let Some(seen) = shared_seen {
                        if let Ok(mut set) = seen.lock() {
                            include_global_size = set.insert(child_record);
                        }
                    }
                    let size = ntfs_file_allocated_size(&child_file, device, block_size);
                    if include_local_size {
                        local_size += size;
                    }
                    if include_global_size {
                        global_size += size;
                    }
                }
            }
        }
    }

    (local_size, global_size, total_dirs, total_files)
}

fn collect_recursive_stats_ntfs_mft(
    base_path: &Path,
    show_hidden: bool,
    dedupe_hardlinks: bool,
    need_sizes: bool,
    need_counts: bool,
) -> io::Result<(
    HashMap<OsString, u64>,
    HashMap<OsString, (u64, u64)>,
    Option<u64>,
    Option<(u64, u64)>,
)> {
    let canonical_base = fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
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

    let block_size = fs_block_size(&canonical_base);
    let available_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let ntfs_threads = std::env::var("TWIG_NTFS_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| available_threads.min(4).max(1));
    let mut recursive_sizes: HashMap<OsString, u64> = HashMap::new();
    let mut recursive_counts: HashMap<OsString, (u64, u64)> = HashMap::new();
    let mut top_level_dirs: Vec<(OsString, u64)> = Vec::new();
    let mut root_dirs_count = 0u64;
    let mut root_files_count = 0u64;
    let shared_seen = if need_sizes && dedupe_hardlinks {
        Some(Arc::new(Mutex::new(HashSet::<u64>::new())))
    } else {
        None
    };
    let mut root_recursive_size = if need_sizes {
        fs::symlink_metadata(&canonical_base)
            .map(|m| on_disk_size(&m))
            .unwrap_or(block_size)
    } else {
        0
    };

    let base_file = ntfs
        .file(&mut device, base_record)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let index = base_file
        .directory_index(&mut device)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let mut entries = index.entries();
    while let Some(entry_result) = entries.next(&mut device) {
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = match ntfs_best_filename(&entry) {
            Some(n) => n,
            None => continue,
        };
        if name == "." || name == ".." {
            continue;
        }
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let child_record = entry.file_reference().file_record_number();
        let child_file = match ntfs.file(&mut device, child_record) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let child_is_dir = child_file.is_directory();
        let child_is_reparse = ntfs_is_reparse_point(&child_file, &mut device);

        if child_is_dir && !child_is_reparse {
            if need_counts {
                root_dirs_count += 1;
            }
            top_level_dirs.push((OsString::from(name), child_record));
        } else if need_sizes {
            let mut include_size = true;
            if let Some(seen) = shared_seen.as_ref() {
                if let Ok(mut set) = seen.lock() {
                    include_size = set.insert(child_record);
                }
            }
            if include_size {
                root_recursive_size +=
                    ntfs_file_allocated_size(&child_file, &mut device, block_size);
            }
            if need_counts {
                root_files_count += 1;
            }
        } else if need_counts {
            root_files_count += 1;
        }
    }

    let run_scan = |dirs: Vec<(OsString, u64)>| {
        dirs.into_par_iter()
            .map_init(
                || {
                    let mut device = fs::File::open(&mount.device).map_err(|_| ())?;
                    let ntfs = ntfs::Ntfs::new(&mut device).map_err(|_| ())?;
                    Ok::<(fs::File, ntfs::Ntfs), ()>((device, ntfs))
                },
                |reader, (name, record)| {
                    let Ok((device, ntfs)) = reader.as_mut() else {
                        return None;
                    };
                    let (local_size, global_size, dirs_count, file_count) =
                        ntfs_scan_subtree_record(
                            ntfs,
                            device,
                            record,
                            show_hidden,
                            need_sizes,
                            need_counts,
                            block_size,
                            shared_seen.as_ref(),
                        );
                    Some((name, local_size, global_size, dirs_count, file_count))
                },
            )
            .filter_map(|result| result)
            .collect::<Vec<_>>()
    };

    let dir_results: Vec<(OsString, u64, u64, u64, u64)> = if ntfs_threads <= 1
        || top_level_dirs.len() <= 1
    {
        top_level_dirs
            .into_iter()
            .filter_map(|(name, record)| {
                let (local_size, global_size, dirs_count, file_count) = ntfs_scan_subtree_record(
                    &ntfs,
                    &mut device,
                    record,
                    show_hidden,
                    need_sizes,
                    need_counts,
                    block_size,
                    shared_seen.as_ref(),
                );
                Some((name, local_size, global_size, dirs_count, file_count))
            })
            .collect()
    } else if let Ok(pool) = rayon::ThreadPoolBuilder::new()
        .num_threads(ntfs_threads)
        .build()
    {
        pool.install(|| run_scan(top_level_dirs))
    } else {
        run_scan(top_level_dirs)
    };

    for (name, local_size, global_size, dirs_count, file_count) in dir_results {
        if need_sizes {
            recursive_sizes.insert(name.clone(), local_size);
            root_recursive_size += global_size;
        }
        if need_counts {
            root_dirs_count += dirs_count;
            root_files_count += file_count;
            recursive_counts.insert(name, (dirs_count.saturating_add(1), file_count));
        }
    }

    Ok((
        recursive_sizes,
        recursive_counts,
        if need_sizes {
            Some(root_recursive_size)
        } else {
            None
        },
        if need_counts {
            Some((root_dirs_count.saturating_add(1), root_files_count))
        } else {
            None
        },
    ))
}

fn size_with_hardlink_dedupe(
    metadata: &fs::Metadata,
    shared_seen: Option<&Arc<Mutex<HashSet<(u64, u64)>>>>,
) -> u64 {
    let size = on_disk_size(metadata);
    if metadata.is_dir() || metadata.nlink() <= 1 || shared_seen.is_none() {
        return size;
    }
    if let Some(seen) = shared_seen {
        if let Ok(mut set) = seen.lock() {
            if set.insert((metadata.dev(), metadata.ino())) {
                return size;
            }
            return 0;
        }
    }
    size
}

struct WalkAggregate {
    top_level: HashMap<OsString, (u64, u64, u64)>,
    root_size: u64,
    root_dirs: u64,
    root_files: u64,
}

fn scan_subtree_stats_low_overhead(
    top_dir: &Path,
    show_hidden: bool,
    need_sizes: bool,
    need_counts: bool,
    shared_seen: Option<&Arc<Mutex<HashSet<(u64, u64)>>>>,
) -> (u64, u64, u64, u64) {
    let mut local_size = 0u64;
    let mut global_size = 0u64;
    let mut total_dirs = 0u64;
    let mut total_files = 0u64;
    let mut local_seen = shared_seen.map(|_| HashSet::<(u64, u64)>::new());

    if need_sizes {
        if let Ok(meta) = fs::symlink_metadata(top_dir) {
            let size = on_disk_size(&meta);
            local_size += size;
            global_size += size;
        }
    }

    let mut stack = vec![top_dir.to_path_buf()];
    while let Some(current_dir) = stack.pop() {
        let entries = match fs::read_dir(&current_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry_res in entries {
            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name();
            if !show_hidden && is_hidden_name(&name) {
                continue;
            }
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let is_dir = metadata.file_type().is_dir();

            if need_counts {
                if is_dir {
                    total_dirs += 1;
                } else {
                    total_files += 1;
                }
            }

            if need_sizes {
                let size = on_disk_size(&metadata);
                let include_local_size = if metadata.is_dir() || metadata.nlink() <= 1 {
                    true
                } else if let Some(seen) = local_seen.as_mut() {
                    seen.insert((metadata.dev(), metadata.ino()))
                } else {
                    true
                };
                let include_global_size = if metadata.is_dir() || metadata.nlink() <= 1 {
                    true
                } else if let Some(seen) = shared_seen {
                    seen.lock()
                        .map(|mut set| set.insert((metadata.dev(), metadata.ino())))
                        .unwrap_or(true)
                } else {
                    true
                };
                if include_local_size {
                    local_size += size;
                }
                if include_global_size {
                    global_size += size;
                }
            }

            if is_dir {
                stack.push(path);
            }
        }
    }

    (local_size, global_size, total_dirs, total_files)
}

fn collect_recursive_stats_ntfs(
    base_path: &Path,
    show_hidden: bool,
    dedupe_hardlinks: bool,
    need_sizes: bool,
    need_counts: bool,
) -> (
    HashMap<OsString, u64>,
    HashMap<OsString, (u64, u64)>,
    Option<u64>,
    Option<(u64, u64)>,
) {
    if !need_sizes && !need_counts {
        return (HashMap::new(), HashMap::new(), None, None);
    }

    let ntfs_debug = std::env::var_os("TWIG_NTFS_DEBUG").is_some();
    match collect_recursive_stats_ntfs_mft(
        base_path,
        show_hidden,
        dedupe_hardlinks,
        need_sizes,
        need_counts,
    ) {
        Ok(stats) => {
            if ntfs_debug {
                eprintln!("twig: NTFS MFT fast path enabled");
            }
            return stats;
        }
        Err(err) => {
            if ntfs_debug {
                eprintln!("twig: NTFS MFT fast path unavailable: {}", err);
            }
        }
    }

    let canonical_base = fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
    let mut recursive_sizes: HashMap<OsString, u64> = HashMap::new();
    let mut recursive_counts: HashMap<OsString, (u64, u64)> = HashMap::new();
    let mut top_level_dirs: Vec<(OsString, PathBuf)> = Vec::new();
    let mut root_dirs_count = 0u64;
    let mut root_files_count = 0u64;
    let available_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let ntfs_threads = std::env::var("TWIG_NTFS_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| available_threads.min(4).max(1));
    let shared_seen = if need_sizes && dedupe_hardlinks {
        Some(Arc::new(Mutex::new(HashSet::<(u64, u64)>::new())))
    } else {
        None
    };
    let root_size_total = if need_sizes {
        fs::symlink_metadata(&canonical_base)
            .map(|m| size_with_hardlink_dedupe(&m, shared_seen.as_ref()))
            .unwrap_or(0)
    } else {
        0
    };
    let mut root_recursive_size = root_size_total;

    let entries = match fs::read_dir(&canonical_base) {
        Ok(e) => e,
        Err(_) => {
            return (
                recursive_sizes,
                recursive_counts,
                if need_sizes {
                    Some(root_recursive_size)
                } else {
                    None
                },
                if need_counts {
                    Some((root_dirs_count.saturating_add(1), root_files_count))
                } else {
                    None
                },
            );
        }
    };

    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name();
        if !show_hidden && is_hidden_name(&name) {
            continue;
        }
        let child_path = entry.path();
        if need_sizes {
            let metadata = match fs::symlink_metadata(&child_path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.file_type().is_dir() {
                if need_counts {
                    root_dirs_count += 1;
                }
                top_level_dirs.push((name, child_path));
            } else {
                if need_counts {
                    root_files_count += 1;
                }
                root_recursive_size += size_with_hardlink_dedupe(&metadata, shared_seen.as_ref());
            }
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            if need_counts {
                root_dirs_count += 1;
            }
            top_level_dirs.push((name, child_path));
        } else if need_counts {
            root_files_count += 1;
        }
    }

    let run_scan = |dirs: Vec<(OsString, PathBuf)>| {
        dirs.into_par_iter()
            .map(|(name, dir_path)| {
                let (local_size, global_size, dirs, files) = scan_subtree_stats_low_overhead(
                    &dir_path,
                    show_hidden,
                    need_sizes,
                    need_counts,
                    shared_seen.as_ref(),
                );
                (name, local_size, global_size, dirs, files)
            })
            .collect::<Vec<_>>()
    };

    let dir_results: Vec<(OsString, u64, u64, u64, u64)> =
        if ntfs_threads <= 1 || top_level_dirs.len() <= 1 {
            top_level_dirs
                .into_iter()
                .map(|(name, dir_path)| {
                    let (local_size, global_size, dirs, files) = scan_subtree_stats_low_overhead(
                        &dir_path,
                        show_hidden,
                        need_sizes,
                        need_counts,
                        shared_seen.as_ref(),
                    );
                    (name, local_size, global_size, dirs, files)
                })
                .collect()
        } else if let Ok(pool) = rayon::ThreadPoolBuilder::new()
            .num_threads(ntfs_threads)
            .build()
        {
            pool.install(|| run_scan(top_level_dirs))
        } else {
            run_scan(top_level_dirs)
        };

    for (name, local_size, global_size, dirs, files) in dir_results {
        if need_sizes {
            recursive_sizes.insert(name.clone(), local_size);
            root_recursive_size += global_size;
        }
        if need_counts {
            root_dirs_count += dirs;
            root_files_count += files;
            recursive_counts.insert(name, (dirs.saturating_add(1), files));
        }
    }

    (
        recursive_sizes,
        recursive_counts,
        if need_sizes {
            Some(root_recursive_size)
        } else {
            None
        },
        if need_counts {
            Some((root_dirs_count.saturating_add(1), root_files_count))
        } else {
            None
        },
    )
}

pub(crate) fn collect_recursive_stats(
    base_path: &Path,
    show_hidden: bool,
    dedupe_hardlinks: bool,
    need_sizes: bool,
    need_counts: bool,
) -> (
    HashMap<OsString, u64>,
    HashMap<OsString, (u64, u64)>,
    Option<u64>,
    Option<(u64, u64)>,
) {
    if !need_sizes && !need_counts {
        return (HashMap::new(), HashMap::new(), None, None);
    }

    let canonical_base = fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
    if is_ntfs_like_filesystem(&canonical_base) {
        return collect_recursive_stats_ntfs(
            &canonical_base,
            show_hidden,
            dedupe_hardlinks,
            need_sizes,
            need_counts,
        );
    }
    let scan_root = canonical_base.clone();
    // Aggregate callback results under one lock instead of taking separate
    // locks for sizes, counts, and each top-level directory.
    let aggregate = Arc::new(Mutex::new(WalkAggregate {
        top_level: HashMap::new(),
        root_size: 0,
        root_dirs: 0,
        root_files: 0,
    }));
    let shared_aggregate = Arc::clone(&aggregate);
    let root_size_total = if need_sizes {
        fs::symlink_metadata(&canonical_base)
            .map(|m| on_disk_size(&m))
            .unwrap_or(0)
    } else {
        0
    };
    let seen_inodes = if need_sizes && dedupe_hardlinks {
        Some(Arc::new(Mutex::new(HashSet::<(u64, u64)>::new())))
    } else {
        None
    };
    let shared_seen = seen_inodes.as_ref().map(Arc::clone);
    let bucket_seen = if need_sizes && dedupe_hardlinks {
        Some(Arc::new(Mutex::new(
            HashMap::<OsString, HashSet<(u64, u64)>>::new(),
        )))
    } else {
        None
    };
    let shared_bucket_seen = bucket_seen.as_ref().map(Arc::clone);

    WalkDir::new(&canonical_base)
        .skip_hidden(!show_hidden)
        .process_read_dir(move |_depth, path, _state, children| {
            let current_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                scan_root.join(path)
            };

            let rel = match current_path.strip_prefix(&canonical_base) {
                Ok(r) => r,
                Err(_) => return,
            };
            let mut rel_components = rel.components();
            let first_component = rel_components.next();

            let mut local_updates: HashMap<OsString, (u64, u64, u64)> = HashMap::new();
            let mut callback_size = 0u64;
            let mut callback_dirs = 0u64;
            let mut callback_files = 0u64;

            // Root callback: seed top-level dir entries and add each top-level dir's own size.
            if first_component.is_none() {
                let mut hardlink_candidates: Vec<(u64, u64, u64)> = Vec::new();
                for child in children.iter_mut().filter_map(|e| e.as_mut().ok()) {
                    let ft = child.file_type();
                    if need_counts {
                        if ft.is_dir() {
                            callback_dirs += 1;
                        } else {
                            callback_files += 1;
                        }
                    }
                    if !need_sizes {
                        continue;
                    }
                    if let Ok(metadata) = child.metadata() {
                        let size = on_disk_size(&metadata);
                        if ft.is_dir() {
                            callback_size += size;
                            let key = child.file_name().to_os_string();
                            let stats = local_updates.entry(key).or_insert((0, 0, 0));
                            stats.0 += size;
                        } else if shared_seen.is_none() || metadata.nlink() <= 1 {
                            callback_size += size;
                        } else {
                            hardlink_candidates.push((metadata.dev(), metadata.ino(), size));
                        }
                    }
                }
                if let Some(ref seen) = shared_seen {
                    if !hardlink_candidates.is_empty() {
                        if let Ok(mut set) = seen.lock() {
                            for (dev, ino, size) in hardlink_candidates {
                                if set.insert((dev, ino)) {
                                    callback_size += size;
                                }
                            }
                        }
                    }
                }
            } else {
                // Non-root callback: all children belong to the same top-level bucket.
                let top_level_name = match first_component {
                    Some(Component::Normal(name)) => name.to_os_string(),
                    _ => return,
                };

                let mut local_size = 0u64;
                let mut global_size = 0u64;
                let mut local_dirs = 0u64;
                let mut local_files = 0u64;
                let mut hardlink_candidates: Vec<(u64, u64, u64)> = Vec::new();

                for child in children.iter_mut().filter_map(|e| e.as_mut().ok()) {
                    let ft = child.file_type();
                    if need_counts {
                        if ft.is_dir() {
                            local_dirs += 1;
                        } else {
                            local_files += 1;
                        }
                    }
                    if need_sizes {
                        if let Ok(metadata) = child.metadata() {
                            if shared_seen.is_none() || metadata.is_dir() || metadata.nlink() <= 1 {
                                let size = on_disk_size(&metadata);
                                local_size += size;
                                global_size += size;
                            } else {
                                hardlink_candidates.push((
                                    metadata.dev(),
                                    metadata.ino(),
                                    on_disk_size(&metadata),
                                ));
                            }
                        }
                    }
                }

                if let Some(ref seen) = shared_bucket_seen {
                    if !hardlink_candidates.is_empty() {
                        if let Ok(mut buckets) = seen.lock() {
                            let bucket = buckets
                                .entry(top_level_name.clone())
                                .or_insert_with(HashSet::new);
                            for &(dev, ino, size) in &hardlink_candidates {
                                if bucket.insert((dev, ino)) {
                                    local_size += size;
                                }
                            }
                        }
                    }
                }

                if let Some(ref seen) = shared_seen {
                    if !hardlink_candidates.is_empty() {
                        if let Ok(mut set) = seen.lock() {
                            for &(dev, ino, size) in &hardlink_candidates {
                                if set.insert((dev, ino)) {
                                    global_size += size;
                                }
                            }
                        }
                    }
                }

                callback_size = global_size;
                callback_dirs = local_dirs;
                callback_files = local_files;
                local_updates.insert(top_level_name, (local_size, local_dirs, local_files));
            }

            if let Ok(mut aggregate) = shared_aggregate.lock() {
                if need_sizes {
                    aggregate.root_size += callback_size;
                }
                for (key, value) in local_updates {
                    let entry = aggregate.top_level.entry(key).or_insert((0, 0, 0));
                    entry.0 += value.0;
                    entry.1 += value.1;
                    entry.2 += value.2;
                }
                if need_counts {
                    aggregate.root_dirs += callback_dirs;
                    aggregate.root_files += callback_files;
                }
            }
        })
        .into_iter()
        .for_each(|_| {});

    let aggregate = match Arc::try_unwrap(aggregate) {
        Ok(mutex) => mutex.into_inner().unwrap_or(WalkAggregate {
            top_level: HashMap::new(),
            root_size: 0,
            root_dirs: 0,
            root_files: 0,
        }),
        Err(shared) => shared
            .lock()
            .map(|m| WalkAggregate {
                top_level: m.top_level.clone(),
                root_size: m.root_size,
                root_dirs: m.root_dirs,
                root_files: m.root_files,
            })
            .unwrap_or(WalkAggregate {
                top_level: HashMap::new(),
                root_size: 0,
                root_dirs: 0,
                root_files: 0,
            }),
    };

    let mut recursive_sizes: HashMap<OsString, u64> = HashMap::new();
    let mut recursive_counts: HashMap<OsString, (u64, u64)> = HashMap::new();

    for (name, stats) in aggregate.top_level {
        if need_sizes {
            recursive_sizes.insert(name.clone(), stats.0);
        }
        if need_counts {
            recursive_counts.insert(name, (stats.1.saturating_add(1), stats.2));
        }
    }

    let root_recursive_size = if need_sizes {
        Some(root_size_total.saturating_add(aggregate.root_size))
    } else {
        None
    };
    let root_recursive_counts = if need_counts {
        Some((aggregate.root_dirs.saturating_add(1), aggregate.root_files))
    } else {
        None
    };

    (
        recursive_sizes,
        recursive_counts,
        root_recursive_size,
        root_recursive_counts,
    )
}

pub(crate) fn recursive_dir_on_disk_size(
    base_path: &Path,
    show_hidden: bool,
    dedupe_hardlinks: bool,
) -> u64 {
    let canonical_base = fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
    if is_ntfs_like_filesystem(&canonical_base) {
        let shared_seen = if dedupe_hardlinks {
            Some(Arc::new(Mutex::new(HashSet::<(u64, u64)>::new())))
        } else {
            None
        };
        let (_, global_size, _, _) = scan_subtree_stats_low_overhead(
            &canonical_base,
            show_hidden,
            true,
            false,
            shared_seen.as_ref(),
        );
        return global_size;
    }
    let mut total = 0u64;
    let mut seen_inodes = if dedupe_hardlinks {
        Some(HashSet::<(u64, u64)>::new())
    } else {
        None
    };

    for entry in WalkDir::new(&canonical_base).skip_hidden(!show_hidden) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if let Some(seen) = seen_inodes.as_mut() {
            if !metadata.is_dir() && metadata.nlink() > 1 {
                let inode_key = (metadata.dev(), metadata.ino());
                if !seen.insert(inode_key) {
                    continue;
                }
            }
        }

        total += on_disk_size(&metadata);
    }

    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_allocated_size_up_to_block_boundary() {
        assert_eq!(round_up_to_block(0, 4096), 0);
        assert_eq!(round_up_to_block(1, 4096), 4096);
        assert_eq!(round_up_to_block(4096, 4096), 4096);
        assert_eq!(round_up_to_block(4097, 4096), 8192);
    }

    #[test]
    fn hidden_name_detection_only_matches_leading_dot() {
        assert!(is_hidden_name(OsStr::new(".config")));
        assert!(!is_hidden_name(OsStr::new("config")));
        assert!(!is_hidden_name(OsStr::new("config.hidden")));
    }

    #[test]
    fn recursive_stats_dedupes_hardlinks_only_in_root_aggregate() {
        let root = std::env::temp_dir().join(format!(
            "twig-hardlink-stats-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/data"), b"shared inode").unwrap();
        fs::hard_link(root.join("a/data"), root.join("b/data")).unwrap();

        let (sizes, _, root_size, _) = collect_recursive_stats(&root, true, true, true, false);
        let root_allocated = on_disk_size(&fs::symlink_metadata(&root).unwrap());
        let a_allocated = on_disk_size(&fs::symlink_metadata(root.join("a")).unwrap());
        let b_allocated = on_disk_size(&fs::symlink_metadata(root.join("b")).unwrap());
        let data_allocated = on_disk_size(&fs::symlink_metadata(root.join("a/data")).unwrap());

        assert_eq!(
            sizes.get(OsStr::new("a")).copied(),
            Some(a_allocated + data_allocated)
        );
        assert_eq!(
            sizes.get(OsStr::new("b")).copied(),
            Some(b_allocated + data_allocated)
        );
        assert_eq!(
            root_size,
            Some(root_allocated + a_allocated + b_allocated + data_allocated)
        );

        fs::remove_dir_all(root).unwrap();
    }
}
