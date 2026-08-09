//! Progress rendering, rate formatting, and regime-aware ETA estimation.
//!
//! This module owns the presentation state and the statistical model used by
//! local and rsync-backed transfers. It does not perform filesystem I/O.

use crate::domain::{
    DeviceIoDeltas, EtaProgressTotals, EtaWorkload, ProcIoDeltas, TransferProgressRates,
};
use crate::output::eta::{EtaTelemetry, TransferEtaEstimator};
use crate::output::summary::format_number;
use crate::runtime::option_u64_saturating_sub;
use std::env;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::{Mutex, OnceLock};

pub(crate) fn fmt_hms_ms(total_seconds: f64) -> String {
    let ms_total = (total_seconds.max(0.0) * 1000.0).round() as i64;
    let h = ms_total / 3_600_000;
    let m = (ms_total % 3_600_000) / 60_000;
    let s = (ms_total % 60_000) / 1000;
    let ms = ms_total % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

pub(crate) fn fmt_hms_tenths(total_seconds: f64) -> String {
    let tenth_total = (total_seconds.max(0.0) * 10.0).round() as i64;
    let h = tenth_total / 36_000;
    let m = (tenth_total % 36_000) / 600;
    let s = (tenth_total % 600) / 10;
    let t = tenth_total % 10;
    format!("{h:02}:{m:02}:{s:02}.{t}")
}

pub(crate) fn format_bytes_binary(byte_value: u64, decimals: usize) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut idx = 0usize;
    let mut value = byte_value as f64;
    while idx < units.len() - 1 && value >= 1024.0 {
        value /= 1024.0;
        idx += 1;
    }
    while idx < units.len() - 1
        && (value * 10f64.powi(decimals as i32)).round() / 10f64.powi(decimals as i32) >= 1024.0
    {
        value /= 1024.0;
        idx += 1;
    }
    if units[idx] == "B" {
        format!("{} B", value as u64)
    } else {
        format!("{value:.decimals$} {}", units[idx])
    }
}

pub(crate) fn fmt_speed_bps(bps: f64) -> String {
    let b = if bps.is_finite() && bps > 0.0 {
        bps as u64
    } else {
        0
    };
    format_bytes_binary(b, 2)
}

pub(crate) fn print_transfer_columns_header() {
    reset_progress_render_state();
    // Intentionally empty: live progress block prints its own structure.
}

pub(crate) fn terminal_columns() -> usize {
    // Terminal geometry can change while a transfer is running. Caching the
    // first query makes a resize leave stale-width frames and cursor movement.
    query_terminal_columns()
}

pub(crate) fn query_terminal_columns() -> usize {
    fn tty_columns_from_fd(fd: i32) -> Option<usize> {
        let mut ws = nix::libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { nix::libc::ioctl(fd, nix::libc::TIOCGWINSZ, &mut ws) };
        if rc == 0 && ws.ws_col >= 40 {
            Some(ws.ws_col as usize)
        } else {
            None
        }
    }

    tty_columns_from_fd(io::stdout().as_raw_fd())
        .or_else(|| tty_columns_from_fd(io::stderr().as_raw_fd()))
        .or_else(|| {
            env::var("COLUMNS")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|v| *v >= 40)
        })
        .unwrap_or(120)
}

pub(crate) fn stdout_is_tty() -> bool {
    unsafe { nix::libc::isatty(io::stdout().as_raw_fd()) == 1 }
}

pub(crate) fn fmt_bytes_block_opt(byte_value: Option<u64>, decimals: usize) -> String {
    match byte_value {
        Some(v) => format_bytes_binary(v, decimals),
        None => "--".to_string(),
    }
}

pub(crate) fn fmt_rate_block_opt(bps: Option<f64>) -> String {
    match bps {
        Some(v) if v.is_finite() && v >= 0.0 => {
            let mut value = v;
            let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
            let mut idx = 0usize;
            while value >= 1024.0 && idx + 1 < units.len() {
                value /= 1024.0;
                idx += 1;
            }
            let number = if value >= 100.0 {
                format!("{value:.1}")
            } else if value >= 10.0 {
                format!("{value:.2}")
            } else {
                format!("{value:.3}")
            };
            format!("{number} {}/s", units[idx])
        }
        _ => "--/s".to_string(),
    }
}

