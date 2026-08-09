//! Application entry flow: validation, planning, execution, and summaries.

use super::local::{run_local_transfer, LocalTransferRequest};
use crate::cli::parse_args;
use crate::domain::{LogLevel, MergeCollisionPolicy, TransferMode};
use crate::output::log;
use crate::plan::{create_destination_parents, enrich_remote_spec, parse_remote_spec};
use crate::transfer::{run_multi_source_file_batch, run_remote_transfer_mode};

pub(crate) fn run() -> i32 {
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return code,
    };

    let requested_mode = if args.move_mode {
        TransferMode::Move
    } else {
        TransferMode::Copy
    };
    let is_move = requested_mode == TransferMode::Move;

    let batch_sources: Vec<String> = std::iter::once(args.source.clone())
        .chain(args.extra.iter().cloned())
        .collect();

    let source_input = args.source.clone();
    let mut source = source_input.clone();
    let destination = args.destination.clone();
    let use_sudo = args.sudo;
    let preview_lite = args.preview_lite;
    let preview_only = args.preview_only || preview_lite;
    let backup_requested = args.backup;
    let overwrite = args.overwrite;
    let force_requested = args.contents_only;
    let mut source_glob_contents = false;

    if source.ends_with("/*") {
        source.pop();
        source_glob_contents = true;
    }

    let force = force_requested || source_glob_contents;
    let contents_mode_requested = force_requested || source_glob_contents;
    let source_remote = parse_remote_spec(&source);
    let destination_remote = parse_remote_spec(&destination).map(enrich_remote_spec);

    if args.create_destination_parents && (source_remote.is_some() || destination_remote.is_some())
    {
        log(
            requested_mode,
            "--create-destination-parents is currently supported for local transfers only.",
            LogLevel::Error,
        );
        return 1;
    }

    if (args.replace_dest_symlink || args.merge_collision_policy != MergeCollisionPolicy::default())
        && (use_sudo || source_remote.is_some() || destination_remote.is_some())
    {
        log(
            requested_mode,
            "Collision and symlink replacement flags are only supported with the local Rust backend.",
            LogLevel::Error,
        );
        return 1;
    }

    if args.sync_mode && args.overwrite {
        log(
            requested_mode,
            "--sync cannot be combined with --overwrite. Use one mode.",
            LogLevel::Error,
        );
        return 1;
    }
    if args.sync_mode && is_move {
        log(
            requested_mode,
            "--sync currently supports copy mode only (no --move).",
            LogLevel::Error,
        );
        return 1;
    }
    if args.sync_mode
        && (args.replace_dest_symlink
            || args.merge_collision_policy != MergeCollisionPolicy::default())
    {
        log(
            requested_mode,
            "--sync cannot be combined with collision/symlink replacement flags.",
            LogLevel::Error,
        );
        return 1;
    }

    if !args.extra.is_empty() {
        if source_remote.is_some() || destination_remote.is_some() {
            log(
                requested_mode,
                "Multiple source paths are only supported for local filesystem transfers.",
                LogLevel::Error,
            );
            return 1;
        }
        if args.contents_only {
            log(
                requested_mode,
                "Multiple source paths do not support --contents-only.",
                LogLevel::Error,
            );
            return 1;
        }
        if args.sync_mode {
            log(
                requested_mode,
                "Multiple source paths do not support --sync.",
                LogLevel::Error,
            );
            return 1;
        }
        if args.create_destination_parents {
            if let Err(code) = create_destination_parents(&args.destination, requested_mode) {
                return code;
            }
        }
        return run_multi_source_file_batch(
            requested_mode,
            &batch_sources,
            &args.destination,
            args.sudo,
            args.preview_only || args.preview_lite,
            is_move,
            args.tree_trunc,
            args.showall,
            args.replace_dest_symlink,
            args.merge_collision_policy,
        );
    }

    if source_remote.is_some() || destination_remote.is_some() {
        return run_remote_transfer_mode(
            requested_mode,
            &source_input,
            &source,
            &destination,
            source_remote.map(enrich_remote_spec),
            destination_remote,
            use_sudo,
            contents_mode_requested,
            overwrite,
            backup_requested,
            args.sync_mode,
            preview_only,
        );
    }

    run_local_transfer(LocalTransferRequest {
        args: &args,
        source_input: &source_input,
        source: &source,
        destination: &destination,
        requested_mode,
        preview_only,
        contents_mode_requested,
        force,
        source_glob_contents,
    })
}
