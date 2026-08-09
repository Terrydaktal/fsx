//! Source cleanup and recursive removal for move and sync operations.
#![allow(clippy::too_many_arguments)]

use super::command::run_command_capture;
use super::copy_engine::{ensure_no_symlink_ancestors, remove_path_local_if_exists};
use crate::domain::{DeleteCleanupOutcome, LogLevel, SrcObjKind, TransferManifest, TransferMode};
use crate::output::log;
use crate::plan::{map_dir_dest_path, normalize_rel, rel_matches_prefix};
use crate::runtime::symlink_targets_equal;
use jwalk::WalkDir;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn path_was_removed_or_replaced(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(nix::libc::ENOTDIR)
}

fn regular_files_equal(left: &Path, right: &Path) -> io::Result<bool> {
    let left_meta = fs::symlink_metadata(left)?;
    let right_meta = fs::symlink_metadata(right)?;
    if left_meta.file_type().is_symlink() || right_meta.file_type().is_symlink() {
        return Ok(symlink_targets_equal(left, right));
    }
    if !left_meta.is_file() || !right_meta.is_file() || left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left_file = fs::File::open(left)?;
    let mut right_file = fs::File::open(right)?;
    let mut left_buf = vec![0u8; 1024 * 1024];
    let mut right_buf = vec![0u8; 1024 * 1024];
    loop {
        let left_n = left_file.read(&mut left_buf)?;
        let right_n = right_file.read(&mut right_buf)?;
        if left_n != right_n {
            return Ok(false);
        }
        if left_n == 0 {
            return Ok(true);
        }
        if left_buf[..left_n] != right_buf[..right_n] {
            return Ok(false);
        }
    }
}

pub(crate) fn remove_path_recursive(path: &Path, use_sudo: bool, mode: TransferMode) -> bool {
    if ensure_no_symlink_ancestors(path.parent().unwrap_or_else(|| Path::new("."))).is_err() {
        log(
            mode,
            &format!(
                "Refusing removal through a symlink ancestor: {}",
                path.display()
            ),
            LogLevel::Error,
        );
        return false;
    }
    if matches!(
        fs::symlink_metadata(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound
    ) {
        return true;
    }
    if use_sudo {
        let cmd = vec![
            "rm".to_string(),
            "-rf".to_string(),
            "--".to_string(),
            path.display().to_string(),
        ];
        match run_command_capture(&cmd, true) {
            Ok(out) if out.code == 0 => true,
            _ => {
                log(
                    mode,
                    &format!("Failed to remove existing path: {}", path.display()),
                    LogLevel::Error,
                );
                false
            }
        }
    } else {
        let res = match fs::symlink_metadata(path) {
            Ok(md) if md.file_type().is_dir() => fs::remove_dir_all(path),
            Ok(_) => fs::remove_file(path),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        if res.is_err() {
            log(
                mode,
                &format!("Failed to remove existing path: {}", path.display()),
                LogLevel::Error,
            );
            return false;
        }
        true
    }
}

pub(crate) fn delete_sync_destination_extras(
    destination_root: &Path,
    manifest: &TransferManifest,
) -> io::Result<DeleteCleanupOutcome> {
    ensure_no_symlink_ancestors(destination_root)?;
    let mut deleted = DeleteCleanupOutcome::default();

    for entry in &manifest.sync_delete_files {
        let path = destination_root.join(entry.rel.as_ref());
        if cleanup_path_unreachable(&path, destination_root)? {
            deleted.files = deleted.files.saturating_add(1);
            deleted.bytes = deleted.bytes.saturating_add(entry.size);
            continue;
        }
        ensure_no_symlink_ancestors(path.parent().unwrap_or(destination_root))?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {
                return Err(io::Error::other(format!(
                    "destination entry changed type during sync cleanup: {}",
                    path.display()
                )));
            }
            Ok(meta) => {
                let same_identity = if entry.is_symlink {
                    meta_is_link_target(&path, entry.link_target.as_deref())
                } else {
                    meta.dev() == entry.dev
                        && meta.ino() == entry.ino
                        && meta.len() == entry.size
                        && (entry.mtime.is_none() || meta.modified().ok() == entry.mtime)
                };
                if !same_identity {
                    return Err(io::Error::other(format!(
                        "destination entry changed during sync cleanup: {}",
                        path.display()
                    )));
                }
                fs::remove_file(&path)?;
                deleted.files = deleted.files.saturating_add(1);
                deleted.bytes = deleted.bytes.saturating_add(entry.size);
            }
            Err(err) if path_was_removed_or_replaced(&err) => {
                deleted.files = deleted.files.saturating_add(1);
                deleted.bytes = deleted.bytes.saturating_add(entry.size);
            }
            Err(err) => return Err(err),
        }
    }

    for entry in &manifest.sync_delete_dirs {
        let path = destination_root.join(&entry.rel);
        if cleanup_path_unreachable(&path, destination_root)? {
            continue;
        }
        ensure_no_symlink_ancestors(path.parent().unwrap_or(destination_root))?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {
                if meta.dev() != entry.dev || meta.ino() != entry.ino {
                    return Err(io::Error::other(format!(
                        "destination directory changed during sync cleanup: {}",
                        path.display()
                    )));
                }
                fs::remove_dir(&path)?
            }
            Ok(_) => remove_path_local_if_exists(&path)?,
            Err(err) if path_was_removed_or_replaced(&err) => {}
            Err(err) => return Err(err),
        }
    }

    Ok(deleted)
}

