//! Remote endpoint dispatch and remote-transfer user interaction.
#![allow(clippy::too_many_arguments)]

use super::cleanup::cleanup_source_dirs;
use super::orchestrator::flush_destination_writes;
use super::rsync::run_rsync_transfer;
use crate::domain::{Endpoint, LogLevel, RemoteSpec, SrcObjKind, TransferMode};
use crate::output::{
    fmt_mode_word, log, log_transfer_complete, print_summary_rate_line, ENDC, FAIL, WARNING, WHITE,
};
use crate::plan::{
    endpoint_to_rsync, enrich_remote_spec, resolve_destination_for_dir,
    resolve_destination_for_file, resolve_source, to_real_path,
};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

pub(crate) fn run_remote_transfer_mode(
    requested_mode: TransferMode,
    source_input: &str,
    source: &str,
    destination: &str,
    source_remote: Option<RemoteSpec>,
    destination_remote: Option<RemoteSpec>,
    use_sudo: bool,
    contents_mode_requested: bool,
    overwrite: bool,
    backup_requested: bool,
    sync_mode: bool,
    preview_only: bool,
) -> i32 {
    if source_remote.is_some() && destination_remote.is_some() {
        log(
            requested_mode,
            "Remote-to-remote paths are not supported in this mode.",
            LogLevel::Error,
        );
        return 1;
    }

    let is_move = requested_mode == TransferMode::Move;
    if is_move && (source_remote.is_some() || destination_remote.is_some()) {
        log(
            requested_mode,
            "Move with a remote endpoint is disabled because remote durability and source cleanup cannot be verified safely; use copy, verify it, then remove the source explicitly.",
            LogLevel::Error,
        );
        return 1;
    }
    let mut local_src_kind: Option<SrcObjKind> = None;
    let source_ep = match source_remote {
        Some(r) => Endpoint::Remote(enrich_remote_spec(r)),
        None => match resolve_source(source, requested_mode) {
            Ok((p, k)) => {
                local_src_kind = Some(k);
                Endpoint::Local(p)
            }
            Err(code) => return code,
        },
    };

    let destination_ep = match destination_remote {
        Some(r) => Endpoint::Remote(enrich_remote_spec(r)),
        None => {
            if let Some(kind) = local_src_kind {
                let resolved = match kind {
                    SrcObjKind::File => {
                        resolve_destination_for_file(destination, requested_mode, false)
                    }
                    SrcObjKind::Dir => {
                        resolve_destination_for_dir(destination, requested_mode, false)
                    }
                };
                match resolved {
                    Ok((p, _)) => Endpoint::Local(p),
                    Err(code) => return code,
                }
            } else {
                let p = to_real_path(destination);
                if !p.exists() {
                    let parent = p.parent().unwrap_or_else(|| Path::new("."));
                    if !parent.is_dir() {
                        log(
                            requested_mode,
                            &format!(
                                "Destination parent directory does not exist: {}",
                                parent.display()
                            ),
                            LogLevel::Error,
                        );
                        return 1;
                    }
                }
                Endpoint::Local(p)
            }
        }
    };

    if overwrite {
        log(
            requested_mode,
            "--overwrite is not supported for remote endpoints; refusing to run with different semantics.",
            LogLevel::Error,
        );
        return 1;
    }
    if sync_mode {
        log(
            requested_mode,
            "Sync mode enabled: destination extras will be deleted to match source.",
            LogLevel::Info,
        );
    }
    if backup_requested {
        log(
            requested_mode,
            "--backup is not supported for remote endpoints; refusing to run without the requested backup.",
            LogLevel::Error,
        );
        return 1;
    }

    let contents_active =
        contents_mode_requested && !matches!(local_src_kind, Some(SrcObjKind::File));
    println!(
        "{}",
        [
            fmt_mode_word("Copy", !is_move),
            fmt_mode_word("Move", is_move),
            fmt_mode_word("Sync", sync_mode),
            fmt_mode_word("Backup", false),
            "|".to_string(),
            fmt_mode_word("Merge", true),
            fmt_mode_word("Overwrite", false),
            fmt_mode_word("Contents", contents_active),
            fmt_mode_word("File", matches!(local_src_kind, Some(SrcObjKind::File))),
        ]
        .join(" ")
    );
    println!();

    let src_path = endpoint_to_rsync(&source_ep, true, contents_active, local_src_kind);
    let dst_path = endpoint_to_rsync(&destination_ep, false, false, local_src_kind);
    println!(
        "{WARNING}Remote rsync mode: detailed pre-scan is skipped for remote endpoints.{ENDC}"
    );
    println!("Source: {WHITE}{src_path}{ENDC}");
    println!("Destination: {WHITE}{dst_path}{ENDC}");
    println!();

    if preview_only {
        return 0;
    }

    print!("Proceed with {}? [Y/n]: ", requested_mode.word());
    let _ = io::stdout().flush();
    let mut ans = String::new();
    if let Err(err) = io::stdin().read_line(&mut ans) {
        log(
            requested_mode,
            &format!("Could not read confirmation: {err}; refusing to continue."),
            LogLevel::Error,
        );
        return 1;
    }
    if ans.is_empty() {
        log(
            requested_mode,
            "Confirmation input ended before approval; refusing to continue.",
            LogLevel::Error,
        );
        return 1;
    }
    let ans = ans.trim().to_ascii_lowercase();
    if !ans.is_empty() && ans != "y" && ans != "yes" {
        println!("{FAIL}Cancelled.{ENDC}");
        return 0;
    }

    log(
        requested_mode,
        &format!(
            "Starting {} (rsync backend): {} -> {}...",
            requested_mode.word(),
            source_input,
            destination
        ),
        LogLevel::Info,
    );
    let start_ts = Instant::now();
    let transfer = run_rsync_transfer(
        &src_path, &dst_path, 0, use_sudo, false, sync_mode, !sync_mode,
    );

    let result = if transfer.rc == 0 {
        let mut flush_ok = true;
        if let Endpoint::Local(dst_local) = &destination_ep {
            flush_ok = flush_destination_writes(
                dst_local,
                use_sudo,
                requested_mode,
                transfer.progress_snapshot,
            )
            .ok;
        }
        if !flush_ok {
            log(
                requested_mode,
                "Transfer completed but destination flush failed; refusing remote source cleanup.",
                LogLevel::Error,
            );
            1
        } else {
            if is_move && matches!(source_ep, Endpoint::Local(_)) {
                if let (Endpoint::Local(src_local), Some(SrcObjKind::Dir)) =
                    (&source_ep, local_src_kind)
                {
                    cleanup_source_dirs(src_local, !contents_active, use_sudo, requested_mode);
                }
            }
            log_transfer_complete(requested_mode);
            0
        }
    } else if transfer.rc == 24 {
        log(
            requested_mode,
            &format!(
                "{} failed: some source files vanished during transfer (rsync exit 24).",
                requested_mode.word_cap()
            ),
            LogLevel::Error,
        );
        1
    } else {
        log(
            requested_mode,
            &format!(
                "{} failed: transfer exited with status {}.",
                requested_mode.word_cap(),
                transfer.rc
            ),
            LogLevel::Error,
        );
        1
    };

    let total_elapsed_s = start_ts.elapsed().as_secs_f64();
    let avg_transfer_bps = if transfer.elapsed_s > 0.0 {
        transfer.bytes_done as f64 / transfer.elapsed_s
    } else {
        0.0
    };
    let avg_total_bps = if total_elapsed_s > 0.0 {
        transfer.bytes_done as f64 / total_elapsed_s
    } else {
        0.0
    };
    print_summary_rate_line(
        "Average transfer speed",
        avg_transfer_bps,
        transfer.elapsed_s,
        false,
    );
    print_summary_rate_line("Overall throughput", avg_total_bps, total_elapsed_s, true);
    result
}
