//! Transfer orchestration, batching, flush phases, and move cleanup phases.
#![allow(clippy::too_many_arguments)]

use super::cleanup::{
    cleanup_source_dirs, cleanup_source_dirs_from_manifest, delete_sync_destination_extras,
    prune_move_source_duplicates, remove_single_file,
};
use super::command::run_command_capture;
use super::copy_engine::{
    copy_file_preserve_with_progress_buffer, copy_symlink, preserve_directory_times_tree,
    remove_path_local_if_exists,
};
use super::rsync::run_rsync_transfer_sources;
use super::telemetry::{counter_delta, device_io_deltas, proc_io_deltas, read_proc_io_counters};
use crate::domain::{
    AtomicEtaProgress, ChangeItem, DeleteCleanupOutcome, DeviceIoDeltas, DeviceIoWindow,
    DstObjKind, EtaWorkload, FileRelationBreakdown, InflightWriteLimiter, LogLevel, MediaKind,
    MergeCollisionPolicy, ProcIoCounters, ProcIoDeltas, ProcessIoWindow, ProgressSnapshot,
    SrcObjKind, TransferBackend, TransferManifest, TransferMode, TransferOutcome,
    TransferProgressRates, TransferRateSmoother,
};
use crate::output::{
    format_bytes_binary, format_number, log, log_transfer_complete, print_changed_top_preview,
    print_copy_duration_summary, print_counts_table, print_preview_counts_table,
    print_preview_root_line, print_transfer_progress_bars, reset_progress_render_state,
    TransferEtaEstimator,
};
use crate::plan::{
    build_destination_index, can_fast_rename_same_fs, pre_scan_file, realpath_allow_missing,
    resolve_destination_for_dir, resolve_source, DestinationKind,
};
use crate::runtime::{
    acquire_file_write_permit, configure_rayon_threads_for_media, dev_media_kind,
    inflight_max_bytes_for_media, option_u64_saturating_add, symlink_targets_equal,
    transfer_media_kind, transfer_profile_key,
};
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn run_rust_file_batch(
    items: &[(PathBuf, u64, bool)],
    destination: &Path,
    planned_bytes: u64,
    media: MediaKind,
    replace_dest_symlink: bool,
    requested_mode: TransferMode,
) -> TransferOutcome {
    let done = Arc::new(AtomicU64::new(0));
    let eta_progress = Arc::new(AtomicEtaProgress::default());
    let mut file_sizes = Vec::new();
    let mut eta_item_indices = vec![None; items.len()];

    for (item_index, (source, _, needs_copy)) in items.iter().enumerate() {
        if !*needs_copy {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(source) {
            let size = if meta.is_file() { meta.len() } else { 0 };
            eta_item_indices[item_index] = Some(file_sizes.len());
            file_sizes.push(size);
        }
    }
    let profile_key = items
        .first()
        .map(|(source, _, _)| transfer_profile_key(source, destination))
        .unwrap_or(0);
    let eta_workload = EtaWorkload::from_file_sizes(&file_sizes, media, profile_key);
    let eta_item_indices = Arc::new(eta_item_indices);

    let limiter =
        inflight_max_bytes_for_media(media).map(|max| Arc::new(InflightWriteLimiter::new(max)));
    let transfer_start = Instant::now();
    let collect_telemetry = planned_bytes >= 8 * 1024 * 1024 || items.len() >= 256;
    let process_window = ProcessIoWindow::from_pid(std::process::id());
    let process_start = collect_telemetry
        .then(|| process_window.current_totals())
        .flatten();
    let destination_text = destination.display().to_string();
    let device_window = collect_telemetry
        .then(|| DeviceIoWindow::from_transfer_paths(&destination_text, &destination_text));
    let device_start = device_window
        .as_ref()
        .map(DeviceIoWindow::current_totals)
        .unwrap_or_default();

    let rates_shared = Arc::new(Mutex::new(TransferProgressRates::default()));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let done_for_ticker = Arc::clone(&done);
    let progress_for_ticker = Arc::clone(&eta_progress);
    let rates_for_ticker = Arc::clone(&rates_shared);
    let destination_for_ticker = destination_text.clone();
    let eta_workload_for_ticker = eta_workload.clone();
    let ticker = collect_telemetry.then(|| {
        thread::spawn(move || {
            let mut process = ProcessIoWindow::from_pid(std::process::id());
            let _ = process.sample();
            let device = DeviceIoWindow::from_transfer_paths(
                &destination_for_ticker,
                &destination_for_ticker,
            );
            let mut previous_device = device_start;
            let mut previous_device_at = transfer_start;
            let mut previous_done = 0u64;
            let mut previous_done_at = transfer_start;
            let mut estimator = TransferEtaEstimator::default();
            let mut smoother = TransferRateSmoother::default();

            loop {
                match stop_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let now = Instant::now();
                        let current_done = done_for_ticker.load(Ordering::Relaxed);
                        let dt = now.duration_since(previous_done_at).as_secs_f64().max(1e-6);
                        let mut rates = process.sample();
                        rates.write_all_bps =
                            Some(current_done.saturating_sub(previous_done) as f64 / dt);
                        previous_done = current_done;
                        previous_done_at = now;

                        let proc_delta = proc_io_deltas(process_start, process.last_counters);
                        let device_now = device.current_totals();
                        let device_delta = device_io_deltas(device_start, device_now);
                        let device_dt = now
                            .duration_since(previous_device_at)
                            .as_secs_f64()
                            .max(1e-6);
                        rates.read_complete_bps = counter_delta(previous_device.0, device_now.0)
                            .map(|value| value as f64 / device_dt);
                        rates.write_complete_bps = counter_delta(previous_device.1, device_now.1)
                            .map(|value| value as f64 / device_dt);
                        rates = smoother.update(rates, dt);
                        previous_device = device_now;
                        previous_device_at = now;

                        if let Ok(mut shared) = rates_for_ticker.lock() {
                            *shared = rates;
                        }
                        print_transfer_progress_bars(
                            now.duration_since(transfer_start).as_secs_f64(),
                            planned_bytes,
                            Some(current_done),
                            "Transfer",
                            rates,
                            proc_delta,
                            device_delta,
                            Some(eta_workload_for_ticker.clone()),
                            Some(progress_for_ticker.snapshot()),
                            Some(&mut estimator),
                            false,
                            false,
                        );
                    }
                }
            }
        })
    });

    let cancelled = AtomicBool::new(false);
    let copy_result: Result<(), String> = items.par_iter().enumerate().try_for_each_init(
        Vec::new,
        |buffer, (item_index, (source, _, needs_copy))| {
            if !*needs_copy {
                return Ok(());
            }
            if cancelled.load(Ordering::Relaxed) {
                return Err("transfer cancelled after another worker failed".to_string());
            }

            let source_meta = fs::symlink_metadata(source)
                .map_err(|err| format!("read source metadata '{}': {err}", source.display()))?;
            let name = source.file_name().ok_or_else(|| {
                format!(
                    "resolve destination name for '{}': source has no filename",
                    source.display()
                )
            })?;
            let target = destination.join(name);
            if source_meta.file_type().is_symlink() {
                copy_symlink(source, &target)
                    .map_err(|err| format!("copy symlink '{}': {err}", source.display()))?;
                eta_progress.mark_file(0);
                if let Some(operation_index) = eta_item_indices[item_index] {
                    eta_workload.mark_operation(operation_index);
                }
                return Ok(());
            }

            let size = source_meta.len();
            let _permit = acquire_file_write_permit(limiter.as_ref(), size, media);
            if replace_dest_symlink
                && fs::symlink_metadata(&target)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false)
            {
                remove_path_local_if_exists(&target)
                    .map_err(|err| format!("remove destination '{}': {err}", target.display()))?;
            }

            copy_file_preserve_with_progress_buffer(source, &target, media, buffer, |count| {
                done.fetch_add(count, Ordering::Relaxed);
            })
            .map_err(|err| format!("copy file '{}': {err}", source.display()))?;
            eta_progress.mark_file(size);
            if let Some(operation_index) = eta_item_indices[item_index] {
                eta_workload.mark_operation(operation_index);
            }
            Ok(())
        },
    );

    if copy_result.is_err() {
        cancelled.store(true, Ordering::Relaxed);
    }
    if let Some(ticker) = ticker {
        let _ = stop_tx.send(());
        let _ = ticker.join();
    }

    let elapsed = transfer_start.elapsed().as_secs_f64().max(1e-6);
    let final_done = done.load(Ordering::Relaxed);
    let process_end = collect_telemetry
        .then(|| process_window.current_totals())
        .flatten();
    let process_delta = proc_io_deltas(process_start, process_end);
    let device_end = device_window
        .as_ref()
        .map(DeviceIoWindow::current_totals)
        .unwrap_or_default();
    let device_delta = device_io_deltas(device_start, device_end);
    let mut rates = rates_shared.lock().map(|rates| *rates).unwrap_or_default();
    rates.write_all_bps = Some(final_done as f64 / elapsed);
    print_transfer_progress_bars(
        elapsed,
        planned_bytes,
        Some(final_done),
        "Transfer",
        rates,
        process_delta,
        device_delta,
        Some(eta_workload.clone()),
        Some(eta_progress.snapshot()),
        None,
        true,
        false,
    );
    if let Err(detail) = &copy_result {
        log(
            requested_mode,
            &format!("Rust backend failure: {detail}"),
            LogLevel::Error,
        );
    }

    TransferOutcome {
        rc: i32::from(copy_result.is_err()),
        bytes_done: final_done,
        elapsed_s: elapsed,
        progress_snapshot: Some(ProgressSnapshot {
            elapsed_s: elapsed,
            planned_bytes,
            write_all_total: Some(final_done),
            phase_label: "Transfer",
            rates,
            proc_deltas: process_delta,
            device_deltas: device_delta,
            eta_workload: Some(eta_workload),
            eta_progress: Some(eta_progress.snapshot()),
        }),
    }
}
pub(crate) fn run_multi_source_file_batch(
    requested_mode: TransferMode,
    source_paths: &[String],
    destination: &str,
    use_sudo: bool,
    preview_only: bool,
    is_move: bool,
    tree_trunc: usize,
    showall: bool,
    replace_dest_symlink: bool,
    merge_collision_policy: MergeCollisionPolicy,
) -> i32 {
    let (dst_mnt, dst_obj_kind) =
        match resolve_destination_for_dir(destination, requested_mode, false) {
            Ok(v) => v,
            Err(code) => return code,
        };
    if dst_obj_kind != DstObjKind::DirExisting {
        log(
            requested_mode,
            "Multiple source paths require an existing destination directory.",
            LogLevel::Error,
        );
        return 1;
    }

    let dst_real = realpath_allow_missing(&dst_mnt);
    let dst_display = format!("{}/", dst_mnt.display().to_string().trim_end_matches('/'));
    let mut seen_names: FxHashSet<String> = FxHashSet::default();
    let mut batch_items: Vec<(PathBuf, u64, bool)> = Vec::new();
    let mut planned_bytes: u64 = 0;
    let mut total_regular_files: u64 = 0;
    let mut add_files: u64 = 0;
    let mut mod_files: u64 = 0;
    let mut display_change_preview: Vec<ChangeItem> = Vec::new();
    let mut source_top_entries: HashSet<String> = HashSet::new();
    let mut has_itemized_changes = false;
    let mut source_rel_files: HashSet<String> = HashSet::default();
    let mut file_relation_breakdown = FileRelationBreakdown::default();
    let destination_index = build_destination_index(&dst_mnt);

    for source in source_paths {
        let (src_mnt, src_obj_kind) = match resolve_source(source, requested_mode) {
            Ok(v) => v,
            Err(code) => return code,
        };
        if src_obj_kind != SrcObjKind::File {
            log(
                requested_mode,
                "Multiple source path support currently only handles files.",
                LogLevel::Error,
            );
            return 1;
        }

        let src_parent_real =
            realpath_allow_missing(src_mnt.parent().unwrap_or_else(|| Path::new(".")));
        if src_parent_real == dst_real {
            log(
                requested_mode,
                "Multiple source file glob mode requires a different destination directory.",
                LogLevel::Error,
            );
            return 1;
        }

        let src_name = src_mnt
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "source".to_string());
        if !seen_names.insert(src_name.clone()) {
            log(
                requested_mode,
                &format!(
                    "Multiple source paths resolve to the same destination name: {}",
                    src_name
                ),
                LogLevel::Error,
            );
            return 1;
        }

        let ps = pre_scan_file(
            &src_mnt,
            &dst_display,
            DstObjKind::DirExisting,
            true,
            preview_only,
            replace_dest_symlink,
            merge_collision_policy,
            Some(&destination_index),
        );
        planned_bytes = planned_bytes.saturating_add(ps.planned_bytes);
        total_regular_files =
            total_regular_files.saturating_add(ps.total_regular_files.unwrap_or(0));
        add_files = add_files.saturating_add(ps.add_files);
        mod_files = mod_files.saturating_add(ps.mod_files);
        display_change_preview.extend(ps.change_preview);
        source_top_entries.insert(src_name.clone());
        file_relation_breakdown.add_assign(ps.file_relation_breakdown);
        if ps.has_itemized_changes {
            has_itemized_changes = true;
        }
        source_rel_files.insert(src_name.clone());
        batch_items.push((src_mnt, ps.planned_bytes, ps.has_itemized_changes));
    }

    let uncollided_files = destination_index
        .entries
        .iter()
        .filter(|(rel, entry)| {
            entry.kind == DestinationKind::Regular && !source_rel_files.contains(*rel)
        })
        .count() as u64;

    let preview_root_path = Path::new(&dst_display);
    print_preview_root_line(preview_root_path, false, true);
    let empty_extra: HashSet<String> = HashSet::new();
    print_changed_top_preview(
        preview_root_path,
        &display_change_preview,
        &source_top_entries,
        showall,
        &empty_extra,
        &empty_extra,
        &empty_extra,
        &empty_extra,
        tree_trunc,
    );

    println!();
    let file_row = Some((
        add_files,
        mod_files,
        total_regular_files.saturating_sub(add_files.saturating_add(mod_files)),
        uncollided_files,
        if is_move { total_regular_files } else { 0 },
        0,
    ));
    let dir_row = Some((0, 0, 0, 0, 0, 0));
    if preview_only {
        print_preview_counts_table(file_row, dir_row, file_relation_breakdown);
    } else {
        print_counts_table(file_row, dir_row);
    }

    println!();
    println!(
        "Planned transfer bytes: {} ({})",
        format_number(planned_bytes),
        format_bytes_binary(planned_bytes, 2)
    );
    println!();

    if preview_only {
        return 0;
    }

    let no_changes_planned = planned_bytes == 0 && !has_itemized_changes;
    if no_changes_planned && !is_move {
        log(
            requested_mode,
            &format!("No changes detected; nothing to {}.", requested_mode.word()),
            LogLevel::Info,
        );
        return 0;
    }

    print!("Proceed with {}? [Y/n]: ", requested_mode.word());
    let _ = io::stdout().flush();
    let mut ans = String::new();
    if io::stdin().read_line(&mut ans).is_err() {
        return 1;
    }
    let ans = ans.trim().to_lowercase();
    if ans != "y" && ans != "yes" && !ans.is_empty() {
        return 1;
    }

    if no_changes_planned && is_move {
        log(
            requested_mode,
            "Destination already has matching files; source files will be removed to complete move.",
            LogLevel::Info,
        );
        log(requested_mode, "Starting cleanup", LogLevel::Info);
        let start_ts = Instant::now();
        let cleanup_start = Instant::now();
        let mut removed = DeleteCleanupOutcome::default();
        let mut flush_targets: HashSet<PathBuf> = HashSet::new();

        for (src_mnt, _, _) in &batch_items {
            let src = src_mnt.as_path();
            let src_lmd = match fs::symlink_metadata(src) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let src_name = match src.file_name() {
                Some(v) => v,
                None => continue,
            };
            let dst = dst_mnt.join(src_name);
            let same = if src_lmd.file_type().is_symlink() {
                symlink_targets_equal(src, &dst)
            } else {
                let src_md = match fs::metadata(src) {
                    Ok(v) if v.is_file() => v,
                    _ => continue,
                };
                match fs::metadata(&dst) {
                    Ok(v) => v.is_file() && v.len() == src_md.len(),
                    Err(_) => false,
                }
            };
            if same && remove_single_file(src, use_sudo, requested_mode) {
                removed.files = removed.files.saturating_add(1);
                removed.bytes = removed
                    .bytes
                    .saturating_add(if src_lmd.file_type().is_symlink() {
                        0
                    } else {
                        src_lmd.len()
                    });
                if let Some(parent) = src.parent() {
                    flush_targets.insert(realpath_allow_missing(parent));
                }
            }
        }
        let cleanup_elapsed_s = cleanup_start.elapsed().as_secs_f64();
        let mut cleanup_flush_elapsed_s = 0.0f64;
        let mut cleanup_flush_bytes_total: u64 = 0;
        for target in flush_targets {
            let flush = flush_source_cleanup_writes(&target, use_sudo, requested_mode);
            cleanup_flush_elapsed_s += flush.elapsed_s;
            if let Some(b) = flush.flushed_bytes {
                cleanup_flush_bytes_total = cleanup_flush_bytes_total.saturating_add(b);
            }
        }
        log(requested_mode, "Cleanup complete.", LogLevel::Info);
        log_transfer_complete(requested_mode);
        let cleanup_bps = if cleanup_elapsed_s > 0.0 {
            removed.bytes as f64 / cleanup_elapsed_s.max(1e-6)
        } else {
            0.0
        };
        let cleanup_flush_bps = if cleanup_flush_elapsed_s > 0.0 {
            cleanup_flush_bytes_total as f64 / cleanup_flush_elapsed_s.max(1e-6)
        } else {
            0.0
        };
        print_copy_duration_summary(
            0.0,
            0.0,
            0.0,
            0.0,
            Some((
                cleanup_elapsed_s,
                cleanup_bps,
                cleanup_flush_elapsed_s,
                cleanup_flush_bps,
            )),
            start_ts.elapsed().as_secs_f64(),
        );
        return 0;
    }

    if is_move && !use_sudo {
        let mut rename_plan: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(batch_items.len());
        let mut can_batch_fast_rename = true;
        for (src_mnt, _, _) in &batch_items {
            let src_name = match src_mnt.file_name() {
                Some(v) => v,
                None => {
                    can_batch_fast_rename = false;
                    break;
                }
            };
            let rename_target = dst_mnt.join(src_name);
            if !can_fast_rename_same_fs(src_mnt, &rename_target)
                || rename_target.exists()
                || rename_target == *src_mnt
            {
                can_batch_fast_rename = false;
                break;
            }
            rename_plan.push((src_mnt.clone(), rename_target));
        }
        if can_batch_fast_rename {
            for (src_mnt, rename_target) in &rename_plan {
                if fs::rename(src_mnt, rename_target).is_err() {
                    can_batch_fast_rename = false;
                    break;
                }
            }
            if can_batch_fast_rename {
                let _ = fsync_directory(&dst_mnt);
                let mut source_parents = HashSet::new();
                for (src, _) in &rename_plan {
                    if let Some(parent) = src.parent() {
                        source_parents.insert(parent.to_path_buf());
                    }
                }
                for parent in source_parents {
                    if parent != dst_mnt {
                        let _ = fsync_directory(&parent);
                    }
                }
                log(
                    requested_mode,
                    &format!(
                        "Fast-path rename on same filesystem (batch): moved {} files into {}",
                        rename_plan.len(),
                        dst_mnt.display()
                    ),
                    LogLevel::Info,
                );
                println!();
                log_transfer_complete(requested_mode);
                return 0;
            }
        }
    }

    let backend = if use_sudo {
        TransferBackend::Rsync
    } else {
        TransferBackend::Rust
    };
    let start_ts = Instant::now();
    let transferred_bytes_total: u64;
    let transferred_elapsed_total_s: f64;
    let mut transfer_flush_elapsed_s: f64 = 0.0;
    let mut transfer_flush_bytes_total: u64 = 0;
    let mut cleanup_elapsed_total_s: f64 = 0.0;
    let mut cleanup_flush_elapsed_s: f64 = 0.0;
    let mut cleanup_flush_bytes_total: u64 = 0;
    let mut deleted_cleanup_total = DeleteCleanupOutcome::default();
    let batch_progress_snapshot: Option<ProgressSnapshot>;
    let completed_batch_items: Vec<(PathBuf, u64)>;

    if matches!(backend, TransferBackend::Rust) {
        let destination_media = dev_media_kind(&dst_mnt);
        let media = batch_items
            .iter()
            .fold(destination_media, |current, (source, _, _)| {
                transfer_media_kind(current, dev_media_kind(source))
            });
        configure_rayon_threads_for_media(media);
        let transfer = run_rust_file_batch(
            &batch_items,
            &dst_mnt,
            planned_bytes,
            media,
            replace_dest_symlink,
            requested_mode,
        );
        transferred_bytes_total = transfer.bytes_done;
        transferred_elapsed_total_s = transfer.elapsed_s;
        if transfer.rc != 0 {
            log(
                requested_mode,
                &format!(
                    "{} failed: transfer exited with status {}.",
                    requested_mode.word_cap(),
                    transfer.rc
                ),
                LogLevel::Error,
            );
            return 1;
        }
        batch_progress_snapshot = transfer.progress_snapshot;
        completed_batch_items = batch_items
            .into_iter()
            .map(|(source, bytes, _)| (source, bytes))
            .collect();
    } else {
        let source_args: Vec<String> = batch_items
            .iter()
            .map(|(source, _, _)| source.display().to_string())
            .collect();
        let transfer = run_rsync_transfer_sources(
            &source_args,
            &dst_display,
            planned_bytes,
            use_sudo,
            false,
            false,
            true,
        );
        transferred_bytes_total = transfer.bytes_done;
        transferred_elapsed_total_s = transfer.elapsed_s;
        if transfer.rc != 0 {
            log(
                requested_mode,
                &format!(
                    "{} failed: transfer exited with status {}.",
                    requested_mode.word_cap(),
                    transfer.rc
                ),
                LogLevel::Error,
            );
            return 1;
        }
        batch_progress_snapshot = transfer.progress_snapshot;
        completed_batch_items = batch_items
            .into_iter()
            .map(|(source, bytes, _)| (source, bytes))
            .collect();
    }

    let flush_stats =
        flush_destination_writes(&dst_mnt, use_sudo, requested_mode, batch_progress_snapshot);
    transfer_flush_elapsed_s += flush_stats.elapsed_s;
    if let Some(bytes) = flush_stats.flushed_bytes {
        transfer_flush_bytes_total = transfer_flush_bytes_total.saturating_add(bytes);
    }

    if is_move {
        for (src_mnt, item_planned_bytes) in completed_batch_items {
            let src_path = src_mnt.display().to_string();
            let cleanup = run_move_cleanup_phase(
                &src_path,
                &dst_display,
                &src_mnt,
                SrcObjKind::File,
                false,
                false,
                None,
                false,
                use_sudo,
                requested_mode,
                None,
                1,
                item_planned_bytes,
            );
            deleted_cleanup_total.files = deleted_cleanup_total
                .files
                .saturating_add(cleanup.deleted.files);
            deleted_cleanup_total.bytes = deleted_cleanup_total
                .bytes
                .saturating_add(cleanup.deleted.bytes);
            cleanup_elapsed_total_s += cleanup.cleanup_elapsed_s;
            cleanup_flush_elapsed_s += cleanup.flush.elapsed_s;
            if let Some(b) = cleanup.flush.flushed_bytes {
                cleanup_flush_bytes_total = cleanup_flush_bytes_total.saturating_add(b);
            }
        }
    }

    log_transfer_complete(requested_mode);

    let total_elapsed_s = start_ts.elapsed().as_secs_f64();
    let avg_transfer_bps = if transferred_elapsed_total_s > 0.0 {
        transferred_bytes_total as f64 / transferred_elapsed_total_s
    } else {
        0.0
    };
    let transfer_flush_bps = if transfer_flush_elapsed_s > 0.0 {
        transfer_flush_bytes_total as f64 / transfer_flush_elapsed_s
    } else {
        0.0
    };
    let cleanup_summary = if is_move {
        let cleanup_bps = if cleanup_elapsed_total_s > 0.0 {
            deleted_cleanup_total.bytes as f64 / cleanup_elapsed_total_s.max(1e-6)
        } else {
            0.0
        };
        let cleanup_flush_bps = if cleanup_flush_elapsed_s > 0.0 {
            cleanup_flush_bytes_total as f64 / cleanup_flush_elapsed_s.max(1e-6)
        } else {
            0.0
        };
        Some((
            cleanup_elapsed_total_s,
            cleanup_bps,
            cleanup_flush_elapsed_s,
            cleanup_flush_bps,
        ))
    } else {
        None
    };
    print_copy_duration_summary(
        transferred_elapsed_total_s,
        avg_transfer_bps,
        transfer_flush_elapsed_s,
        transfer_flush_bps,
        cleanup_summary,
        total_elapsed_s,
    );
    0
}