fn meta_is_link_target(path: &Path, expected: Option<&Path>) -> bool {
    match (expected, fs::read_link(path)) {
        (Some(expected), Ok(actual)) => actual == expected,
        _ => false,
    }
}

fn cleanup_path_unreachable(path: &Path, root: &Path) -> io::Result<bool> {
    let mut current = path.parent();
    while let Some(parent) = current {
        if parent == root {
            return Ok(false);
        }
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing cleanup through symlink ancestor: {}",
                        parent.display()
                    ),
                ));
            }
            Ok(metadata) if !metadata.is_dir() => return Ok(true),
            Ok(_) => current = parent.parent(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(err) => return Err(err),
        }
    }
    Ok(false)
}

pub(crate) fn remove_empty_dirs(path: &Path, remove_root: bool) -> bool {
    if !fs::symlink_metadata(path)
        .map(|md| md.file_type().is_dir())
        .unwrap_or(false)
    {
        return true;
    }
    let root = path.to_path_buf();
    let mut stack: Vec<(PathBuf, bool)> = vec![(root.clone(), false)];
    let mut success = true;
    while let Some((dir, visited)) = stack.pop() {
        if !visited {
            stack.push((dir.clone(), true));
            let entries = match fs::read_dir(&dir) {
                Ok(v) => v,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => {
                    success = false;
                    continue;
                }
            };
            for ent in entries {
                let ent = match ent {
                    Ok(ent) => ent,
                    Err(_) => {
                        success = false;
                        continue;
                    }
                };
                let p = ent.path();
                let md = match fs::symlink_metadata(&p) {
                    Ok(m) => m,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) => {
                        success = false;
                        continue;
                    }
                };
                if md.is_dir() {
                    stack.push((p, false));
                }
            }
            continue;
        }

        if !remove_root && dir == root {
            continue;
        }
        let is_empty = fs::read_dir(&dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false);
        if is_empty {
            if let Err(err) = fs::remove_dir(&dir) {
                if err.kind() != io::ErrorKind::NotFound {
                    success = false;
                }
            }
        }
    }
    success
}

pub(crate) fn cleanup_source_dirs(
    src_root: &Path,
    remove_root: bool,
    use_sudo: bool,
    mode: TransferMode,
) -> bool {
    if !src_root.is_dir() {
        return true;
    }
    if use_sudo {
        let mut cmd = vec!["find".to_string(), src_root.display().to_string()];
        if !remove_root {
            cmd.push("-mindepth".to_string());
            cmd.push("1".to_string());
        }
        cmd.extend([
            "-depth".to_string(),
            "-type".to_string(),
            "d".to_string(),
            "-empty".to_string(),
            "-delete".to_string(),
        ]);
        let success = run_command_capture(&cmd, true)
            .map(|out| out.code == 0)
            .unwrap_or(false);
        if !success {
            log(
                mode,
                "Source cleanup failed: could not remove all empty directories.",
                LogLevel::Error,
            );
        }
        success
    } else {
        let success = remove_empty_dirs(src_root, remove_root);
        if !success {
            log(
                mode,
                "Source cleanup failed: could not inspect or remove all empty directories.",
                LogLevel::Error,
            );
        }
        success
    }
}

