//! Process, device, mount, and scheduler telemetry collection.

use super::command::run_command_capture;
use crate::domain::{
    DeviceIoDeltas, DeviceIoWindow, LogLevel, ProcIoCounters, ProcIoDeltas, ProcessIoWindow,
    TransferMode, TransferProgressRates,
};
use crate::output::log;
use crate::plan::{existing_probe_path, realpath_allow_missing};
use nix::sys::stat::{major, minor};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub(crate) fn read_diskstats_bytes_for_keys(
    src_keys: &[(u64, u64)],
    dst_keys: &[(u64, u64)],
) -> io::Result<(Option<u64>, Option<u64>)> {
    let raw = fs::read_to_string("/proc/diskstats")?;
    let mut read_total = 0u64;
    let mut write_total = 0u64;
    let mut found_read = false;
    let mut found_write = false;
    for line in raw.lines() {
        let mut columns = line.split_whitespace();
        let maj = match columns.next().and_then(|value| value.parse::<u64>().ok()) {
            Some(v) => v,
            None => continue,
        };
        let min = match columns.next().and_then(|value| value.parse::<u64>().ok()) {
            Some(v) => v,
            None => continue,
        };
        let key = (maj, min);
        if !src_keys.contains(&key) && !dst_keys.contains(&key) {
            continue;
        }
        let _name = columns.next();
        let _reads_completed = columns.next();
        let _reads_merged = columns.next();
        let sectors_read = match columns.next().and_then(|value| value.parse::<u64>().ok()) {
            Some(v) => v,
            None => continue,
        };
        let _read_ms = columns.next();
        let _writes_completed = columns.next();
        let _writes_merged = columns.next();
        let sectors_written = match columns.next().and_then(|value| value.parse::<u64>().ok()) {
            Some(v) => v,
            None => continue,
        };
        if src_keys.contains(&key) {
            found_read = true;
            read_total = read_total.saturating_add(sectors_read.saturating_mul(512));
        }
        if dst_keys.contains(&key) {
            found_write = true;
            write_total = write_total.saturating_add(sectors_written.saturating_mul(512));
        }
    }
    Ok((
        found_read.then_some(read_total),
        found_write.then_some(write_total),
    ))
}

pub(crate) fn unescape_mountinfo_field(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let oct = &raw[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub(crate) fn mount_source_device_for_path(path: &Path) -> Option<PathBuf> {
    let probe = existing_probe_path(path)?;
    let probe_real = realpath_allow_missing(&probe);
    let raw = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, PathBuf)> = None;
    for line in raw.lines() {
        let (left, right) = line.split_once(" - ")?;
        let left_cols: Vec<&str> = left.split_whitespace().collect();
        if left_cols.len() < 5 {
            continue;
        }
        let right_cols: Vec<&str> = right.split_whitespace().collect();
        if right_cols.len() < 2 {
            continue;
        }
        let mount_point = PathBuf::from(unescape_mountinfo_field(left_cols[4]));
        if !probe_real.starts_with(&mount_point) {
            continue;
        }
        let src = unescape_mountinfo_field(right_cols[1]);
        if !src.starts_with("/dev/") {
            continue;
        }
        let depth = mount_point.components().count();
        match &best {
            Some((best_depth, _)) if *best_depth >= depth => {}
            _ => best = Some((depth, PathBuf::from(src))),
        }
    }
    best.map(|(_, p)| p)
}

pub(crate) fn device_key_for_block_device(devnode: &Path) -> Option<(u64, u64)> {
    let md = fs::metadata(devnode).ok()?;
    let rdev = md.rdev();
    if rdev == 0 {
        return None;
    }
    Some((major(rdev), minor(rdev)))
}

pub(crate) fn parse_major_minor(raw: &str) -> Option<(u64, u64)> {
    let mut parts = raw.trim().split(':');
    let maj = parts.next()?.trim().parse::<u64>().ok()?;
    let min = parts.next()?.trim().parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((maj, min))
}

pub(crate) fn device_key_for_sys_block_name(name: &str) -> Option<(u64, u64)> {
    let dev_path = Path::new("/sys/class/block").join(name).join("dev");
    let raw = fs::read_to_string(dev_path).ok()?;
    parse_major_minor(&raw)
}

pub(crate) fn collect_leaf_keys_for_block_name(
    name: &str,
    visited: &mut HashSet<String>,
    out: &mut Vec<(u64, u64)>,
) {
    if !visited.insert(name.to_string()) {
        return;
    }

    let slaves_dir = Path::new("/sys/class/block").join(name).join("slaves");
    let mut had_slave = false;
    if let Ok(rd) = fs::read_dir(&slaves_dir) {
        for ent in rd.flatten() {
            let child_name = ent.file_name().to_string_lossy().to_string();
            if child_name.is_empty() {
                continue;
            }
            had_slave = true;
            collect_leaf_keys_for_block_name(&child_name, visited, out);
        }
    }

    if !had_slave {
        if let Some(k) = device_key_for_sys_block_name(name) {
            out.push(k);
        }
    }
}

pub(crate) fn leaf_device_keys_for_block_device(devnode: &Path) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    let canonical = fs::canonicalize(devnode).unwrap_or_else(|_| devnode.to_path_buf());
    if let Some(name) = canonical.file_name().and_then(|s| s.to_str()) {
        collect_leaf_keys_for_block_name(name, &mut visited, &mut out);
    }

    if out.is_empty() {
        if let Some(k) = device_key_for_block_device(devnode) {
            out.push(k);
        }
    }

    out.sort_unstable();
    out.dedup();
    out
}

