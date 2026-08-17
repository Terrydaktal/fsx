//! Local Rust-backend transfer execution and worker coordination.
#![allow(clippy::too_many_arguments)]

use super::content::regular_file_contents_equal;
use super::copy_engine::{
    copy_file_preserve_atomic_with_progress_buf, copy_file_preserve_with_progress_buf,
    copy_hardlink_atomic, copy_symlink_atomic, ensure_directory_target, interrupted,
    preserve_directory_times_tree, verify_regular_file_pair,
};
use super::telemetry::{counter_delta, device_io_deltas, proc_io_deltas};
use crate::domain::{
    AtomicEtaProgress, DeviceIoWindow, EtaWorkload, InflightWriteLimiter, LogLevel, MediaKind,
    MergeCollisionPolicy, ProcessIoWindow, ProgressSnapshot, SrcObjKind, TransferManifest,
    TransferMode, TransferOutcome, TransferProgressRates,
};
use crate::output::{
    log, print_transfer_columns_header, print_transfer_progress_bars, TransferEtaEstimator,
};
use crate::plan::{
    map_dir_dest_path, map_dir_dest_relative_path, normalize_rel, regular_file_collision_change,
    regular_file_relation_change, rel_matches_prefix,
};
use crate::runtime::{
    acquire_file_write_permit, copy_chunk_bytes_for_media, inflight_max_bytes_for_media,
    symlink_targets_equal, transfer_profile_key,
};
use jwalk::WalkDir;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn remember_transfer_error(
    errors: &Arc<Mutex<Option<String>>>,
    operation: &str,
    path: &Path,
    error: &io::Error,
) {
    let message = format!("{operation} '{}': {error}", path.display());
    if let Ok(mut slot) = errors.lock() {
        if slot.is_none() {
            *slot = Some(message);
        }
    }
}