pub(crate) fn fsync_directory(path: &Path) -> io::Result<()> {
    let directory = File::open(path)?;
    directory.sync_all()
}

#[allow(dead_code)]
pub(crate) fn flush_rename_metadata(src: &Path, dst: &Path) -> io::Result<()> {
    let src_parent = src.parent().unwrap_or_else(|| Path::new("/"));
    let dst_parent = dst.parent().unwrap_or_else(|| Path::new("/"));
    fsync_directory(dst_parent)?;
    if src_parent != dst_parent {
        fsync_directory(src_parent)?;
    }
    Ok(())
}

pub(crate) fn run_flush_command(target: &Path, use_sudo: bool) -> bool {
    if use_sudo {
        let cmd = vec![
            "sync".to_string(),
            "-f".to_string(),
            target.display().to_string(),
        ];
        return run_command_capture(&cmd, true)
            .map(|o| o.code == 0)
            .unwrap_or(false);
    }
    let file = match fs::File::open(target) {
        Ok(file) => file,
        Err(_) => return false,
    };
    unsafe { nix::libc::syncfs(file.as_raw_fd()) == 0 }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct FlushStats {
    pub(crate) elapsed_s: f64,
    pub(crate) flushed_bytes: Option<u64>,
    pub(crate) ok: bool,
}

pub(crate) fn flush_destination_writes(
    dst_path: &Path,
    use_sudo: bool,
    mode: TransferMode,
    progress: Option<ProgressSnapshot>,
) -> FlushStats {
    let target = if dst_path.exists() {
        if dst_path.is_dir() {
            dst_path.to_path_buf()
        } else {
            dst_path
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf()
        }
    } else {
        dst_path
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf()
    };

    let target_key = target.display().to_string();
    let flush_device_window = DeviceIoWindow::from_transfer_paths(&target_key, &target_key);
    let flush_device_start = flush_device_window.current_totals();
    let flush_write_start = flush_device_start.1;
    let flush_start = Instant::now();
    let proc_start = read_proc_io_counters(std::process::id());
    let (flush_tx, flush_rx) = mpsc::channel::<bool>();
    let target_for_flush = target.clone();
    let flush_thread = thread::spawn(move || {
        let ok = run_flush_command(&target_for_flush, use_sudo);
        let _ = flush_tx.send(ok);
    });

    let mut flush_ok = false;
    let mut last_proc_sample = proc_start;
    let mut last_device_sample = flush_device_start;
    let mut last_sample_at = flush_start;
    let mut last_progress_rates = progress.as_ref().map(|p| p.rates).unwrap_or_default();

    loop {
        match flush_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ok) => {
                flush_ok = ok;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(snapshot) = progress.as_ref() {
                    let now = Instant::now();
                    let proc_now = read_proc_io_counters(std::process::id());
                    let proc_extra = proc_io_deltas(proc_start, proc_now);
                    let device_now = flush_device_window.current_totals();
                    let device_extra = device_io_deltas(flush_device_start, device_now);
                    if let (Some(cur), Some(prev)) = (proc_now, last_proc_sample) {
                        let dt = now.duration_since(last_sample_at).as_secs_f64().max(1e-6);
                        last_progress_rates.rchar_bps =
                            Some(cur.rchar.saturating_sub(prev.rchar) as f64 / dt);
                        last_progress_rates.wchar_bps =
                            Some(cur.wchar.saturating_sub(prev.wchar) as f64 / dt);
                        last_progress_rates.read_bytes_bps =
                            Some(cur.read_bytes.saturating_sub(prev.read_bytes) as f64 / dt);
                        last_progress_rates.write_bytes_bps =
                            Some(cur.write_bytes.saturating_sub(prev.write_bytes) as f64 / dt);
                        last_progress_rates.read_complete_bps =
                            counter_delta(last_device_sample.0, device_now.0)
                                .map(|v| v as f64 / dt);
                        last_progress_rates.write_complete_bps =
                            counter_delta(last_device_sample.1, device_now.1)
                                .map(|v| v as f64 / dt);
                        last_proc_sample = proc_now;
                        last_device_sample = device_now;
                        last_sample_at = now;
                    }
                    let merged_proc_deltas = ProcIoDeltas {
                        rchar: option_u64_saturating_add(
                            snapshot.proc_deltas.rchar,
                            proc_extra.rchar,
                        ),
                        wchar: option_u64_saturating_add(
                            snapshot.proc_deltas.wchar,
                            proc_extra.wchar,
                        ),
                        read_bytes: option_u64_saturating_add(
                            snapshot.proc_deltas.read_bytes,
                            proc_extra.read_bytes,
                        ),
                        write_bytes: option_u64_saturating_add(
                            snapshot.proc_deltas.write_bytes,
                            proc_extra.write_bytes,
                        ),
                    };
                    let merged_device_deltas = DeviceIoDeltas {
                        read_complete: option_u64_saturating_add(
                            snapshot.device_deltas.read_complete,
                            device_extra.read_complete,
                        ),
                        write_complete: option_u64_saturating_add(
                            snapshot.device_deltas.write_complete,
                            device_extra.write_complete,
                        ),
                    };
                    let mut display_rates = snapshot.rates;
                    display_rates.rchar_bps =
                        last_progress_rates.rchar_bps.or(display_rates.rchar_bps);
                    display_rates.wchar_bps =
                        last_progress_rates.wchar_bps.or(display_rates.wchar_bps);
                    display_rates.read_bytes_bps = last_progress_rates
                        .read_bytes_bps
                        .or(display_rates.read_bytes_bps);
                    display_rates.write_bytes_bps = last_progress_rates
                        .write_bytes_bps
                        .or(display_rates.write_bytes_bps);
                    display_rates.read_complete_bps = last_progress_rates
                        .read_complete_bps
                        .or(display_rates.read_complete_bps);
                    display_rates.write_complete_bps = last_progress_rates
                        .write_complete_bps
                        .or(display_rates.write_complete_bps);
                    print_transfer_progress_bars(
                        snapshot.elapsed_s + now.duration_since(flush_start).as_secs_f64(),
                        snapshot.planned_bytes,
                        snapshot.write_all_total,
                        snapshot.phase_label,
                        display_rates,
                        merged_proc_deltas,
                        merged_device_deltas,
                        snapshot.eta_workload.clone(),
                        snapshot.eta_progress,
                        None,
                        false,
                        true,
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
    let _ = flush_thread.join();

    let flush_elapsed_s = flush_start.elapsed().as_secs_f64();
    let flush_device_end = flush_device_window.current_totals();
    let flush_write_end = flush_device_end.1;

    if let Some(snapshot) = progress {
        let proc_end = read_proc_io_counters(std::process::id());
        let proc_extra = proc_io_deltas(proc_start, proc_end);
        let device_extra = device_io_deltas(flush_device_start, flush_device_end);
        let merged_proc_deltas = ProcIoDeltas {
            rchar: option_u64_saturating_add(snapshot.proc_deltas.rchar, proc_extra.rchar),
            wchar: option_u64_saturating_add(snapshot.proc_deltas.wchar, proc_extra.wchar),
            read_bytes: option_u64_saturating_add(
                snapshot.proc_deltas.read_bytes,
                proc_extra.read_bytes,
            ),
            write_bytes: option_u64_saturating_add(
                snapshot.proc_deltas.write_bytes,
                proc_extra.write_bytes,
            ),
        };
        let merged_device_deltas = DeviceIoDeltas {
            read_complete: option_u64_saturating_add(
                snapshot.device_deltas.read_complete,
                device_extra.read_complete,
            ),
            write_complete: option_u64_saturating_add(
                snapshot.device_deltas.write_complete,
                device_extra.write_complete,
            ),
        };
        let mut display_rates = snapshot.rates;
        display_rates.rchar_bps = last_progress_rates.rchar_bps.or(display_rates.rchar_bps);
        display_rates.wchar_bps = last_progress_rates.wchar_bps.or(display_rates.wchar_bps);
        display_rates.read_bytes_bps = last_progress_rates
            .read_bytes_bps
            .or(display_rates.read_bytes_bps);
        display_rates.write_bytes_bps = last_progress_rates
            .write_bytes_bps
            .or(display_rates.write_bytes_bps);
        display_rates.read_complete_bps = last_progress_rates
            .read_complete_bps
            .or(display_rates.read_complete_bps);
        display_rates.write_complete_bps = last_progress_rates
            .write_complete_bps
            .or(display_rates.write_complete_bps);
        print_transfer_progress_bars(
            snapshot.elapsed_s + flush_elapsed_s,
            snapshot.planned_bytes,
            snapshot.write_all_total,
            snapshot.phase_label,
            display_rates,
            merged_proc_deltas,
            merged_device_deltas,
            snapshot.eta_workload,
            snapshot.eta_progress,
            None,
            true,
            true,
        );
    }
    let flushed_bytes = counter_delta(flush_write_start, flush_write_end);
    if flushed_bytes.is_none() && !flush_ok {
        log(
            mode,
            "Flush phase could not be measured via disk counters.",
            LogLevel::Warn,
        );
    }
    FlushStats {
        elapsed_s: flush_elapsed_s,
        flushed_bytes,
        ok: flush_ok,
    }
}
pub(crate) fn flush_source_cleanup_writes(
    src_path: &Path,
    use_sudo: bool,
    mode: TransferMode,
) -> FlushStats {
    let target = if src_path.exists() {
        if src_path.is_dir() {
            src_path.to_path_buf()
        } else {
            src_path
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf()
        }
    } else {
        src_path
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf()
    };
    let target_key = target.display().to_string();
    let device_window = DeviceIoWindow::from_transfer_paths(&target_key, &target_key);
    let start_totals = device_window.current_totals();
    let start = Instant::now();
    let ok = run_flush_command(&target, use_sudo);
    let elapsed_s = start.elapsed().as_secs_f64();
    let end_totals = device_window.current_totals();
    let flushed_bytes = counter_delta(start_totals.1, end_totals.1);
    if !ok {
        log(
            mode,
            "Source cleanup flush may be incomplete.",
            LogLevel::Warn,
        );
    }
    FlushStats {
        elapsed_s,
        flushed_bytes,
        ok,
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct CleanupPhaseStats {
    pub(crate) deleted: DeleteCleanupOutcome,
    pub(crate) cleanup_elapsed_s: f64,
    pub(crate) flush: FlushStats,
    pub(crate) success: bool,
}

pub(crate) struct SyncCleanupPhaseResult {
    pub(crate) stats: CleanupPhaseStats,
    pub(crate) success: bool,
}

fn render_cleanup_phase_completion(
    total_elapsed_s: f64,
    cleanup_elapsed_s: f64,
    deleted: DeleteCleanupOutcome,
    expected_bytes: u64,
    proc_start: Option<ProcIoCounters>,
    device_window: &DeviceIoWindow,
    device_start: (Option<u64>, Option<u64>),
    flush: FlushStats,
    completed: bool,
) {
    let proc_end = read_proc_io_counters(std::process::id());
    let proc_delta = proc_io_deltas(proc_start, proc_end);
    let device_end = device_window.current_totals();
    let device_delta = device_io_deltas(device_start, device_end);

    let mut rates = TransferProgressRates::default();
    if cleanup_elapsed_s > 0.0 {
        rates.write_all_bps = Some(deleted.bytes as f64 / cleanup_elapsed_s.max(1e-6));
    }
    rates.rchar_bps = proc_delta.rchar.map(|v| v as f64 / total_elapsed_s);
    rates.wchar_bps = proc_delta.wchar.map(|v| v as f64 / total_elapsed_s);
    rates.read_bytes_bps = proc_delta.read_bytes.map(|v| v as f64 / total_elapsed_s);
    rates.write_bytes_bps = proc_delta.write_bytes.map(|v| v as f64 / total_elapsed_s);
    rates.read_complete_bps = device_delta
        .read_complete
        .map(|v| v as f64 / total_elapsed_s);
    rates.write_complete_bps = if flush.elapsed_s > 0.0 {
        flush
            .flushed_bytes
            .map(|v| v as f64 / flush.elapsed_s.max(1e-6))
            .or_else(|| {
                device_delta
                    .write_complete
                    .map(|v| v as f64 / total_elapsed_s)
            })
    } else {
        device_delta
            .write_complete
            .map(|v| v as f64 / total_elapsed_s)
    };

    let delete_total = deleted.bytes.max(expected_bytes);
    let delete_done = if completed {
        delete_total
    } else {
        deleted.bytes.min(delete_total)
    };
    let mut eta_estimator = TransferEtaEstimator::default();
    print_transfer_progress_bars(
        total_elapsed_s,
        delete_total,
        Some(delete_done),
        "Delete",
        rates,
        proc_delta,
        device_delta,
        None,
        None,
        Some(&mut eta_estimator),
        true,
        true,
    );
}

pub(crate) fn run_move_cleanup_phase(
    src_path: &str,
    dst_path: &str,
    src_mnt: &Path,
    src_obj_kind: SrcObjKind,
    contents_mode_requested: bool,
    source_contents_mode: bool,
    exclude_rel: Option<&str>,
    rename_dir_to_new_path: bool,
    use_sudo: bool,
    mode: TransferMode,
    transfer_manifest: Option<&TransferManifest>,
    expected_files: u64,
    expected_bytes: u64,
) -> CleanupPhaseStats {
    log(mode, "Starting cleanup", LogLevel::Info);
    println!();
    reset_progress_render_state();

    let pid = std::process::id();
    let proc_start = read_proc_io_counters(pid);
    let device_window = DeviceIoWindow::from_transfer_paths(src_path, src_path);
    let device_start = device_window.current_totals();

    let cleanup_start = Instant::now();
    let deleted = prune_move_source_duplicates(
        src_path,
        dst_path,
        src_obj_kind,
        contents_mode_requested && src_obj_kind == SrcObjKind::Dir,
        exclude_rel,
        use_sudo,
        mode,
        transfer_manifest,
        expected_files,
        expected_bytes,
    );
    let mut directory_cleanup_ok = true;
    if src_obj_kind == SrcObjKind::Dir {
        let remove_root = (!source_contents_mode) || rename_dir_to_new_path;
        if !use_sudo {
            if let Some(manifest) = transfer_manifest {
                directory_cleanup_ok =
                    cleanup_source_dirs_from_manifest(src_mnt, manifest, remove_root, exclude_rel);
            } else {
                directory_cleanup_ok = cleanup_source_dirs(src_mnt, remove_root, false, mode);
            }
        } else {
            directory_cleanup_ok = cleanup_source_dirs(src_mnt, remove_root, true, mode);
        }
    }
    let cleanup_elapsed_s = cleanup_start.elapsed().as_secs_f64();

    let flush = flush_source_cleanup_writes(src_mnt, use_sudo, mode);
    let total_elapsed_s = (cleanup_elapsed_s + flush.elapsed_s).max(1e-6);
    render_cleanup_phase_completion(
        total_elapsed_s,
        cleanup_elapsed_s,
        deleted,
        expected_bytes,
        proc_start,
        &device_window,
        device_start,
        flush,
        deleted.success && directory_cleanup_ok && flush.ok,
    );

    let success = deleted.success && directory_cleanup_ok && flush.ok;
    if success {
        log(mode, "Cleanup complete.", LogLevel::Info);
    } else {
        log(
            mode,
            "Cleanup completed but source flush failed; source durability is unconfirmed.",
            LogLevel::Error,
        );
    }
    CleanupPhaseStats {
        deleted,
        cleanup_elapsed_s,
        flush,
        success,
    }
}

pub(crate) fn run_sync_cleanup_phase(
    src_path: &str,
    dst_path: &str,
    mode: TransferMode,
    manifest: &TransferManifest,
) -> SyncCleanupPhaseResult {
    log(mode, "Starting sync cleanup", LogLevel::Info);
    println!();
    reset_progress_render_state();

    let src_root = Path::new(src_path.trim_end_matches('/'));
    let dst_base = Path::new(dst_path.trim_end_matches('/'));
    let include_root = !src_path.ends_with('/');
    let src_base = src_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let destination_root = if include_root {
        dst_base.join(&src_base)
    } else {
        dst_base.to_path_buf()
    };
    let expected_bytes = manifest
        .sync_delete_files
        .iter()
        .map(|entry| entry.size)
        .sum();

    let proc_start = read_proc_io_counters(std::process::id());
    let destination_key = destination_root.display().to_string();
    let device_window = DeviceIoWindow::from_transfer_paths(&destination_key, &destination_key);
    let device_start = device_window.current_totals();
    let cleanup_start = Instant::now();
    let deletion = delete_sync_destination_extras(&destination_root, manifest);
    let cleanup_elapsed_s = cleanup_start.elapsed().as_secs_f64();
    let (deleted, cleanup_error) = match deletion {
        Ok(deleted) => (deleted, None),
        Err(err) => (DeleteCleanupOutcome::default(), Some(err)),
    };
    let deletion_ok = cleanup_error.is_none();

    let metadata_ok = preserve_directory_times_tree(
        src_root,
        dst_base,
        include_root,
        &src_base,
        Some(&manifest.dir_times),
    )
    .is_ok();
    let flush = flush_source_cleanup_writes(&destination_root, false, mode);
    let total_elapsed_s = (cleanup_elapsed_s + flush.elapsed_s).max(1e-6);
    render_cleanup_phase_completion(
        total_elapsed_s,
        cleanup_elapsed_s,
        deleted,
        expected_bytes,
        proc_start,
        &device_window,
        device_start,
        flush,
        deletion_ok && flush.ok,
    );

    let success = deletion_ok && metadata_ok && flush.ok;
    if success {
        log(mode, "Sync cleanup complete.", LogLevel::Info);
    } else {
        log(
            mode,
            &format!(
                "Sync cleanup failed; destination-only entries may remain: {}",
                cleanup_error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown cleanup error".to_string())
            ),
            LogLevel::Error,
        );
    }
    SyncCleanupPhaseResult {
        stats: CleanupPhaseStats {
            deleted,
            cleanup_elapsed_s,
            flush,
            success,
        },
        success,
    }
}
