//! Rsync subprocess integration and progress-stream translation.

use super::telemetry::{counter_delta, device_io_deltas, proc_io_deltas};
use crate::domain::{
    DeviceIoWindow, ProcessIoWindow, ProgressSnapshot, RsyncStreamEvent, TransferOutcome,
    TransferProgressRates,
};
use crate::output::{
    finish_progress_render_state, print_transfer_columns_header, print_transfer_progress_bars,
    TransferEtaEstimator,
};
use regex::Regex;
use std::io::Read;
use std::io::{self, BufRead};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn parse_progress2_bytes(line: &str) -> Option<u64> {
    let re = Regex::new(r"^\s*([0-9][0-9,]*(?:\.[0-9]+)?)([kKmMgGtTpPeE]?)\s+[0-9]{1,3}%").ok()?;
    let caps = re.captures(line)?;
    let num_txt = caps.get(1)?.as_str().replace(',', "");
    let unit = caps
        .get(2)
        .map(|m| m.as_str().to_ascii_uppercase())
        .unwrap_or_default();
    let mut val: f64 = num_txt.parse().ok()?;
    let mult = match unit.as_str() {
        "K" => 1024f64,
        "M" => 1024f64.powi(2),
        "G" => 1024f64.powi(3),
        "T" => 1024f64.powi(4),
        "P" => 1024f64.powi(5),
        "E" => 1024f64.powi(6),
        _ => 1.0,
    };
    val *= mult;
    Some(val as u64)
}

pub(crate) fn handle_rsync_stream_line(tx: &mpsc::Sender<RsyncStreamEvent>, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(bytes) = parse_progress2_bytes(trimmed) {
        let _ = tx.send(RsyncStreamEvent::Progress(bytes));
    } else {
        let _ = tx.send(RsyncStreamEvent::Text(trimmed.to_string()));
    }
}

pub(crate) fn spawn_rsync_stdout_reader(
    stdout: impl Read + Send + 'static,
    tx: mpsc::Sender<RsyncStreamEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let reader = io::BufReader::new(stdout);
        for result in reader.lines() {
            match result {
                Ok(line) => handle_rsync_stream_line(&tx, &line),
                Err(err) => {
                    let _ = tx.send(RsyncStreamEvent::Text(format!(
                        "rsync stdout read error: {err}"
                    )));
                    break;
                }
            }
        }
    })
}

pub(crate) fn spawn_rsync_stderr_reader(
    stderr: impl Read + Send + 'static,
    tx: mpsc::Sender<RsyncStreamEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = io::BufReader::new(stderr);
        let mut buf = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::new();

        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    let _ = tx.send(RsyncStreamEvent::Text(format!(
                        "rsync stderr read error: {err}"
                    )));
                    break;
                }
            };
            pending.extend_from_slice(&buf[..n]);

            let mut consumed = 0usize;
            for i in 0..pending.len() {
                let b = pending[i];
                if b == b'\n' || b == b'\r' {
                    if i > consumed {
                        let chunk = &pending[consumed..i];
                        let line = String::from_utf8_lossy(chunk);
                        handle_rsync_stream_line(&tx, &line);
                    }
                    consumed = i + 1;
                }
            }
            if consumed > 0 {
                pending.drain(..consumed);
            }
        }

        if !pending.is_empty() {
            let line = String::from_utf8_lossy(&pending);
            handle_rsync_stream_line(&tx, &line);
        }
    })
}

