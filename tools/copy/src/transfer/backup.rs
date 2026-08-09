//! Destination backup naming and backup-copy operations.

use super::command::run_command_capture;
use super::copy_engine::copy_path_recursive;
use crate::domain::{LogLevel, TransferMode};
use crate::output::log;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod libc_time {
    use nix::libc::{localtime_r, time_t, tm};

    #[derive(Clone, Copy)]
    pub struct LocalTime {
        pub year: i32,
        pub month: i32,
        pub day: i32,
        pub hour: i32,
        pub min: i32,
        pub sec: i32,
    }

    impl LocalTime {
        pub fn from_unix(secs: i64) -> Option<Self> {
            let mut out = tm {
                tm_sec: 0,
                tm_min: 0,
                tm_hour: 0,
                tm_mday: 0,
                tm_mon: 0,
                tm_year: 0,
                tm_wday: 0,
                tm_yday: 0,
                tm_isdst: 0,
                #[cfg(any(target_env = "gnu", target_env = "musl"))]
                tm_gmtoff: 0,
                #[cfg(any(target_env = "gnu", target_env = "musl"))]
                tm_zone: std::ptr::null(),
            };
            let t: time_t = secs as time_t;
            let ptr = unsafe { localtime_r(&t, &mut out) };
            if ptr.is_null() {
                return None;
            }
            Some(Self {
                year: out.tm_year + 1900,
                month: out.tm_mon + 1,
                day: out.tm_mday,
                hour: out.tm_hour,
                min: out.tm_min,
                sec: out.tm_sec,
            })
        }

        pub fn fallback(secs: i64) -> Self {
            let sec = (secs % 60).abs() as i32;
            Self {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                min: 0,
                sec,
            }
        }
    }
}

pub(crate) fn backup_base_path(path: &Path) -> Option<PathBuf> {
    let src = if path == Path::new("/") {
        return None;
    } else {
        path
    };
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    let name = src.file_name()?.to_string_lossy().to_string();
    let now = chrono_like_stamp();
    Some(parent.join(format!("{name}.{now}")))
}

pub(crate) fn chrono_like_stamp() -> String {
    // YYYYMMDD-HHMMSS localtime without external crates.
    // Falls back to unix seconds formatting if conversion fails.
    let now = SystemTime::now();
    let secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs() as i64;
    let t = libc_time::LocalTime::from_unix(secs)
        .unwrap_or_else(|| libc_time::LocalTime::fallback(secs));
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.min, t.sec
    )
}

pub(crate) fn next_backup_candidate_from_base(base: &Path) -> Option<PathBuf> {
    for idx in 0..1000 {
        let candidate = if idx == 0 {
            base.to_path_buf()
        } else {
            PathBuf::from(format!("{}.{}", base.display(), idx))
        };
        if fs::symlink_metadata(&candidate).is_err() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn plan_backup_path(path: &Path) -> Option<PathBuf> {
    let base = backup_base_path(path)?;
    next_backup_candidate_from_base(&base)
}

pub(crate) fn backup_path_with_base(
    path: &Path,
    use_sudo: bool,
    base: &Path,
    mode: TransferMode,
) -> Option<PathBuf> {
    if path == Path::new("/") {
        log(mode, "Refusing to backup root path.", LogLevel::Error);
        return None;
    }

    for idx in 0..1000 {
        let candidate = if idx == 0 {
            base.to_path_buf()
        } else {
            PathBuf::from(format!("{}.{}", base.display(), idx))
        };
        if fs::symlink_metadata(&candidate).is_ok() {
            continue;
        }

        let ok = if use_sudo {
            let cmd = vec![
                "mv".to_string(),
                "--".to_string(),
                path.display().to_string(),
                candidate.display().to_string(),
            ];
            run_command_capture(&cmd, true)
                .map(|o| o.code == 0)
                .unwrap_or(false)
        } else {
            rename_noreplace(path, &candidate).is_ok()
        };

        if ok {
            return Some(candidate);
        }
    }

    log(
        mode,
        &format!(
            "Failed to create unique backup name for: {}",
            path.display()
        ),
        LogLevel::Error,
    );
    None
}

fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let source_path = source.to_path_buf();
        let destination_path = destination.to_path_buf();
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
        let rc = unsafe {
            nix::libc::renameat2(
                nix::libc::AT_FDCWD,
                source.as_ptr(),
                nix::libc::AT_FDCWD,
                destination.as_ptr(),
                1,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            if matches!(
                err.raw_os_error(),
                Some(nix::libc::EINVAL | nix::libc::ENOSYS | nix::libc::EOPNOTSUPP)
            ) {
                if fs::symlink_metadata(&destination_path).is_ok() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "backup destination already exists",
                    ));
                }
                return fs::rename(source_path, destination_path);
            }
            Err(err)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        if fs::symlink_metadata(destination).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "backup destination already exists",
            ));
        }
        fs::rename(source, destination)
    }
}

pub(crate) fn copy_path_to_backup(
    path: &Path,
    backup_path: &Path,
    use_sudo: bool,
    mode: TransferMode,
) -> Option<PathBuf> {
    let parent = backup_path.parent().unwrap_or_else(|| Path::new("."));
    let staging_dir = match tempfile::Builder::new()
        .prefix(".copy-rs-backup-")
        .tempdir_in(parent)
    {
        Ok(dir) => dir,
        Err(_) => {
            log(
                mode,
                &format!(
                    "Failed to allocate backup staging area: {}",
                    backup_path.display()
                ),
                LogLevel::Error,
            );
            return None;
        }
    };
    let staged = staging_dir.path().join("payload");
    let ok = if use_sudo {
        let cmd = vec![
            "cp".to_string(),
            "-a".to_string(),
            "--".to_string(),
            path.display().to_string(),
            staged.display().to_string(),
        ];
        let copied = run_command_capture(&cmd, true)
            .map(|o| o.code == 0)
            .unwrap_or(false);
        if copied {
            let mv = vec![
                "mv".to_string(),
                "--no-clobber".to_string(),
                "--".to_string(),
                staged.display().to_string(),
                backup_path.display().to_string(),
            ];
            run_command_capture(&mv, true)
                .map(|o| o.code == 0 && fs::symlink_metadata(backup_path).is_ok())
                .unwrap_or(false)
        } else {
            false
        }
    } else {
        copy_path_recursive(path, &staged)
            .and_then(|_| rename_noreplace(&staged, backup_path))
            .is_ok()
    };

    if !ok {
        log(
            mode,
            &format!("Failed to create backup copy: {}", path.display()),
            LogLevel::Error,
        );
        return None;
    }
    Some(backup_path.to_path_buf())
}
