use crate::path::{realpath_allow_missing, realpath_preserve_final_symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountInfo {
    pub source: PathBuf,
    pub mount_point: PathBuf,
    pub filesystem: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalSymlinkPolicy {
    Follow,
    Preserve,
}

pub fn mount_for_path(path: &Path) -> Option<MountInfo> {
    mount_for_path_with_policy(path, FinalSymlinkPolicy::Follow)
}

pub fn mount_for_path_with_policy(
    path: &Path,
    final_symlink_policy: FinalSymlinkPolicy,
) -> Option<MountInfo> {
    let probe = existing_probe_path(path)?;
    let probe = match final_symlink_policy {
        FinalSymlinkPolicy::Follow => realpath_allow_missing(&probe),
        FinalSymlinkPolicy::Preserve => realpath_preserve_final_symlink(&probe),
    };
    let mounts = cached_mounts()?;
    let mut best: Option<MountInfo> = None;

    for mount in mounts.iter() {
        if !path_is_within_mount(&probe, &mount.mount_point) {
            continue;
        }
        if best.as_ref().is_some_and(|current| {
            current.mount_point.components().count() >= mount.mount_point.components().count()
        }) {
            continue;
        }
        best = Some(mount.clone());
    }
    best
}

fn path_is_within_mount(path: &Path, mount_point: &Path) -> bool {
    path == mount_point
        || path
            .strip_prefix(mount_point)
            .is_ok_and(|relative| relative.components().next().is_some())
}

fn cached_mounts() -> Option<Arc<Vec<MountInfo>>> {
    type MountCache = Option<(Instant, Arc<Vec<MountInfo>>)>;
    static CACHE: OnceLock<Mutex<MountCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().ok()?;
    if let Some((created, mounts)) = guard.as_ref()
        && created.elapsed() < Duration::from_secs(5)
    {
        return Some(Arc::clone(mounts));
    }

    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut mounts = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut left_fields = left.split_whitespace();
        let Some(mount_point) = left_fields.nth(4) else {
            continue;
        };
        let mut right_fields = right.split_whitespace();
        let Some(filesystem) = right_fields.next() else {
            continue;
        };
        let Some(source) = right_fields.next() else {
            continue;
        };
        mounts.push(MountInfo {
            source: PathBuf::from(unescape_mount_field(source)),
            mount_point: PathBuf::from(unescape_mount_field(mount_point)),
            filesystem: filesystem.to_string(),
        });
    }
    let mounts = Arc::new(mounts);
    *guard = Some((Instant::now(), Arc::clone(&mounts)));
    Some(mounts)
}

fn existing_probe_path(path: &Path) -> Option<PathBuf> {
    let mut probe = if path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        path.to_path_buf()
    };
    loop {
        if std::fs::symlink_metadata(&probe).is_ok() {
            return Some(probe);
        }
        if !probe.pop() {
            return None;
        }
    }
}

pub fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = [bytes[index + 1], bytes[index + 2], bytes[index + 3]];
            if octal.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                output.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + octal[2] - b'0');
                index += 4;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}