pub(crate) fn run_rsync_transfer(
    src_path: &str,
    dst_path: &str,
    planned_bytes: u64,
    use_sudo: bool,
    remove_source_during: bool,
    delete_destination_extras: bool,
    size_only: bool,
) -> TransferOutcome {
    let mut cmd: Vec<String> = vec![
        "rsync".to_string(),
        "-aH".to_string(),
        "--partial".to_string(),
        "--protect-args".to_string(),
    ];
    if size_only {
        cmd.push("--size-only".to_string());
    }
    if delete_destination_extras {
        cmd.push("--delete".to_string());
    }
    if remove_source_during {
        cmd.push("--remove-source-files".to_string());
    }
    cmd.extend([
        "--info=progress2,stats2,name0".to_string(),
        "--".to_string(),
        src_path.to_string(),
        dst_path.to_string(),
    ]);

    let mut full_cmd = Vec::new();
    if use_sudo {
        full_cmd.push("pkexec".to_string());
    }
    full_cmd.extend(cmd);

    let mut child = match Command::new(&full_cmd[0])
        .args(&full_cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            eprintln!("copy-rs: failed to start rsync: {err}");
            return TransferOutcome {
                rc: 1,
                bytes_done: 0,
                elapsed_s: 0.0,
                progress_snapshot: None,
            };
        }
    };

    let transfer_start = Instant::now();
    print_transfer_columns_header();
    let mut done_bytes: u64 = 0;
    let mut last_report = transfer_start;
    let mut last_done_bytes: u64 = 0;
    let mut last_done_at = transfer_start;
    let mut io_window = ProcessIoWindow::from_pid(child.id());
    let _ = io_window.sample();
    let io_start_counters = io_window.current_totals();
    let device_window = DeviceIoWindow::from_transfer_paths(src_path, dst_path);
    let device_start_totals = device_window.current_totals();
    let mut last_device_totals = device_start_totals;
    let mut last_device_at = transfer_start;
    let mut last_io_rates = TransferProgressRates::default();
    let mut eta_estimator = TransferEtaEstimator::default();

    let (event_tx, event_rx) = mpsc::channel::<RsyncStreamEvent>();
    let stdout_handle = child
        .stdout
        .take()
        .map(|stdout| spawn_rsync_stdout_reader(stdout, event_tx.clone()));
    let stderr_handle = child
        .stderr
        .take()
        .map(|stderr| spawn_rsync_stderr_reader(stderr, event_tx.clone()));
    drop(event_tx);
    let mut progress_line_active = false;

    let rc: i32 = loop {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(RsyncStreamEvent::Progress(bytes)) => {
                if bytes > done_bytes {
                    done_bytes = bytes;
                }
            }
            Ok(RsyncStreamEvent::Text(line)) => {
                if progress_line_active {
                    finish_progress_render_state();
                    progress_line_active = false;
                }
                println!("{line}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }

        let now = Instant::now();
        if now.duration_since(last_report) >= Duration::from_millis(200) {
            last_io_rates = io_window.sample();
            let io_delta = proc_io_deltas(io_start_counters, io_window.last_counters);
            let device_now_totals = device_window.current_totals();
            let device_delta = device_io_deltas(device_start_totals, device_now_totals);
            let dt = now.duration_since(last_done_at).as_secs_f64().max(1e-6);
            last_io_rates.write_all_bps =
                Some(done_bytes.saturating_sub(last_done_bytes) as f64 / dt);
            let dt_dev = now.duration_since(last_device_at).as_secs_f64().max(1e-6);
            last_io_rates.read_complete_bps =
                counter_delta(last_device_totals.0, device_now_totals.0).map(|v| v as f64 / dt_dev);
            last_io_rates.write_complete_bps =
                counter_delta(last_device_totals.1, device_now_totals.1).map(|v| v as f64 / dt_dev);
            last_device_totals = device_now_totals;
            last_device_at = now;
            last_done_bytes = done_bytes;
            last_done_at = now;
            print_transfer_progress_bars(
                now.duration_since(transfer_start).as_secs_f64(),
                planned_bytes,
                Some(done_bytes),
                "Transfer",
                last_io_rates,
                io_delta,
                device_delta,
                None,
                None,
                Some(&mut eta_estimator),
                false,
                false,
            );
            progress_line_active = true;
            last_report = now;
        }

        if let Ok(Some(status)) = child.try_wait() {
            while let Ok(event) = event_rx.recv_timeout(Duration::from_millis(20)) {
                match event {
                    RsyncStreamEvent::Progress(bytes) => {
                        if bytes > done_bytes {
                            done_bytes = bytes;
                        }
                    }
                    RsyncStreamEvent::Text(line) => {
                        if progress_line_active {
                            finish_progress_render_state();
                            progress_line_active = false;
                        }
                        println!("{line}");
                    }
                }
            }
            break status.code().unwrap_or(1);
        }
    };

    if let Some(h) = stdout_handle {
        let _ = h.join();
    }
    if let Some(h) = stderr_handle {
        let _ = h.join();
    }

    let final_done = if planned_bytes > 0 && rc == 0 {
        planned_bytes.max(done_bytes)
    } else {
        done_bytes
    };
    let io_end_totals = io_window.current_totals();
    let io_delta = proc_io_deltas(io_start_counters, io_end_totals);
    let device_end_totals = device_window.current_totals();
    let device_delta = device_io_deltas(device_start_totals, device_end_totals);
    let total_elapsed_s = transfer_start.elapsed().as_secs_f64().max(1e-6);
    if last_io_rates.write_all_bps.is_none() {
        last_io_rates.write_all_bps = Some(final_done as f64 / total_elapsed_s);
    }
    if last_io_rates.rchar_bps.is_none() {
        last_io_rates.rchar_bps = io_delta.rchar.map(|v| v as f64 / total_elapsed_s);
    }
    if last_io_rates.wchar_bps.is_none() {
        last_io_rates.wchar_bps = io_delta.wchar.map(|v| v as f64 / total_elapsed_s);
    }
    if last_io_rates.read_bytes_bps.is_none() {
        last_io_rates.read_bytes_bps = io_delta.read_bytes.map(|v| v as f64 / total_elapsed_s);
    }
    if last_io_rates.write_bytes_bps.is_none() {
        last_io_rates.write_bytes_bps = io_delta.write_bytes.map(|v| v as f64 / total_elapsed_s);
    }
    if last_io_rates.read_complete_bps.is_none() {
        last_io_rates.read_complete_bps = device_delta
            .read_complete
            .map(|v| v as f64 / total_elapsed_s);
    }
    if last_io_rates.write_complete_bps.is_none() {
        last_io_rates.write_complete_bps = device_delta
            .write_complete
            .map(|v| v as f64 / total_elapsed_s);
    }
    print_transfer_progress_bars(
        transfer_start.elapsed().as_secs_f64(),
        planned_bytes,
        Some(final_done),
        "Transfer",
        last_io_rates,
        io_delta,
        device_delta,
        None,
        None,
        Some(&mut eta_estimator),
        true,
        false,
    );

    TransferOutcome {
        rc,
        bytes_done: final_done,
        elapsed_s: transfer_start.elapsed().as_secs_f64(),
        progress_snapshot: Some(ProgressSnapshot {
            elapsed_s: transfer_start.elapsed().as_secs_f64(),
            planned_bytes,
            write_all_total: Some(final_done),
            phase_label: "Transfer",
            rates: last_io_rates,
            proc_deltas: io_delta,
            device_deltas: device_delta,
            eta_workload: None,
            eta_progress: None,
        }),
    }
}

pub(crate) fn run_rsync_transfer_sources(
    src_paths: &[String],
    dst_path: &str,
    planned_bytes: u64,
    use_sudo: bool,
    remove_source_during: bool,
    delete_destination_extras: bool,
    size_only: bool,
) -> TransferOutcome {
    if src_paths.len() <= 1 {
        return run_rsync_transfer(
            src_paths.first().map(String::as_str).unwrap_or(""),
            dst_path,
            planned_bytes,
            use_sudo,
            remove_source_during,
            delete_destination_extras,
            size_only,
        );
    }

    let mut aggregate = TransferOutcome {
        rc: 0,
        bytes_done: 0,
        elapsed_s: 0.0,
        progress_snapshot: None,
    };
    for (index, source) in src_paths.iter().enumerate() {
        let transfer = run_rsync_transfer(
            source,
            dst_path,
            if index + 1 == src_paths.len() {
                planned_bytes.saturating_sub(aggregate.bytes_done)
            } else {
                0
            },
            use_sudo,
            remove_source_during,
            delete_destination_extras && index + 1 == src_paths.len(),
            size_only,
        );
        aggregate.rc = transfer.rc;
        aggregate.bytes_done = aggregate.bytes_done.saturating_add(transfer.bytes_done);
        aggregate.elapsed_s += transfer.elapsed_s;
        aggregate.progress_snapshot = transfer.progress_snapshot;
        // Rsync status 24 means that files vanished during transfer. It is a
        // partial result, not a successful batch that may be committed or
        // followed by move cleanup.
        if transfer.rc != 0 {
            break;
        }
    }
    aggregate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    #[test]
    fn handle_rsync_stream_line_emits_progress_event() {
        let (tx, rx) = mpsc::channel();
        handle_rsync_stream_line(&tx, "   1,024  10%   1.00MB/s    0:00:00");
        match rx.recv().expect("event") {
            RsyncStreamEvent::Progress(bytes) => assert_eq!(bytes, 1024),
            RsyncStreamEvent::Text(line) => panic!("expected progress, got text: {line}"),
        }
    }

    #[test]
    fn handle_rsync_stream_line_emits_text_event() {
        let (tx, rx) = mpsc::channel();
        handle_rsync_stream_line(&tx, "building file list ...");
        match rx.recv().expect("event") {
            RsyncStreamEvent::Text(line) => assert_eq!(line, "building file list ..."),
            RsyncStreamEvent::Progress(bytes) => panic!("expected text, got progress: {bytes}"),
        }
    }
}