pub(crate) fn build_progress_bar(pct: Option<f64>, width: usize) -> String {
    let w = width.max(8);
    match pct {
        Some(v) => {
            let clamped = v.clamp(0.0, 100.0);
            if clamped >= 100.0 {
                return "=".repeat(w);
            }
            let filled = ((clamped / 100.0) * (w as f64)).floor() as usize;
            if filled == 0 {
                format!(">{}", " ".repeat(w.saturating_sub(1)))
            } else if filled >= w {
                "=".repeat(w)
            } else {
                format!("{}>{}", "=".repeat(filled), " ".repeat(w - filled - 1))
            }
        }
        None => " ".repeat(w),
    }
}

pub(crate) fn clamp_line_for_columns(mut line: String, terminal_width: usize) -> String {
    let max_cols = terminal_width.saturating_sub(1).max(40);
    if line.chars().count() > max_cols {
        line = line.chars().take(max_cols).collect();
    }
    line
}

#[derive(Default)]

struct ProgressRenderState {
    active: bool,
    lines: usize,
    finalized_lines: usize,
    terminal_width: usize,
    last_frame: String,
}

fn progress_render_state() -> &'static Mutex<ProgressRenderState> {
    static STATE: OnceLock<Mutex<ProgressRenderState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ProgressRenderState::default()))
}

fn visual_rows_for_frame(frame: &str, terminal_width: usize) -> usize {
    let width = terminal_width.max(1);
    frame
        .split('\n')
        .map(|line| {
            let columns = line.chars().count().max(1);
            columns.div_ceil(width)
        })
        .sum()
}