pub(crate) fn cleanup_source_dirs_from_manifest(
    src_root: &Path,
    manifest: &TransferManifest,
    remove_root: bool,
    exclude_rel: Option<&str>,
) -> bool {
    let mut success = true;
    for rel in manifest.dirs.iter().rev() {
        if exclude_rel
            .map(|prefix| rel_matches_prefix(rel, prefix))
            .unwrap_or(false)
        {
            continue;
        }
        if let Err(err) = fs::remove_dir(src_root.join(rel)) {
            if err.kind() != io::ErrorKind::NotFound
                && err.kind() != io::ErrorKind::DirectoryNotEmpty
            {
                success = false;
            }
        }
    }
    if remove_root {
        if let Err(err) = fs::remove_dir(src_root) {
            if err.kind() != io::ErrorKind::NotFound
                && err.kind() != io::ErrorKind::DirectoryNotEmpty
            {
                success = false;
            }
        }
    }
    success
}

pub(crate) fn remove_single_file(path: &Path, use_sudo: bool, mode: TransferMode) -> bool {
    if ensure_no_symlink_ancestors(path.parent().unwrap_or_else(|| Path::new("."))).is_err() {
        log(
            mode,
            &format!(
                "Refusing removal through a symlink ancestor: {}",
                path.display()
            ),
            LogLevel::Error,
        );
        return false;
    }
    if matches!(
        fs::symlink_metadata(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound
    ) {
        return true;
    }
    if use_sudo {
        let cmd = vec![
            "rm".to_string(),
            "-f".to_string(),
            "--".to_string(),
            path.display().to_string(),
        ];
        run_command_capture(&cmd, true)
            .map(|o| o.code == 0)
            .unwrap_or_else(|_| {
                log(
                    mode,
                    &format!("Failed to remove source file: {}", path.display()),
                    LogLevel::Warn,
                );
                false
            })
    } else {
        fs::remove_file(path).is_ok()
    }
}

pub(crate) fn prune_move_source_duplicates(
    src_path: &str,
    dst_path: &str,
    src_obj_kind: SrcObjKind,
    contents_mode: bool,
    exclude_rel: Option<&str>,
    use_sudo: bool,
    mode: TransferMode,
    manifest: Option<&TransferManifest>,
    _expected_files: u64,
    _expected_bytes: u64,
) -> DeleteCleanupOutcome {
    let mut removed = DeleteCleanupOutcome::default();
    let report_progress = |_force: bool, _removed: &DeleteCleanupOutcome| {};

    match src_obj_kind {
        SrcObjKind::File => {
            let src = Path::new(src_path);
            let mut dst_buf = PathBuf::from(dst_path);
            if dst_buf.is_dir() {
                let src_name = match src.file_name() {
                    Some(v) => v,
                    None => return removed,
                };
                dst_buf = dst_buf.join(src_name);
            }
            let dst = dst_buf.as_path();
            let src_lmd = match fs::symlink_metadata(src) {
                Ok(v) => v,
                Err(_) => {
                    removed.success = false;
                    return removed;
                }
            };
            let same = regular_files_equal(src, dst).unwrap_or(false);
            if same {
                if remove_single_file(src, use_sudo, mode) {
                    removed.files += 1;
                    removed.bytes += if src_lmd.file_type().is_symlink() {
                        0
                    } else {
                        src_lmd.len()
                    };
                } else {
                    removed.success = false;
                }
            }
            report_progress(false, &removed);
        }
        SrcObjKind::Dir => {
            let src_no_trailing = src_path.trim_end_matches('/');
            let include_root = if contents_mode {
                false
            } else {
                !src_path.ends_with('/')
            };
            let src_root = Path::new(src_no_trailing);
            let dst_base = Path::new(dst_path.trim_end_matches('/'));
            let src_base = src_root
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Some(m) = manifest {
                let mut privileged_deletes: Vec<(PathBuf, u64)> = Vec::new();
                for entries in [&m.copy_files, &m.identical_files] {
                    for entry in entries {
                        if entry.rel.is_empty()
                            || exclude_rel
                                .map(|prefix| rel_matches_prefix(&entry.rel, prefix))
                                .unwrap_or(false)
                        {
                            continue;
                        }
                        let src_file = src_root.join(entry.rel.as_ref());
                        let dst_item =
                            map_dir_dest_path(include_root, &src_base, &entry.rel, dst_base);
                        if src_file == dst_item {
                            continue;
                        }
                        let src_md = match fs::symlink_metadata(&src_file) {
                            Ok(md) => md,
                            Err(_) => {
                                removed.success = false;
                                continue;
                            }
                        };
                        let source_unchanged = src_md.dev() == entry.dev
                            && src_md.ino() == entry.ino
                            && (entry.is_symlink || src_md.len() == entry.size)
                            && (entry.is_symlink
                                || entry.mtime.is_none()
                                || src_md.modified().ok() == entry.mtime);
                        if !source_unchanged {
                            continue;
                        }
                        let destination_matches =
                            regular_files_equal(&src_file, &dst_item).unwrap_or(false);
                        if !destination_matches {
                            continue;
                        }
                        if use_sudo {
                            privileged_deletes.push((src_file, entry.size));
                        } else if remove_single_file(&src_file, false, mode) {
                            removed.files = removed.files.saturating_add(1);
                            removed.bytes = removed.bytes.saturating_add(entry.size);
                        } else {
                            removed.success = false;
                        }
                        report_progress(false, &removed);
                    }
                }
                const PRIVILEGED_DELETE_CHUNK: usize = 256;
                for chunk in privileged_deletes.chunks(PRIVILEGED_DELETE_CHUNK) {
                    let mut cmd = vec!["rm".to_string(), "-f".to_string(), "--".to_string()];
                    cmd.extend(chunk.iter().map(|(path, _)| path.display().to_string()));
                    let command_ok = run_command_capture(&cmd, true)
                        .map(|output| output.code == 0)
                        .unwrap_or(false);
                    if !command_ok {
                        removed.success = false;
                    }
                    for (path, size) in chunk {
                        if fs::symlink_metadata(path).is_err() {
                            removed.files = removed.files.saturating_add(1);
                            removed.bytes = removed.bytes.saturating_add(*size);
                        } else {
                            removed.success = false;
                        }
                    }
                }
                report_progress(true, &removed);
                return removed;
            }

            for ent in WalkDir::new(src_root)
                .sort(false)
                .skip_hidden(false)
                .parallelism(jwalk::Parallelism::RayonDefaultPool {
                    busy_timeout: Duration::from_secs(0),
                })
                .into_iter()
            {
                let ent = match ent {
                    Ok(ent) => ent,
                    Err(_) => {
                        removed.success = false;
                        continue;
                    }
                };
                let p = ent.path();
                if p == src_root {
                    continue;
                }
                let md = match fs::symlink_metadata(&p) {
                    Ok(v) => v,
                    Err(_) => {
                        removed.success = false;
                        continue;
                    }
                };
                if !md.is_file() && !md.file_type().is_symlink() {
                    continue;
                }
                let rel = match p.strip_prefix(src_root) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let rel_str = normalize_rel(rel);
                if exclude_rel
                    .map(|prefix| rel_matches_prefix(&rel_str, prefix))
                    .unwrap_or(false)
                {
                    continue;
                }
                let dst_item = if include_root {
                    dst_base.join(&src_base).join(rel)
                } else {
                    dst_base.join(rel)
                };

                if p == dst_item {
                    continue;
                }

                let same = if md.file_type().is_symlink() {
                    symlink_targets_equal(&p, &dst_item)
                } else {
                    match fs::metadata(&dst_item) {
                        Ok(dm) => dm.is_file() && dm.len() == md.len(),
                        Err(_) => false,
                    }
                };
                if same {
                    if remove_single_file(&p, use_sudo, mode) {
                        removed.files += 1;
                        removed.bytes += md.len();
                    } else {
                        removed.success = false;
                    }
                }
                report_progress(false, &removed);
            }
        }
    }

    report_progress(true, &removed);
    removed
}