pub(crate) fn local_path_from_transfer_arg(arg: &str) -> Option<PathBuf> {
    let trimmed = arg.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('/') || trimmed.starts_with("./") || trimmed.starts_with("../") {
        return Some(PathBuf::from(trimmed));
    }
    None
}

pub(crate) fn device_keys_for_path(path: &Path) -> Vec<(u64, u64)> {
    if let Some(devnode) = mount_source_device_for_path(path) {
        let keys = leaf_device_keys_for_block_device(&devnode);
        if !keys.is_empty() {
            return keys;
        }
    }
    let Some(probe) = existing_probe_path(path) else {
        return Vec::new();
    };
    let Some(md) = fs::metadata(probe).ok() else {
        return Vec::new();
    };
    let dev = md.dev();
    vec![(major(dev), minor(dev))]
}

pub(crate) fn block_name_for_dev_key(key: (u64, u64)) -> Option<String> {
    let link = PathBuf::from(format!("/sys/dev/block/{}:{}", key.0, key.1));
    let canon = fs::canonicalize(link).ok()?;
    canon.file_name().map(|s| s.to_string_lossy().to_string())
}

pub(crate) fn block_leaf_names_for_path(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = device_keys_for_path(path)
        .into_iter()
        .filter_map(block_name_for_dev_key)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

pub(crate) fn block_is_rotational(name: &str) -> Option<bool> {
    let p = Path::new("/sys/class/block")
        .join(name)
        .join("queue/rotational");
    let raw = fs::read_to_string(p).ok()?;
    match raw.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

pub(crate) fn parse_scheduler_info(raw: &str) -> (Option<String>, HashSet<String>) {
    let mut active: Option<String> = None;
    let mut available: HashSet<String> = HashSet::new();
    for tok in raw.split_whitespace() {
        let is_active = tok.starts_with('[') && tok.ends_with(']');
        let name = tok.trim_matches('[').trim_matches(']').to_string();
        if name.is_empty() {
            continue;
        }
        if is_active {
            active = Some(name.clone());
        }
        available.insert(name);
    }
    (active, available)
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn set_block_scheduler(name: &str, scheduler: &str, use_sudo: bool) -> bool {
    let p = Path::new("/sys/class/block")
        .join(name)
        .join("queue/scheduler");
    if fs::write(&p, scheduler).is_ok() {
        return true;
    }
    if !use_sudo {
        return false;
    }
    let script = format!(
        "printf %s {} > {}",
        shell_single_quote(scheduler),
        shell_single_quote(&p.display().to_string())
    );
    let cmd = vec!["sh".to_string(), "-c".to_string(), script];
    run_command_capture(&cmd, true)
        .map(|o| o.code == 0)
        .unwrap_or(false)
}

pub(crate) fn prefer_hdd_scheduler_for_paths(paths: &[&Path], use_sudo: bool, mode: TransferMode) {
    // Queue scheduler selection is a system-wide mutation and cannot be
    // safely restored if the process is killed. Make it an explicit opt-in
    // instead of changing unrelated workloads for every transfer.
    if std::env::var_os("COPY_RS_SET_HDD_SCHEDULER").is_none() {
        return;
    }
    let mut block_names: BTreeSet<String> = BTreeSet::new();
    for p in paths {
        for name in block_leaf_names_for_path(p) {
            block_names.insert(name);
        }
    }
    if block_names.is_empty() {
        return;
    }

    let mut changed: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for name in block_names {
        if !matches!(block_is_rotational(&name), Some(true)) {
            continue;
        }
        let sched_path = Path::new("/sys/class/block")
            .join(&name)
            .join("queue/scheduler");
        let raw = match fs::read_to_string(&sched_path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (active, available) = parse_scheduler_info(&raw);
        let desired = if available.contains("mq-deadline") {
            Some("mq-deadline")
        } else if available.contains("deadline") {
            Some("deadline")
        } else {
            None
        };
        let Some(desired_scheduler) = desired else {
            continue;
        };
        if active.as_deref() == Some(desired_scheduler) {
            continue;
        }
        if set_block_scheduler(&name, desired_scheduler, use_sudo) {
            changed.push(format!("{name}:{desired_scheduler}"));
        } else {
            failed.push(name);
        }
    }

    if !changed.is_empty() {
        log(
            mode,
            &format!("Using preferred HDD scheduler on {}", changed.join(", ")),
            LogLevel::Info,
        );
    }
    if !failed.is_empty() {
        log(
            mode,
            &format!(
                "Could not set preferred HDD scheduler on {} (insufficient permissions or unsupported device).",
                failed.join(", ")
            ),
            LogLevel::Warn,
        );
    }
}

pub(crate) fn read_proc_io_counters(pid: u32) -> Option<ProcIoCounters> {
    let path = format!("/proc/{pid}/io");
    let text = fs::read_to_string(path).ok()?;
    let mut counters = ProcIoCounters::default();
    let mut saw_rchar = false;
    let mut saw_wchar = false;
    let mut saw_read_bytes = false;
    let mut saw_write_bytes = false;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let value = parts.next()?.parse::<u64>().ok()?;
        match key {
            "rchar:" => {
                counters.rchar = value;
                saw_rchar = true;
            }
            "wchar:" => {
                counters.wchar = value;
                saw_wchar = true;
            }
            "read_bytes:" => {
                counters.read_bytes = value;
                saw_read_bytes = true;
            }
            "write_bytes:" => {
                counters.write_bytes = value;
                saw_write_bytes = true;
            }
            _ => {}
        }
    }
    if saw_rchar && saw_wchar && saw_read_bytes && saw_write_bytes {
        Some(counters)
    } else {
        None
    }
}

impl ProcessIoWindow {
    pub(crate) fn from_pid(pid: u32) -> Self {
        Self {
            pid,
            ..Self::default()
        }
    }

    pub(crate) fn sample(&mut self) -> TransferProgressRates {
        let mut rates = TransferProgressRates::default();
        let now = Instant::now();
        let now_counters = read_proc_io_counters(self.pid);
        if let Some(prev_at) = self.last_at {
            let dt = now.duration_since(prev_at).as_secs_f64().max(1e-6);
            if let (Some(cur), Some(prev)) = (now_counters, self.last_counters) {
                rates.rchar_bps = Some(cur.rchar.saturating_sub(prev.rchar) as f64 / dt);
                rates.wchar_bps = Some(cur.wchar.saturating_sub(prev.wchar) as f64 / dt);
                rates.read_bytes_bps =
                    Some(cur.read_bytes.saturating_sub(prev.read_bytes) as f64 / dt);
                rates.write_bytes_bps =
                    Some(cur.write_bytes.saturating_sub(prev.write_bytes) as f64 / dt);
            }
        }
        self.last_at = Some(now);
        if now_counters.is_some() {
            self.last_counters = now_counters;
        }
        rates
    }

    pub(crate) fn current_totals(&self) -> Option<ProcIoCounters> {
        read_proc_io_counters(self.pid)
    }
}

impl DeviceIoWindow {
    pub(crate) fn from_transfer_paths(src_path: &str, dst_path: &str) -> Self {
        let src_keys = local_path_from_transfer_arg(src_path)
            .as_deref()
            .map(device_keys_for_path)
            .unwrap_or_default();
        let dst_keys = local_path_from_transfer_arg(dst_path)
            .as_deref()
            .map(device_keys_for_path)
            .unwrap_or_default();
        Self { src_keys, dst_keys }
    }

    pub(crate) fn current_totals(&self) -> (Option<u64>, Option<u64>) {
        read_diskstats_bytes_for_keys(&self.src_keys, &self.dst_keys).unwrap_or((None, None))
    }
}

pub(crate) fn counter_delta(start: Option<u64>, end: Option<u64>) -> Option<u64> {
    match (start, end) {
        (Some(a), Some(b)) => Some(b.saturating_sub(a)),
        _ => None,
    }
}

pub(crate) fn proc_io_deltas(
    start: Option<ProcIoCounters>,
    end: Option<ProcIoCounters>,
) -> ProcIoDeltas {
    match (start, end) {
        (Some(a), Some(b)) => ProcIoDeltas {
            rchar: Some(b.rchar.saturating_sub(a.rchar)),
            wchar: Some(b.wchar.saturating_sub(a.wchar)),
            read_bytes: Some(b.read_bytes.saturating_sub(a.read_bytes)),
            write_bytes: Some(b.write_bytes.saturating_sub(a.write_bytes)),
        },
        _ => ProcIoDeltas::default(),
    }
}

pub(crate) fn device_io_deltas(
    start: (Option<u64>, Option<u64>),
    end: (Option<u64>, Option<u64>),
) -> DeviceIoDeltas {
    DeviceIoDeltas {
        read_complete: counter_delta(start.0, end.0),
        write_complete: counter_delta(start.1, end.1),
    }
}