pub(crate) fn run_rust_transfer(
    src_path: &Path,
    dst_path: &Path,
    include_root: bool,
    src_obj_kind: SrcObjKind,
    _is_move: bool,
    requested_mode: TransferMode,
    planned_bytes: u64,
    manifest: Option<&TransferManifest>,
    media: MediaKind,
    replace_dest_symlink: bool,
    merge_collision_policy: MergeCollisionPolicy,
    sync_mode: bool,
    exclude_rel: Option<&str>,
    verify: bool,
) -> TransferOutcome {
    super::copy_engine::install_interrupt_handler();
    let done = Arc::new(AtomicU64::new(0));
    let transfer_errors = Arc::new(Mutex::new(None::<String>));
    let copy_buf_bytes = copy_chunk_bytes_for_media(media);
    let inflight_limiter = inflight_max_bytes_for_media(media)
        .map(InflightWriteLimiter::new)
        .map(Arc::new);
    let eta_workload = manifest
        .map(|m| {
            EtaWorkload::from_manifest(
                m,
                src_obj_kind == SrcObjKind::Dir && include_root,
                media,
                transfer_profile_key(src_path, dst_path),
            )
        })
        .or_else(|| {
            (src_obj_kind == SrcObjKind::File)
                .then(|| {
                    fs::symlink_metadata(src_path).ok().map(|meta| {
                        EtaWorkload::from_file(
                            meta.len(),
                            media,
                            transfer_profile_key(src_path, dst_path),
                        )
                    })
                })
                .flatten()
        });
    let eta_progress = Arc::new(AtomicEtaProgress::default());
    let transfer_start = Instant::now();
    print_transfer_columns_header();
    let io_window_for_avg = ProcessIoWindow::from_pid(std::process::id());
    let io_start_counters = io_window_for_avg.current_totals();
    let device_window_for_avg = DeviceIoWindow::from_local_paths(src_path, dst_path);
    let device_start_totals = device_window_for_avg.current_totals();
    let transfer_start_for_ticker = transfer_start;
    let io_start_counters_for_ticker = io_start_counters;
    let device_start_totals_for_ticker = device_start_totals;
    let src_path_for_ticker = src_path.to_path_buf();
    let dst_path_for_ticker = dst_path.to_path_buf();
    let eta_workload_for_ticker = eta_workload.clone();
    let eta_progress_for_ticker = Arc::clone(&eta_progress);

    let done_for_ticker = Arc::clone(&done);
    let io_rates_shared = Arc::new(Mutex::new(TransferProgressRates::default()));
    let io_rates_for_ticker = Arc::clone(&io_rates_shared);
    let (ticker_stop_tx, ticker_stop_rx) = mpsc::channel::<()>();
    let ticker = thread::spawn(move || {
        let mut io_window = ProcessIoWindow::from_pid(std::process::id());
        let _ = io_window.sample();
        let device_window =
            DeviceIoWindow::from_local_paths(&src_path_for_ticker, &dst_path_for_ticker);
        let mut last_device_totals = device_start_totals_for_ticker;
        let mut last_device_at = transfer_start_for_ticker;
        let mut last_done_bytes: u64 = 0;
        let mut last_done_at = transfer_start_for_ticker;
        let mut eta_estimator = TransferEtaEstimator::default();
        loop {
            match ticker_stop_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    let done_bytes = done_for_ticker.load(Ordering::Relaxed);
                    let mut io_rates = io_window.sample();
                    let dt = now.duration_since(last_done_at).as_secs_f64().max(1e-6);
                    io_rates.write_all_bps =
                        Some(done_bytes.saturating_sub(last_done_bytes) as f64 / dt);
                    last_done_bytes = done_bytes;
                    last_done_at = now;
                    let io_delta =
                        proc_io_deltas(io_start_counters_for_ticker, io_window.last_counters);
                    let device_now_totals = device_window.current_totals();
                    let device_delta =
                        device_io_deltas(device_start_totals_for_ticker, device_now_totals);
                    let dt_dev = now.duration_since(last_device_at).as_secs_f64().max(1e-6);
                    io_rates.read_complete_bps =
                        counter_delta(last_device_totals.0, device_now_totals.0)
                            .map(|v| v as f64 / dt_dev);
                    io_rates.write_complete_bps =
                        counter_delta(last_device_totals.1, device_now_totals.1)
                            .map(|v| v as f64 / dt_dev);
                    last_device_totals = device_now_totals;
                    last_device_at = now;
                    if let Ok(mut g) = io_rates_for_ticker.lock() {
                        *g = io_rates;
                    }
                    print_transfer_progress_bars(
                        now.duration_since(transfer_start_for_ticker).as_secs_f64(),
                        planned_bytes,
                        Some(done_bytes),
                        "Transfer",
                        io_rates,
                        io_delta,
                        device_delta,
                        eta_workload_for_ticker.clone(),
                        Some(eta_progress_for_ticker.snapshot()),
                        Some(&mut eta_estimator),
                        false,
                        false,
                    );
                }
            }
        }
    });

    macro_rules! finish_transfer {
        ($rc:expr) => {{
            let transfer_rc = if interrupted() { 130 } else { $rc };
            let final_done = done.load(Ordering::Relaxed);
            let elapsed = transfer_start.elapsed().as_secs_f64().max(1e-6);
            let io_end_totals = io_window_for_avg.current_totals();
            let io_delta = proc_io_deltas(io_start_counters, io_end_totals);
            let device_end_totals = device_window_for_avg.current_totals();
            let device_delta = device_io_deltas(device_start_totals, device_end_totals);
            let mut final_io_rates = io_rates_shared.lock().map(|g| *g).unwrap_or_default();
            if final_io_rates.write_all_bps.is_none() {
                final_io_rates.write_all_bps = Some(final_done as f64 / elapsed);
            }
            if final_io_rates.rchar_bps.is_none() {
                final_io_rates.rchar_bps = io_delta.rchar.map(|v| v as f64 / elapsed);
            }
            if final_io_rates.wchar_bps.is_none() {
                final_io_rates.wchar_bps = io_delta.wchar.map(|v| v as f64 / elapsed);
            }
            if final_io_rates.read_bytes_bps.is_none() {
                final_io_rates.read_bytes_bps = io_delta.read_bytes.map(|v| v as f64 / elapsed);
            }
            if final_io_rates.write_bytes_bps.is_none() {
                final_io_rates.write_bytes_bps = io_delta.write_bytes.map(|v| v as f64 / elapsed);
            }
            if final_io_rates.read_complete_bps.is_none() {
                final_io_rates.read_complete_bps =
                    device_delta.read_complete.map(|v| v as f64 / elapsed);
            }
            if final_io_rates.write_complete_bps.is_none() {
                final_io_rates.write_complete_bps =
                    device_delta.write_complete.map(|v| v as f64 / elapsed);
            }
            let _ = ticker_stop_tx.send(());
            let _ = ticker.join();
            print_transfer_progress_bars(
                elapsed,
                planned_bytes,
                Some(final_done),
                "Transfer",
                final_io_rates,
                io_delta,
                device_delta,
                eta_workload.clone(),
                Some(eta_progress.snapshot()),
                None,
                true,
                false,
            );
            if let Ok(guard) = transfer_errors.lock() {
                if let Some(detail) = guard.as_deref() {
                    log(
                        requested_mode,
                        &format!("Rust backend failure: {detail}"),
                        LogLevel::Error,
                    );
                }
            }
            return TransferOutcome {
                rc: transfer_rc,
                bytes_done: final_done,
                elapsed_s: elapsed,
                progress_snapshot: Some(ProgressSnapshot {
                    elapsed_s: elapsed,
                    planned_bytes,
                    write_all_total: Some(final_done),
                    phase_label: "Transfer",
                    rates: final_io_rates,
                    proc_deltas: io_delta,
                    device_deltas: device_delta,
                    eta_workload: eta_workload.clone(),
                    eta_progress: Some(eta_progress.snapshot()),
                }),
            };
        }};
    }

    match src_obj_kind {
        SrcObjKind::File => {
            let src = src_path;
            let mut dst_buf = dst_path.to_path_buf();
            let dst_is_symlink = fs::symlink_metadata(&dst_buf)
                .map(|md| md.file_type().is_symlink())
                .unwrap_or(false);
            if dst_buf.is_dir() && !(replace_dest_symlink && dst_is_symlink) {
                let src_name = match src.file_name() {
                    Some(v) => v,
                    None => finish_transfer!(1),
                };
                dst_buf = dst_buf.join(src_name);
            }
            let dst = dst_buf.as_path();
            let src_lmd = match fs::symlink_metadata(src) {
                Ok(v) => v,
                Err(err) => {
                    remember_transfer_error(&transfer_errors, "read source metadata", src, &err);
                    finish_transfer!(1);
                }
            };
            if src_lmd.file_type().is_symlink() {
                let needs_copy = !symlink_targets_equal(src, dst);
                if needs_copy {
                    if let Err(err) = copy_symlink_atomic(src, dst) {
                        remember_transfer_error(&transfer_errors, "copy symlink", src, &err);
                        finish_transfer!(1);
                    }
                }
                eta_progress.mark_file(0);
                if let Some(workload) = eta_workload.as_ref() {
                    workload.mark_operation(0);
                }
                finish_transfer!(0);
            }
            let src_meta = match fs::metadata(src) {
                Ok(v) => v,
                Err(err) => {
                    remember_transfer_error(&transfer_errors, "read source metadata", src, &err);
                    finish_transfer!(1);
                }
            };
            let src_mtime = src_meta.modified().ok();
            let dst_lmd = fs::symlink_metadata(dst).ok();
            if dst_lmd
                .as_ref()
                .map(|meta| meta.file_type().is_dir())
                .unwrap_or(false)
                && !sync_mode
            {
                let err = io::Error::new(io::ErrorKind::IsADirectory, "Is a directory");
                remember_transfer_error(&transfer_errors, "copy file", dst, &err);
                finish_transfer!(1);
            }
            let dst_is_symlink = dst_lmd
                .as_ref()
                .map(|md| md.file_type().is_symlink())
                .unwrap_or(false);
            let dst_exists = dst_lmd.is_some();
            let dst_meta = if replace_dest_symlink && dst_is_symlink {
                None
            } else {
                fs::metadata(dst).ok()
            };
            let dst_size = dst_meta.as_ref().map(|m| m.len());
            let dst_mtime = dst_meta.as_ref().and_then(|m| m.modified().ok());
            let mut needs_copy = regular_file_collision_change(
                merge_collision_policy,
                src_meta.len(),
                src_mtime,
                dst_exists,
                dst_size,
                dst_mtime,
            )
            .is_some();
            if !needs_copy
                && merge_collision_policy.requires_content_identity_check()
                && dst_exists
                && !dst_is_symlink
            {
                match regular_file_contents_equal(src, dst) {
                    Ok(equal) => needs_copy = !equal,
                    Err(err) => {
                        remember_transfer_error(
                            &transfer_errors,
                            "compare file contents",
                            src,
                            &err,
                        );
                        finish_transfer!(1);
                    }
                }
            }
            if needs_copy {
                let _permit =
                    acquire_file_write_permit(inflight_limiter.as_ref(), src_meta.len(), media);
                let copy_result = if dst_is_symlink && !replace_dest_symlink {
                    copy_file_preserve_with_progress_buf(src, dst, copy_buf_bytes, |n| {
                        done.fetch_add(n, Ordering::Relaxed);
                    })
                } else {
                    copy_file_preserve_atomic_with_progress_buf(
                        src,
                        dst,
                        media,
                        copy_buf_bytes,
                        |n| {
                            done.fetch_add(n, Ordering::Relaxed);
                        },
                    )
                };
                if let Err(err) = copy_result {
                    remember_transfer_error(&transfer_errors, "copy file", src, &err);
                    finish_transfer!(1);
                }
            }
            if verify {
                if let Err(err) = verify_regular_file_pair(src, dst) {
                    remember_transfer_error(&transfer_errors, "verify file", src, &err);
                    finish_transfer!(1);
                }
            }
            eta_progress.mark_file(src_meta.len());
            if let Some(workload) = eta_workload.as_ref() {
                workload.mark_operation(0);
            }
        }
        SrcObjKind::Dir => {
            let src_root = src_path;
            let dst_base = dst_path;
            let Some(src_base) = src_root.file_name() else {
                finish_transfer!(1);
            };

            if include_root {
                let target = dst_base.join(src_base);
                if let Err(err) = ensure_directory_target(&target, sync_mode) {
                    remember_transfer_error(
                        &transfer_errors,
                        "create destination directory",
                        &target,
                        &err,
                    );
                    finish_transfer!(1);
                }
                eta_progress.mark_dir();
                if let Some(workload) = eta_workload.as_ref() {
                    workload.mark_operation(0);
                }
            } else if let Err(err) = ensure_directory_target(dst_base, sync_mode) {
                remember_transfer_error(
                    &transfer_errors,
                    "create destination directory",
                    dst_base,
                    &err,
                );
                finish_transfer!(1);
            }

            if let Some(m) = manifest {
                let dir_paths: FxHashMap<&str, &Path> = m
                    .dir_times
                    .iter()
                    .map(|entry| (entry.rel.as_str(), entry.relative_path.as_path()))
                    .collect();
                for (dir_index, rel) in m.dirs.iter().enumerate() {
                    let dst_dir = dir_paths
                        .get(rel.as_str())
                        .map(|relative_path| {
                            map_dir_dest_relative_path(
                                include_root,
                                src_base,
                                relative_path,
                                dst_base,
                            )
                        })
                        .unwrap_or_else(|| {
                            map_dir_dest_path(include_root, src_base, rel, dst_base)
                        });
                    if let Err(err) = ensure_directory_target(&dst_dir, sync_mode) {
                        remember_transfer_error(
                            &transfer_errors,
                            "create destination directory",
                            &dst_dir,
                            &err,
                        );
                        finish_transfer!(1);
                    }
                    eta_progress.mark_dir();
                    if let Some(workload) = eta_workload.as_ref() {
                        workload.mark_operation(usize::from(include_root) + dir_index);
                    }
                }
                let hardlink_anchors =
                    Arc::new(Mutex::new(FxHashMap::<(u64, u64), PathBuf>::default()));
                let hardlink_copy_lock = Arc::new(Mutex::new(()));
                let copy_ok = m
                    .copy_files
                    .par_iter()
                    .enumerate()
                    .map(|(file_index, entry)| {
                        let src_file = entry.source_path.clone().unwrap_or_else(|| {
                            entry
                                .relative_path
                                .as_deref()
                                .map(|path| src_root.join(path))
                                .unwrap_or_else(|| src_root.join(entry.rel.as_ref()))
                        });
                        let exact_relative_path = entry
                            .relative_path
                            .as_deref()
                            .or_else(|| src_file.strip_prefix(src_root).ok());
                        let dst_item = exact_relative_path
                            .map(|relative_path| {
                                map_dir_dest_relative_path(
                                    include_root,
                                    src_base,
                                    relative_path,
                                    dst_base,
                                )
                            })
                            .unwrap_or_else(|| {
                                map_dir_dest_path(include_root, src_base, &entry.rel, dst_base)
                            });
                        let src_md = match fs::symlink_metadata(&src_file) {
                            Ok(md) => md,
                            Err(err) => {
                                remember_transfer_error(
                                    &transfer_errors,
                                    "read source metadata",
                                    &src_file,
                                    &err,
                                );
                                return false;
                            }
                        };
                        if fs::symlink_metadata(&dst_item)
                            .map(|meta| meta.file_type().is_dir())
                            .unwrap_or(false)
                            && !sync_mode
                        {
                            let err = io::Error::new(io::ErrorKind::IsADirectory, "Is a directory");
                            remember_transfer_error(&transfer_errors, "copy file", &src_file, &err);
                            return false;
                        }
                        let hardlink_key = (!entry.is_symlink && entry.nlink > 1)
                            .then_some((entry.dev, entry.ino));
                        // A hardlink anchor must be published before another
                        // member is copied. Serialize only hardlinked files;
                        // unrelated files remain parallel.
                        let _hardlink_guard =
                            hardlink_key.and_then(|_| hardlink_copy_lock.lock().ok());
                        if let Some(key) = hardlink_key {
                            let anchor = hardlink_anchors
                                .lock()
                                .ok()
                                .and_then(|anchors| anchors.get(&key).cloned());
                            if let Some(anchor) = anchor {
                                match copy_hardlink_atomic(&anchor, &dst_item) {
                                    Ok(()) => {
                                        if verify {
                                            if let Err(err) =
                                                verify_regular_file_pair(&src_file, &dst_item)
                                            {
                                                remember_transfer_error(
                                                    &transfer_errors,
                                                    "verify hardlink",
                                                    &src_file,
                                                    &err,
                                                );
                                                return false;
                                            }
                                        }
                                        eta_progress.mark_file(0);
                                        if let Some(workload) = eta_workload.as_ref() {
                                            workload.mark_operation(
                                                usize::from(include_root)
                                                    + m.dirs.len()
                                                    + file_index,
                                            );
                                        }
                                        return true;
                                    }
                                    Err(err) => {
                                        remember_transfer_error(
                                            &transfer_errors,
                                            "create hardlink",
                                            &dst_item,
                                            &err,
                                        );
                                        return false;
                                    }
                                }
                            }
                        }
                        if src_md.file_type().is_symlink() {
                            let result = copy_symlink_atomic(&src_file, &dst_item);
                            match result {
                                Ok(()) => {
                                    eta_progress.mark_file(0);
                                    if let Some(workload) = eta_workload.as_ref() {
                                        workload.mark_operation(
                                            usize::from(include_root) + m.dirs.len() + file_index,
                                        );
                                    }
                                    true
                                }
                                Err(err) => {
                                    remember_transfer_error(
                                        &transfer_errors,
                                        "copy symlink",
                                        &src_file,
                                        &err,
                                    );
                                    false
                                }
                            }
                        } else if src_md.is_file() {
                            let src_mtime = src_md.modified().ok();
                            let dst_lmd = fs::symlink_metadata(&dst_item).ok();
                            let dst_is_symlink = dst_lmd
                                .as_ref()
                                .map(|md| md.file_type().is_symlink())
                                .unwrap_or(false);
                            let dst_exists = dst_lmd.is_some();
                            let dst_is_regular_file = dst_lmd
                                .as_ref()
                                .map(|md| md.file_type().is_file())
                                .unwrap_or(false);
                            let dst_meta = if replace_dest_symlink && dst_is_symlink {
                                None
                            } else {
                                fs::metadata(&dst_item).ok()
                            };
                            let dst_size = dst_meta.as_ref().map(|m| m.len());
                            let dst_mtime = dst_meta.as_ref().and_then(|m| m.modified().ok());
                            let mut needs_copy = if sync_mode {
                                regular_file_relation_change(
                                    src_md.len(),
                                    src_mtime,
                                    dst_is_regular_file,
                                    dst_size,
                                    dst_mtime,
                                )
                                .is_some()
                            } else {
                                regular_file_collision_change(
                                    merge_collision_policy,
                                    src_md.len(),
                                    src_mtime,
                                    dst_exists,
                                    dst_size,
                                    dst_mtime,
                                )
                                .is_some()
                            };
                            if !needs_copy
                                && !sync_mode
                                && merge_collision_policy.requires_content_identity_check()
                                && dst_is_regular_file
                                && !dst_is_symlink
                            {
                                match regular_file_contents_equal(&src_file, &dst_item) {
                                    Ok(equal) => needs_copy = !equal,
                                    Err(err) => {
                                        remember_transfer_error(
                                            &transfer_errors,
                                            "compare file contents",
                                            &src_file,
                                            &err,
                                        );
                                        return false;
                                    }
                                }
                            }
                            if !needs_copy {
                                if verify {
                                    if let Err(err) = verify_regular_file_pair(&src_file, &dst_item)
                                    {
                                        remember_transfer_error(
                                            &transfer_errors,
                                            "verify file",
                                            &src_file,
                                            &err,
                                        );
                                        return false;
                                    }
                                }
                                eta_progress.mark_file(src_md.len());
                                if let Some(workload) = eta_workload.as_ref() {
                                    workload.mark_operation(
                                        usize::from(include_root) + m.dirs.len() + file_index,
                                    );
                                }
                                return true;
                            }
                            let _permit = acquire_file_write_permit(
                                inflight_limiter.as_ref(),
                                src_md.len(),
                                media,
                            );
                            let result = if sync_mode || replace_dest_symlink || !dst_is_symlink {
                                copy_file_preserve_atomic_with_progress_buf(
                                    &src_file,
                                    &dst_item,
                                    media,
                                    copy_buf_bytes,
                                    |n| {
                                        done.fetch_add(n, Ordering::Relaxed);
                                    },
                                )
                                .map(|_| ())
                            } else {
                                copy_file_preserve_with_progress_buf(
                                    &src_file,
                                    &dst_item,
                                    copy_buf_bytes,
                                    |n| {
                                        done.fetch_add(n, Ordering::Relaxed);
                                    },
                                )
                                .map(|_| ())
                            };
                            match result {
                                Ok(()) => {
                                    if verify {
                                        if let Err(err) =
                                            verify_regular_file_pair(&src_file, &dst_item)
                                        {
                                            remember_transfer_error(
                                                &transfer_errors,
                                                "verify file",
                                                &src_file,
                                                &err,
                                            );
                                            return false;
                                        }
                                    }
                                    if let Some(key) = hardlink_key {
                                        if let Ok(mut anchors) = hardlink_anchors.lock() {
                                            anchors.entry(key).or_insert_with(|| dst_item.clone());
                                        }
                                    }
                                    eta_progress.mark_file(src_md.len());
                                    if let Some(workload) = eta_workload.as_ref() {
                                        workload.mark_operation(
                                            usize::from(include_root) + m.dirs.len() + file_index,
                                        );
                                    }
                                    true
                                }
                                Err(err) => {
                                    remember_transfer_error(
                                        &transfer_errors,
                                        "copy file",
                                        &src_file,
                                        &err,
                                    );
                                    false
                                }
                            }
                        } else {
                            true
                        }
                    })
                    .reduce(|| true, |a, b| a && b);
                if !copy_ok {
                    finish_transfer!(1);
                }
                if let Err(err) = preserve_directory_times_tree(
                    src_root,
                    dst_base,
                    include_root,
                    src_base,
                    manifest.map(|m| m.dir_times.as_slice()),
                ) {
                    remember_transfer_error(
                        &transfer_errors,
                        "preserve directory metadata",
                        src_root,
                        &err,
                    );
                    finish_transfer!(1);
                }
                eta_progress.mark_metadata(m.dir_times.len() as u64);
                if let Some(workload) = eta_workload.as_ref() {
                    let metadata_start =
                        usize::from(include_root) + m.dirs.len() + m.copy_files.len();
                    for index in 0..m.dir_times.len() {
                        workload.mark_operation(metadata_start + index);
                    }
                }
            } else {
                let mut entries: Vec<PathBuf> = Vec::new();
                for result in WalkDir::new(src_root)
                    .sort(false)
                    .skip_hidden(false)
                    .parallelism(jwalk::Parallelism::Serial)
                    .into_iter()
                {
                    let entry = match result {
                        Ok(entry) => entry,
                        Err(error) => {
                            let error = io::Error::other(error.to_string());
                            remember_transfer_error(
                                &transfer_errors,
                                "traverse source",
                                src_root,
                                &error,
                            );
                            finish_transfer!(1);
                        }
                    };
                    if let Some(error) = entry.read_children_error.as_ref() {
                        let error = io::Error::other(error.to_string());
                        remember_transfer_error(
                            &transfer_errors,
                            "read source directory",
                            &entry.path(),
                            &error,
                        );
                        finish_transfer!(1);
                    }
                    entries.push(entry.path().to_path_buf());
                }
                entries.sort();

                for p in entries {
                    if p == src_root {
                        continue;
                    }
                    let relative_path = p.strip_prefix(src_root).unwrap_or(Path::new(""));
                    let rel = normalize_rel(relative_path);
                    if exclude_rel
                        .map(|prefix| rel_matches_prefix(&rel, prefix))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let dst_item =
                        map_dir_dest_relative_path(include_root, src_base, relative_path, dst_base);
                    let md = match fs::symlink_metadata(&p) {
                        Ok(v) => v,
                        Err(_) => finish_transfer!(1),
                    };
                    if md.is_dir() {
                        if fs::create_dir_all(&dst_item).is_err() {
                            finish_transfer!(1);
                        }
                        eta_progress.mark_dir();
                        continue;
                    }
                    if md.file_type().is_symlink() {
                        let needs_copy = !symlink_targets_equal(&p, &dst_item);
                        if needs_copy && copy_symlink_atomic(&p, &dst_item).is_err() {
                            finish_transfer!(1);
                        }
                        eta_progress.mark_file(0);
                        continue;
                    }
                    if !md.is_file() {
                        continue;
                    }
                    let src_mtime = md.modified().ok();
                    let dst_lmd = fs::symlink_metadata(&dst_item).ok();
                    let dst_is_symlink = dst_lmd
                        .as_ref()
                        .map(|meta| meta.file_type().is_symlink())
                        .unwrap_or(false);
                    let dst_exists = dst_lmd.is_some();
                    let dst_meta = if replace_dest_symlink && dst_is_symlink {
                        None
                    } else {
                        fs::metadata(&dst_item).ok()
                    };
                    let dst_size = dst_meta.as_ref().map(|m| m.len());
                    let dst_mtime = dst_meta.as_ref().and_then(|m| m.modified().ok());
                    let needs_copy = regular_file_collision_change(
                        merge_collision_policy,
                        md.len(),
                        src_mtime,
                        dst_exists,
                        dst_size,
                        dst_mtime,
                    )
                    .is_some();
                    if needs_copy {
                        let _permit =
                            acquire_file_write_permit(inflight_limiter.as_ref(), md.len(), media);
                        let copy_result = if dst_is_symlink && !replace_dest_symlink {
                            copy_file_preserve_with_progress_buf(
                                &p,
                                &dst_item,
                                copy_buf_bytes,
                                |n| {
                                    done.fetch_add(n, Ordering::Relaxed);
                                },
                            )
                        } else {
                            copy_file_preserve_atomic_with_progress_buf(
                                &p,
                                &dst_item,
                                media,
                                copy_buf_bytes,
                                |n| {
                                    done.fetch_add(n, Ordering::Relaxed);
                                },
                            )
                        };
                        if copy_result.is_err() {
                            finish_transfer!(1);
                        }
                    }
                    if verify {
                        if let Err(err) = verify_regular_file_pair(&p, &dst_item) {
                            remember_transfer_error(&transfer_errors, "verify file", &p, &err);
                            finish_transfer!(1);
                        }
                    }
                    eta_progress.mark_file(md.len());
                }
                if let Err(err) = preserve_directory_times_tree(
                    src_root,
                    dst_base,
                    include_root,
                    src_base,
                    manifest.map(|m| m.dir_times.as_slice()),
                ) {
                    remember_transfer_error(
                        &transfer_errors,
                        "preserve directory metadata",
                        src_root,
                        &err,
                    );
                    finish_transfer!(1);
                }
            }
        }
    }

    finish_transfer!(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DstObjKind;
    use crate::plan::{pre_scan_directory, pre_scan_file};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use tempfile::tempdir;

    #[test]
    fn non_utf8_file_root_keeps_exact_basename_in_preview_and_transfer() {
        let td = tempdir().expect("tempdir");
        let source_parent = td.path().join("source-parent");
        let destination = td.path().join("destination");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&destination).expect("destination");
        let source_name = OsString::from_vec(b"file-\xff".to_vec());
        let source = source_parent.join(&source_name);
        fs::write(&source, b"payload").expect("source file");

        let scan = pre_scan_file(
            &source,
            &destination,
            DstObjKind::DirExisting,
            true,
            true,
            false,
            MergeCollisionPolicy::default(),
            None,
        );
        assert!(scan.scan_complete);
        assert_eq!(scan.change_preview.len(), 1);
        assert_eq!(scan.change_preview[0].rel, "file-%FF");

        let outcome = run_rust_transfer(
            &source,
            &destination,
            false,
            SrcObjKind::File,
            false,
            TransferMode::Copy,
            scan.planned_bytes,
            None,
            MediaKind::Other,
            false,
            MergeCollisionPolicy::default(),
            false,
            None,
            false,
        );
        assert_eq!(outcome.rc, 0);
        assert_eq!(
            fs::read(destination.join(&source_name)).unwrap(),
            b"payload"
        );
        assert!(!destination.join("source").exists());
        assert!(!destination.join("file-%FF").exists());
    }

    #[test]
    fn non_utf8_directory_root_keeps_exact_basename_in_preview_and_transfer() {
        let td = tempdir().expect("tempdir");
        let source_parent = td.path().join("source-parent");
        let destination = td.path().join("destination");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&destination).expect("destination");
        let source_name = OsString::from_vec(b"dir-\xff".to_vec());
        let source = source_parent.join(&source_name);
        fs::create_dir(&source).expect("source directory");
        fs::write(source.join("payload"), b"directory payload").expect("source child");

        let scan = pre_scan_directory(
            &source,
            &destination,
            true,
            true,
            false,
            true,
            true,
            false,
            false,
            MergeCollisionPolicy::default(),
            None,
            None,
            false,
        );
        assert!(scan.scan_complete);
        assert!(scan
            .change_preview
            .iter()
            .any(|entry| entry.rel == "dir-%FF/"));
        let manifest = scan.transfer_manifest.expect("transfer manifest");

        let outcome = run_rust_transfer(
            &source,
            &destination,
            true,
            SrcObjKind::Dir,
            false,
            TransferMode::Copy,
            scan.planned_bytes,
            Some(&manifest),
            MediaKind::Other,
            false,
            MergeCollisionPolicy::default(),
            false,
            None,
            false,
        );
        assert_eq!(outcome.rc, 0);
        assert_eq!(
            fs::read(destination.join(&source_name).join("payload")).unwrap(),
            b"directory payload"
        );
        assert!(!destination.join("dir-%FF").exists());
    }

    #[test]
    fn non_manifest_transfer_keeps_exact_non_utf8_descendant_paths() {
        let td = tempdir().expect("tempdir");
        let source = td.path().join("source");
        let destination = td.path().join("destination");
        let child_name = OsString::from_vec(b"child-\xff".to_vec());
        fs::create_dir(&source).expect("source directory");
        fs::create_dir(&destination).expect("destination directory");
        fs::create_dir(source.join(&child_name)).expect("non-UTF-8 child directory");
        fs::write(source.join(&child_name).join("payload"), b"payload").expect("source child");

        let outcome = run_rust_transfer(
            &source,
            &destination,
            true,
            SrcObjKind::Dir,
            false,
            TransferMode::Copy,
            7,
            None,
            MediaKind::Other,
            false,
            MergeCollisionPolicy::default(),
            false,
            None,
            false,
        );

        assert_eq!(outcome.rc, 0);
        assert_eq!(
            fs::read(destination.join("source").join(&child_name).join("payload")).unwrap(),
            b"payload"
        );
        assert!(!destination.join("source").join("child-%FF").exists());
    }
}
