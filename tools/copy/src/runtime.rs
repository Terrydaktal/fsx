//! Shared runtime tuning for local transfers.
//!
//! This module contains only process-wide transfer configuration. Planning,
//! execution, telemetry, and terminal output live in their own modules.

use crate::domain::{InflightWriteLimiter, InflightWritePermit, MediaKind};
use nix::sys::stat::{major, minor};
use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::thread;

pub(crate) fn option_u64_saturating_sub(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_sub(y)),
        _ => None,
    }
}

pub(crate) fn option_u64_saturating_add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

pub(crate) fn dev_media_kind(path: &Path) -> MediaKind {
    let mut probe = path;
    let metadata = loop {
        if let Ok(metadata) = fs::metadata(probe) {
            break metadata;
        }
        probe = match probe.parent() {
            Some(parent) if parent != probe => parent,
            _ => return MediaKind::Other,
        };
    };
    let device = metadata.dev();
    let sys_link = PathBuf::from(format!(
        "/sys/dev/block/{}:{}",
        major(device),
        minor(device)
    ));
    let canonical = match fs::canonicalize(&sys_link) {
        Ok(path) => path,
        Err(_) => return MediaKind::Other,
    };

    let mut rotational = None;
    let mut saw_nvme = false;
    for ancestor in canonical.ancestors() {
        if let Some(name) = ancestor.file_name() {
            let name = name.to_string_lossy();
            if name.starts_with("nvme") {
                saw_nvme = true;
            }
            let queue = Path::new("/sys/class/block")
                .join(name.as_ref())
                .join("queue/rotational");
            if let Ok(value) = fs::read_to_string(queue) {
                match value.trim() {
                    "0" => {
                        rotational = Some(false);
                        if name.starts_with("nvme") {
                            saw_nvme = true;
                        }
                        break;
                    }
                    "1" => {
                        rotational = Some(true);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    match rotational {
        Some(true) => MediaKind::Hdd,
        Some(false) if saw_nvme => MediaKind::Nvme,
        Some(false) => MediaKind::Other,
        None if canonical.to_string_lossy().contains("/nvme") => MediaKind::Nvme,
        _ => MediaKind::Other,
    }
}

fn device_id(path: &Path) -> Option<u64> {
    let mut probe = path;
    let metadata = loop {
        if let Ok(metadata) = fs::metadata(probe) {
            break metadata;
        }
        probe = match probe.parent() {
            Some(parent) if parent != probe => parent,
            _ => return None,
        };
    };
    Some(metadata.dev())
}

/// Return a stable, path-independent key for the source/destination device pair.
/// It lets ETA priors follow a storage topology without persisting user paths.
pub(crate) fn transfer_profile_key(source: &Path, destination: &Path) -> u64 {
    let mut key = 0xcbf2_9ce4_8422_2325u64;
    for value in [
        device_id(source).unwrap_or(0),
        device_id(destination).unwrap_or(0),
    ] {
        for byte in value.to_le_bytes() {
            key ^= u64::from(byte);
            key = key.wrapping_mul(0x1000_0000_01b3);
        }
    }
    key
}

pub(crate) fn symlink_targets_equal(src: &Path, dst: &Path) -> bool {
    let src_md = match fs::symlink_metadata(src) {
        Ok(metadata) if metadata.file_type().is_symlink() => metadata,
        _ => return false,
    };
    let dst_md = match fs::symlink_metadata(dst) {
        Ok(metadata) if metadata.file_type().is_symlink() => metadata,
        _ => return false,
    };
    let _ = (src_md, dst_md);
    match (fs::read_link(src), fs::read_link(dst)) {
        (Ok(source), Ok(destination)) => source == destination,
        _ => false,
    }
}

fn parse_env_threads() -> Option<usize> {
    let raw = env::var("COPY_RS_THREADS").ok()?;
    let parsed = raw.trim().parse::<usize>().ok()?;
    (parsed != 0).then_some(parsed)
}

fn parse_env_u64(name: &str) -> Option<u64> {
    let raw = env::var(name).ok()?;
    let parsed = raw.trim().parse::<u64>().ok()?;
    (parsed != 0).then_some(parsed)
}

fn preferred_thread_count(media: MediaKind) -> usize {
    if let Some(n) = parse_env_threads() {
        return match media {
            MediaKind::Hdd => n.clamp(1, 2),
            MediaKind::Nvme => n.clamp(2, 32),
            MediaKind::Other => n.clamp(1, 8),
        };
    }
    let logical = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    match media {
        MediaKind::Hdd => logical.clamp(2, 2),
        MediaKind::Nvme => logical.clamp(2, 32),
        MediaKind::Other => logical.clamp(1, 8),
    }
}

pub(crate) fn transfer_media_kind(source: MediaKind, destination: MediaKind) -> MediaKind {
    if source == MediaKind::Hdd || destination == MediaKind::Hdd {
        MediaKind::Hdd
    } else if source == MediaKind::Nvme && destination == MediaKind::Nvme {
        MediaKind::Nvme
    } else {
        MediaKind::Other
    }
}

pub(crate) fn copy_chunk_bytes_for_media(media: MediaKind) -> usize {
    if let Some(kib) = parse_env_u64("COPY_RS_CHUNK_KIB") {
        return (kib.saturating_mul(1024)).max(64 * 1024) as usize;
    }
    match media {
        MediaKind::Hdd => 4 * 1024 * 1024,
        MediaKind::Nvme => 2 * 1024 * 1024,
        MediaKind::Other => 1024 * 1024,
    }
}

pub(crate) fn copy_chunk_bytes_for_file(media: MediaKind, file_size: u64) -> usize {
    const TINY: u64 = 64 * 1024;
    const MEDIUM: u64 = 4 * 1024 * 1024;
    if file_size <= TINY {
        64 * 1024
    } else if file_size <= MEDIUM {
        256 * 1024
    } else {
        copy_chunk_bytes_for_media(media)
    }
}

pub(crate) fn inflight_max_bytes_for_media(media: MediaKind) -> Option<u64> {
    if let Some(mib) = parse_env_u64("COPY_RS_MAX_INFLIGHT_MIB") {
        return Some(mib.saturating_mul(1024 * 1024));
    }
    match media {
        MediaKind::Hdd => Some(96 * 1024 * 1024),
        _ => None,
    }
}

fn inflight_reserve_bytes_for_file(file_size: u64, media: MediaKind) -> u64 {
    match media {
        MediaKind::Hdd => {
            let min_reserve = 4 * 1024 * 1024u64;
            let max_reserve = 32 * 1024 * 1024u64;
            file_size.max(min_reserve).min(max_reserve)
        }
        _ => file_size.max(1024 * 1024),
    }
}

pub(crate) fn acquire_file_write_permit(
    limiter: Option<&Arc<InflightWriteLimiter>>,
    file_size: u64,
    media: MediaKind,
) -> Option<InflightWritePermit> {
    let lim = limiter?;
    let reserve = inflight_reserve_bytes_for_file(file_size, media);
    Some(lim.acquire(reserve))
}

pub(crate) fn configure_rayon_threads_for_media(media: MediaKind) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let threads = preferred_thread_count(media);
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn dev_media_kind_probes_existing_parent_for_missing_target() {
        let td = tempdir().expect("tempdir");
        let existing = dev_media_kind(td.path());
        let missing = dev_media_kind(&td.path().join("not-created").join("target"));
        assert_eq!(missing, existing);
    }

    #[test]
    fn transfer_media_uses_conservative_endpoint_kind() {
        assert_eq!(
            transfer_media_kind(MediaKind::Nvme, MediaKind::Hdd),
            MediaKind::Hdd
        );
        assert_eq!(
            transfer_media_kind(MediaKind::Hdd, MediaKind::Nvme),
            MediaKind::Hdd
        );
        assert_eq!(
            transfer_media_kind(MediaKind::Nvme, MediaKind::Nvme),
            MediaKind::Nvme
        );
        assert_eq!(
            transfer_media_kind(MediaKind::Nvme, MediaKind::Other),
            MediaKind::Other
        );
    }
}