pub(crate) fn eta_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env::var_os("COPY_RS_ETA_DEBUG").is_some())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn print_transfer_progress_bars(
    elapsed_s: f64,
    planned_bytes: u64,
    write_all_total: Option<u64>,
    phase_label: &str,
    rates: TransferProgressRates,
    proc_totals_delta: ProcIoDeltas,
    device_totals_delta: DeviceIoDeltas,
    eta_workload: Option<EtaWorkload>,
    eta_progress: Option<EtaProgressTotals>,
    mut eta_estimator: Option<&mut TransferEtaEstimator>,
    finalize_line: bool,
    flushing: bool,
) {
    const BAR_WIDTH: usize = 34;
    const PROGRESS_LABEL_WIDTH: usize = 8;
    const LABEL_COL_WIDTH: usize = 13;
    const BYTES_COL_WIDTH: usize = 11;
    const RATE_COL_WIDTH: usize = 14;

    let done = write_all_total;
    let pct = if planned_bytes > 0 {
        done.map(|d| (d as f64 * 100.0 / planned_bytes as f64).clamp(0.0, 100.0))
    } else {
        None
    };
    let eta_telemetry = EtaTelemetry {
        write_bytes: proc_totals_delta.write_bytes,
        write_complete: device_totals_delta.write_complete,
        read_bytes: proc_totals_delta.read_bytes,
        read_complete: device_totals_delta.read_complete,
        aligned_pipeline_counters: false,
    };
    let mut eta_estimate = None;
    let eta = match eta_estimator.as_mut() {
        Some(estimator) => {
            let eta = estimator.update_with_telemetry(
                done,
                planned_bytes,
                rates.write_all_bps,
                elapsed_s,
                finalize_line,
                eta_workload.clone(),
                eta_progress,
                eta_telemetry,
                rates,
            );
            eta_estimate = estimator.last_estimate();
            eta
        }
        None if done.map(|value| value >= planned_bytes).unwrap_or(false) => Some(0.0),
        None => None,
    };

    let transfer_total = done;
    let transfer_rate = rates.write_all_bps;
    let write_disk_total = proc_totals_delta.write_bytes;
    let write_disk_rate = rates.write_bytes_bps;
    let write_complete_total = device_totals_delta.write_complete;
    let write_complete_rate = rates.write_complete_bps;
    let read_disk_total = proc_totals_delta.read_bytes;
    let read_disk_rate = rates.read_bytes_bps;
    let read_complete_total = device_totals_delta.read_complete;
    let read_complete_rate = rates.read_complete_bps;
    let read_cache_total =
        option_u64_saturating_sub(proc_totals_delta.rchar, proc_totals_delta.read_bytes);
    let read_cache_rate = read_cache_total.map(|total| {
        if elapsed_s <= 0.0 {
            0.0
        } else {
            total as f64 / elapsed_s.max(1e-6)
        }
    });

    let done_s = fmt_bytes_block_opt(transfer_total, 3);
    let planned_s = if planned_bytes > 0 {
        format_bytes_binary(planned_bytes, 3)
    } else {
        "--".to_string()
    };
    let eta_s = match eta {
        Some(v) => fmt_hms_tenths(v.max(0.0)),
        None => "--:--:--.-".to_string(),
    };
    let eta_debug = if eta_debug_enabled() {
        eta_estimate
            .map(|estimate| {
                let p10 = estimate
                    .p10_s
                    .map(fmt_hms_tenths)
                    .unwrap_or_else(|| "--:--:--.-".to_string());
                let p50 = estimate
                    .p50_s
                    .map(fmt_hms_tenths)
                    .unwrap_or_else(|| "--:--:--.-".to_string());
                let p90 = estimate
                    .p90_s
                    .map(fmt_hms_tenths)
                    .unwrap_or_else(|| "--:--:--.-".to_string());
                format!(
                    " [p10 {p10} p50 {p50} p90 {p90} {} {:.0}%]",
                    estimate.activity.label(),
                    estimate.confidence * 100.0
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let pct_s = match pct {
        Some(v) => format!("{v:>6.2}%"),
        None => "  ---%".to_string(),
    };
    let transfer_complete = pct.map(|p| p >= 100.0).unwrap_or(false);
    let terminal_width = terminal_columns();
    let clamp = |line| clamp_line_for_columns(line, terminal_width);
    let transfer_rate_display = if transfer_complete {
        format!("{:>width$}", "Complete", width = RATE_COL_WIDTH)
    } else {
        format!(
            "{:>width$}",
            fmt_rate_block_opt(transfer_rate),
            width = RATE_COL_WIDTH
        )
    };

    let file_progress = eta_workload
        .as_ref()
        .zip(eta_progress)
        .map(|(workload, progress)| {
            let total_files: u64 = workload.file_bins.iter().copied().sum();
            let completed_files = progress.file_count().min(total_files);
            (completed_files, total_files)
        });

    let transfer_progress_line = |bar_width: usize| {
        format!(
            "{:<label_width$} {pct_s} [{}] {done_s} / {planned_s}  {eta_s} eta{eta_debug}",
            phase_label,
            build_progress_bar(pct, bar_width),
            label_width = PROGRESS_LABEL_WIDTH
        )
    };
    let progress_line_width = |bar_width: usize| {
        let mut width = transfer_progress_line(bar_width).chars().count();
        if let Some((completed_files, total_files)) = file_progress {
            width = width.max(
                format_object_progress_line("Files", completed_files, total_files, bar_width)
                    .chars()
                    .count(),
            );
        }
        width
    };
    let max_progress_columns = terminal_width.saturating_sub(1).max(40);
    let mut bar_width = BAR_WIDTH;
    while progress_line_width(bar_width) > max_progress_columns && bar_width > 8 {
        let excess = progress_line_width(bar_width) - max_progress_columns;
        bar_width = bar_width.saturating_sub(excess).max(8);
    }

    let mut lines = vec![clamp(transfer_progress_line(bar_width))];
    if let Some((completed_files, total_files)) = file_progress {
        lines.push(clamp(format_object_progress_line(
            "Files",
            completed_files,
            total_files,
            bar_width,
        )));
    }
    lines.push(String::new());
    lines.extend([
        clamp(format!(
            "{:<label_width$}  {:>bytes_width$}   {:>rate_width$}",
            phase_label,
            fmt_bytes_block_opt(transfer_total, 3),
            transfer_rate_display,
            label_width = LABEL_COL_WIDTH,
            bytes_width = BYTES_COL_WIDTH,
            rate_width = RATE_COL_WIDTH
        )),
        clamp(format!(
            "{:<label_width$}  {:>bytes_width$}   {:>rate_width$}",
            "WriteDisk",
            fmt_bytes_block_opt(write_disk_total, 3),
            fmt_rate_block_opt(write_disk_rate),
            label_width = LABEL_COL_WIDTH,
            bytes_width = BYTES_COL_WIDTH,
            rate_width = RATE_COL_WIDTH
        )),
        clamp(format!(
            "{:<label_width$}  {:>bytes_width$}   {:>rate_width$}{}",
            "WriteComplete",
            fmt_bytes_block_opt(write_complete_total, 3),
            fmt_rate_block_opt(write_complete_rate),
            if flushing { " flushing" } else { "" },
            label_width = LABEL_COL_WIDTH,
            bytes_width = BYTES_COL_WIDTH,
            rate_width = RATE_COL_WIDTH
        )),
        clamp(format!(
            "{:<label_width$}  {:>bytes_width$}   {:>rate_width$}",
            "ReadCache",
            fmt_bytes_block_opt(read_cache_total, 3),
            fmt_rate_block_opt(read_cache_rate),
            label_width = LABEL_COL_WIDTH,
            bytes_width = BYTES_COL_WIDTH,
            rate_width = RATE_COL_WIDTH
        )),
        clamp(format!(
            "{:<label_width$}  {:>bytes_width$}   {:>rate_width$}",
            "ReadDisk",
            fmt_bytes_block_opt(read_disk_total, 3),
            fmt_rate_block_opt(read_disk_rate),
            label_width = LABEL_COL_WIDTH,
            bytes_width = BYTES_COL_WIDTH,
            rate_width = RATE_COL_WIDTH
        )),
        clamp(format!(
            "{:<label_width$}  {:>bytes_width$}   {:>rate_width$}",
            "ReadComplete",
            fmt_bytes_block_opt(read_complete_total, 3),
            fmt_rate_block_opt(read_complete_rate),
            label_width = LABEL_COL_WIDTH,
            bytes_width = BYTES_COL_WIDTH,
            rate_width = RATE_COL_WIDTH
        )),
    ]);

    if !stdout_is_tty() {
        if finalize_line {
            for line in &lines {
                println!("{line}");
            }
        }
        return;
    }

    if let Ok(mut state) = progress_render_state().lock() {
        let frame = lines.join("\n");
        if state.active && !finalize_line && state.last_frame == frame {
            return;
        }
        let terminal_width_changed =
            state.terminal_width != 0 && state.terminal_width != terminal_width;
        let previous_row_count = if terminal_width_changed {
            visual_rows_for_frame(&state.last_frame, state.terminal_width)
        } else if state.active {
            state.lines
        } else {
            state.finalized_lines
        };
        let previous_lines = if state.active {
            previous_row_count.saturating_sub(1)
        } else {
            previous_row_count
        };
        let mut output = String::with_capacity(frame.len() + lines.len() * 16 + 64);
        if previous_lines > 0 {
            let _ = write!(output, "\x1b[{}A\r", previous_lines);
        }

        // Keep each frame to one terminal row. Vertical cursor movement avoids
        // line-feed scrolling when the frame is rendered at the bottom edge.
        output.push_str("\x1b[?7l");
        for (idx, line) in lines.iter().enumerate() {
            let _ = write!(output, "\r\x1b[2K{line}");
            if idx + 1 < lines.len() {
                if previous_lines == 0 {
                    output.push('\n');
                } else {
                    output.push_str("\x1b[1B\r");
                }
            }
        }
        output.push_str("\x1b[?7h");
        let trailing_lines = previous_row_count.saturating_sub(lines.len());
        if trailing_lines > 0 {
            for _ in 0..trailing_lines {
                output.push_str("\x1b[1B\r\x1b[2K");
            }
            let _ = write!(output, "\x1b[{}A\r", trailing_lines);
        }
        if finalize_line {
            output.push('\n');
            state.active = false;
            state.lines = 0;
            state.finalized_lines = lines.len();
            state.terminal_width = terminal_width;
            state.last_frame = frame;
        } else {
            state.active = true;
            state.lines = lines.len();
            state.finalized_lines = 0;
            state.terminal_width = terminal_width;
            state.last_frame = frame;
        }
        let stdout = io::stdout();
        let mut locked = stdout.lock();
        let _ = locked.write_all(output.as_bytes());
        let _ = locked.flush();
    }
}

fn format_object_progress_line(
    label: &str,
    completed: u64,
    total: u64,
    bar_width: usize,
) -> String {
    const PROGRESS_LABEL_WIDTH: usize = 8;

    let completed = completed.min(total);
    let pct = if total > 0 {
        completed as f64 * 100.0 / total as f64
    } else {
        100.0
    };
    let pct_s = format!("{:>6.2}%", pct);
    format!(
        "{label:<label_width$} {pct_s} [{}] {} / {}",
        build_progress_bar(Some(pct), bar_width),
        format_number(completed),
        format_number(total),
        label_width = PROGRESS_LABEL_WIDTH,
    )
}

pub(crate) fn reset_progress_render_state() {
    finish_progress_render_state();
}

pub(crate) fn finish_progress_render_state() {
    if let Ok(mut state) = progress_render_state().lock() {
        if state.active {
            let mut output = String::new();
            let up = state.lines.saturating_sub(1);
            if up > 0 {
                let _ = write!(output, "\x1b[{up}A\r");
            }
            for idx in 0..state.lines {
                output.push_str("\r\x1b[2K");
                if idx + 1 < state.lines {
                    output.push_str("\x1b[1B\r");
                }
            }
            // Leave the cursor below the cleared frame so the next message
            // starts on a fresh line rather than overwriting its last row.
            output.push('\n');
            let stdout = io::stdout();
            let mut locked = stdout.lock();
            let _ = locked.write_all(output.as_bytes());
            let _ = locked.flush();
        }
        state.active = false;
        state.lines = 0;
        state.finalized_lines = 0;
        state.terminal_width = 0;
        state.last_frame.clear();
    }
}

pub(crate) fn print_summary_rate_line(label: &str, bps: f64, duration_s: f64, total: bool) {
    let total_suffix = if total { " (total)" } else { "" };
    let prefix = format!("{label}:");
    println!(
        "{prefix:<24}{}/s | Duration: {}{}",
        fmt_speed_bps(bps),
        fmt_hms_ms(duration_s),
        total_suffix
    );
}

pub(crate) fn print_copy_duration_summary(
    transfer_duration_s: f64,
    transfer_bps: f64,
    flush_duration_s: f64,
    flush_bps: f64,
    cleanup: Option<(f64, f64, f64, f64)>,
    total_duration_s: f64,
) {
    println!(
        "{:<26}{}  ({}/s)",
        "Transfer Duration:",
        fmt_hms_ms(transfer_duration_s),
        fmt_speed_bps(transfer_bps)
    );
    println!(
        "{:<26}{}  ({}/s)",
        "Transfer Flush Duration:",
        fmt_hms_ms(flush_duration_s),
        fmt_speed_bps(flush_bps)
    );
    if let Some((cleanup_duration_s, cleanup_bps, cleanup_flush_duration_s, cleanup_flush_bps)) =
        cleanup
    {
        println!(
            "{:<26}{}  ({}/s)",
            "Cleanup Duration:",
            fmt_hms_ms(cleanup_duration_s),
            fmt_speed_bps(cleanup_bps)
        );
        println!(
            "{:<26}{}  ({}/s)",
            "Cleanup Flush Duration:",
            fmt_hms_ms(cleanup_flush_duration_s),
            fmt_speed_bps(cleanup_flush_bps)
        );
    }
    println!("{:<26}{}", "Total Duration:", fmt_hms_ms(total_duration_s));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_progress_lines_use_the_shared_bar_format() {
        let line = format_object_progress_line("Files", 3_466, 10_000, 34);

        assert!(line.starts_with("Files     34.66% ["));
        assert!(line.ends_with(" 3,466 / 10,000"));
        assert_eq!(line.matches('=').count() + line.matches('>').count(), 12);
    }

    #[test]
    fn resized_frames_count_reflowed_rows() {
        assert_eq!(visual_rows_for_frame("0123456789\nabc", 10), 2);
        assert_eq!(visual_rows_for_frame("0123456789\nabc", 5), 3);
    }
}
