//! Local transfer workflow: destination resolution, preview, planning, and execution.

use crate::cli::CliArgs;
use crate::domain::{
    ChangeItem, ChangeKind, DeleteCleanupOutcome, DstObjKind, LogLevel, PreScan, SrcObjKind,
    TransferBackend, TransferMode,
};
use crate::output::{
    build_change_tree, collect_source_top_entries, fmt_mode_word, format_bytes_binary,
    format_number, log, log_transfer_complete, print_changed_top_preview_with_cache,
    print_copy_duration_summary, print_counts_table, print_preview_counts_table,
    print_preview_root_line, remap_item_under_prefix, remap_path_set_under_prefix,
    render_showall_tree_to_string_with_cache, ENDC, FAIL,
};
use crate::plan::{
    can_fast_rename_same_fs, count_tree_any, create_destination_parents,
    destination_available_bytes, normalize_rel, pre_scan_directory, pre_scan_file,
    realpath_allow_missing, resolve_destination_for_dir, resolve_destination_for_file,
    resolve_source, top_level_rel_component,
};
use crate::runtime::{configure_rayon_threads_for_media, dev_media_kind, transfer_media_kind};
use crate::transfer::{
    backup_base_path, backup_path_with_base, copy_path_to_backup, flush_destination_writes,
    plan_backup_path, prefer_hdd_scheduler_for_paths, premerge_fast_rename_noncolliding_children,
    remove_path_recursive, run_command_capture, run_move_cleanup_phase, run_rsync_transfer,
    run_rust_transfer, run_sync_cleanup_phase,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::TempDir;

pub(crate) struct LocalTransferRequest<'a> {
    pub(crate) args: &'a CliArgs,
    pub(crate) source_input: &'a str,
    pub(crate) source: &'a str,
    pub(crate) destination: &'a str,
    pub(crate) requested_mode: TransferMode,
    pub(crate) preview_only: bool,
    pub(crate) contents_mode_requested: bool,
    pub(crate) force: bool,
    pub(crate) source_glob_contents: bool,
}

pub(crate) fn run_local_transfer(request: LocalTransferRequest<'_>) -> i32 {
    let LocalTransferRequest {
        args,
        source_input,
        source,
        destination,
        requested_mode,
        preview_only,
        contents_mode_requested,
        force,
        source_glob_contents,
    } = request;
    let is_move = requested_mode == TransferMode::Move;
    let use_sudo = args.sudo;
    let backup_requested = args.backup;
    let overwrite = args.overwrite;
    let (src_mnt, src_obj_kind) = match resolve_source(source, requested_mode) {
        Ok(v) => v,
        Err(code) => return code,
    };
    if args.sync_mode && src_obj_kind != SrcObjKind::Dir {
        log(
            requested_mode,
            "--sync currently supports directory sources only.",
            LogLevel::Error,
        );
        return 1;
    }

    if args.create_destination_parents {
        if let Err(code) = create_destination_parents(destination, requested_mode) {
            return code;
        }
    }

    let (dst_mnt, dst_obj_kind) = match src_obj_kind {
        SrcObjKind::File => match resolve_destination_for_file(
            destination,
            requested_mode,
            args.replace_dest_symlink,
        ) {
            Ok(v) => v,
            Err(code) => return code,
        },
        SrcObjKind::Dir => {
            match resolve_destination_for_dir(destination, requested_mode, overwrite) {
                Ok(v) => v,
                Err(code) => return code,
            }
        }
    };

    let source_contents_mode = source_glob_contents && !force;
    let mut descendant_target_contents_mode = false;
    let mut descendant_target_exclude_rel: Option<String> = None;
    if src_obj_kind == SrcObjKind::Dir
        && matches!(
            dst_obj_kind,
            DstObjKind::Dir | DstObjKind::DirExisting | DstObjKind::DirNew
        )
        && !overwrite
    {
        let dst_real = realpath_allow_missing(&dst_mnt);
        if dst_real != src_mnt {
            if let Ok(rel) = dst_real.strip_prefix(&src_mnt) {
                let rel_norm = normalize_rel(rel);
                if !rel_norm.is_empty() {
                    descendant_target_contents_mode = true;
                    descendant_target_exclude_rel = Some(rel_norm);
                }
            }
        }
    }
    let effective_contents_mode_requested = (contents_mode_requested
        || descendant_target_contents_mode)
        && src_obj_kind == SrcObjKind::Dir;
    let effective_source_contents_mode = (source_contents_mode || descendant_target_contents_mode)
        && src_obj_kind == SrcObjKind::Dir;
    let dest_tail_raw = destination
        .trim_end_matches('/')
        .split('/')
        .next_back()
        .unwrap_or("");
    let destination_is_dir_ref = destination.ends_with('/')
        || dest_tail_raw.is_empty()
        || dest_tail_raw == "."
        || dest_tail_raw == "..";

    let mut rename_dir_to_new_path = false;
    let mut merge_child_into_parent = false;
    let mut source_already_in_destination = false;
    let mut overwrite_parent_from_child = false;

    if src_obj_kind == SrcObjKind::Dir
        && matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting)
    {
        let dst_slot_for_src =
            realpath_allow_missing(&dst_mnt.join(src_mnt.file_name().unwrap_or_default()));
        if dst_slot_for_src == src_mnt {
            let src_base = src_mnt
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let dst_base = dst_mnt
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if src_base == dst_base {
                source_already_in_destination = true;
                if force && !effective_source_contents_mode {
                    merge_child_into_parent = true;
                    source_already_in_destination = false;
                }
            } else {
                source_already_in_destination = true;
                if overwrite && !effective_source_contents_mode && !destination_is_dir_ref {
                    overwrite_parent_from_child = true;
                    source_already_in_destination = false;
                } else if force && !effective_source_contents_mode {
                    source_already_in_destination = false;
                }
            }
        } else {
            let src_parent_real =
                realpath_allow_missing(src_mnt.parent().unwrap_or_else(|| Path::new(".")));
            if dst_slot_for_src == src_parent_real {
                source_already_in_destination = true;
                if force && !effective_source_contents_mode {
                    merge_child_into_parent = true;
                    source_already_in_destination = false;
                }
            }
        }
    }

    let rename_style_existing_dir_target = src_obj_kind == SrcObjKind::Dir
        && dst_obj_kind == DstObjKind::DirExisting
        && !effective_source_contents_mode
        && !destination_is_dir_ref
        && src_mnt.file_name() != dst_mnt.file_name();

    let mut target_dir_for_name: Option<PathBuf> = None;
    let mut target_name_for_conflict: Option<String> = None;
    let mut overwrite_rename_dir_target = false;
    let mut overwrite_replace_file_target = false;

    if overwrite_parent_from_child || (overwrite && force && rename_style_existing_dir_target) {
        overwrite_rename_dir_target = true;
    }

    if overwrite
        && src_obj_kind == SrcObjKind::Dir
        && dst_obj_kind == DstObjKind::FileExistingForDir
        && !effective_source_contents_mode
    {
        overwrite_replace_file_target = true;
    }

    let force_merge_dir_target = force
        && src_obj_kind == SrcObjKind::Dir
        && matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting)
        && !effective_source_contents_mode
        && !source_already_in_destination
        && !overwrite_parent_from_child
        && !overwrite_rename_dir_target;

    if matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting) {
        if overwrite_rename_dir_target
            || force_merge_dir_target
            || (merge_child_into_parent && src_obj_kind == SrcObjKind::Dir)
        {
            target_dir_for_name = dst_mnt.parent().map(|p| p.to_path_buf());
            target_name_for_conflict = dst_mnt.file_name().map(|s| s.to_string_lossy().to_string());
        } else {
            target_dir_for_name = Some(dst_mnt.clone());
            target_name_for_conflict = match src_obj_kind {
                SrcObjKind::Dir => src_mnt.file_name().map(|s| s.to_string_lossy().to_string()),
                SrcObjKind::File => src_mnt.file_name().map(|s| s.to_string_lossy().to_string()),
            };
        }
    } else if dst_obj_kind == DstObjKind::DirNew && src_obj_kind == SrcObjKind::Dir {
        target_dir_for_name = dst_mnt.parent().map(|p| p.to_path_buf());
        target_name_for_conflict = dst_mnt.file_name().map(|s| s.to_string_lossy().to_string());
    } else if matches!(
        dst_obj_kind,
        DstObjKind::File | DstObjKind::FileExistingForDir
    ) {
        target_dir_for_name = dst_mnt.parent().map(|p| p.to_path_buf());
        target_name_for_conflict = dst_mnt.file_name().map(|s| s.to_string_lossy().to_string());
    }

    let mut target_conflict_path: Option<PathBuf> = None;
    let mut existing_same_name_target = false;
    if let (Some(dir), Some(name)) = (&target_dir_for_name, &target_name_for_conflict) {
        let p = dir.join(name);
        existing_same_name_target = p.exists();
        target_conflict_path = Some(p);
    }

    let mut overwrite_target_path: Option<PathBuf> = None;
    let mut overwrite_target_kind: Option<&str> = None;

    if overwrite_rename_dir_target {
        let candidate_real = realpath_allow_missing(&dst_mnt);
        if candidate_real == src_mnt {
            log(
                requested_mode,
                "Refusing to overwrite source directory itself.",
                LogLevel::Error,
            );
            return 1;
        }
        overwrite_target_path = Some(candidate_real);
        overwrite_target_kind = Some("dir");
    } else if overwrite_replace_file_target {
        let candidate_real = realpath_allow_missing(&dst_mnt);
        if candidate_real == src_mnt {
            log(
                requested_mode,
                "Refusing to overwrite source directory itself.",
                LogLevel::Error,
            );
            return 1;
        }
        overwrite_target_path = Some(candidate_real);
        overwrite_target_kind = Some("file");
    } else if overwrite
        && src_obj_kind == SrcObjKind::Dir
        && matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting)
        && !effective_source_contents_mode
        && !merge_child_into_parent
        && !source_already_in_destination
    {
        if let Some(src_name) = src_mnt.file_name() {
            let candidate = dst_mnt.join(src_name);
            if candidate.exists() && candidate.is_dir() {
                let candidate_real = realpath_allow_missing(&candidate);
                if candidate_real == src_mnt {
                    log(
                        requested_mode,
                        "Refusing to overwrite source directory itself.",
                        LogLevel::Error,
                    );
                    return 1;
                }
                overwrite_target_path = Some(candidate_real);
                overwrite_target_kind = Some("dir");
            }
        }
    }

    let src_path = match src_obj_kind {
        SrcObjKind::File => src_mnt.display().to_string(),
        SrcObjKind::Dir => {
            let src_s = src_mnt.display().to_string();
            if (overwrite_rename_dir_target || overwrite_replace_file_target)
                && !effective_source_contents_mode
            {
                rename_dir_to_new_path = true;
                format!("{}/", src_s.trim_end_matches('/'))
            } else if effective_source_contents_mode || force_merge_dir_target {
                format!("{}/", src_s.trim_end_matches('/'))
            } else if dst_obj_kind == DstObjKind::DirNew && !effective_source_contents_mode {
                if !force {
                    rename_dir_to_new_path = true;
                }
                format!("{}/", src_s.trim_end_matches('/'))
            } else if merge_child_into_parent && !effective_source_contents_mode {
                format!("{}/", src_s.trim_end_matches('/'))
            } else {
                src_s.trim_end_matches('/').to_string()
            }
        }
    };

    let dst_path = if overwrite_rename_dir_target || overwrite_replace_file_target {
        dst_mnt
            .display()
            .to_string()
            .trim_end_matches('/')
            .to_string()
    } else if matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting) {
        format!("{}/", dst_mnt.display().to_string().trim_end_matches('/'))
    } else {
        dst_mnt
            .display()
            .to_string()
            .trim_end_matches('/')
            .to_string()
    };

    let src_parent = src_mnt
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let target_parent = target_dir_for_name.clone().unwrap_or_default();

    let mut _mode_move =
        realpath_allow_missing(&src_parent) != realpath_allow_missing(&target_parent);
    if merge_child_into_parent || overwrite_parent_from_child {
        _mode_move = false;
    }

    let mut mode_overwrite = false;
    let mut mode_merge = false;
    if src_obj_kind == SrcObjKind::Dir {
        if overwrite_target_path.is_some() || (existing_same_name_target && overwrite) {
            mode_overwrite = true;
        } else if force_merge_dir_target || existing_same_name_target {
            mode_merge = true;
        }
    } else if existing_same_name_target {
        mode_overwrite = true;
    }

    if source_already_in_destination {
        mode_overwrite = false;
        _mode_move = false;
        mode_merge = false;
    }

    let mut backup_source_path: Option<PathBuf> = None;
    let mut backup_source_kind: Option<&str> = None;
    if backup_requested && !source_already_in_destination {
        if args.sync_mode {
            let sync_root = if effective_source_contents_mode {
                dst_mnt.clone()
            } else if matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting) {
                dst_mnt.join(src_mnt.file_name().unwrap_or_default())
            } else {
                dst_mnt.clone()
            };
            if sync_root.exists() {
                let p = realpath_allow_missing(&sync_root);
                if p != src_mnt {
                    backup_source_kind = Some(if p.is_dir() { "dir" } else { "file" });
                    backup_source_path = Some(p);
                }
            }
        } else if let Some(otp) = &overwrite_target_path {
            backup_source_path = Some(otp.clone());
            backup_source_kind = overwrite_target_kind;
        } else if (mode_merge || mode_overwrite)
            && target_conflict_path
                .as_ref()
                .map(|p| p.exists())
                .unwrap_or(false)
        {
            if let Some(tp) = &target_conflict_path {
                let p = realpath_allow_missing(tp);
                if p != src_mnt {
                    backup_source_kind = Some(if p.is_dir() { "dir" } else { "file" });
                    backup_source_path = Some(p);
                }
            }
        }
    }

    let mut planned_backup_path: Option<PathBuf> = None;
    if let Some(bsp) = &backup_source_path {
        planned_backup_path = plan_backup_path(bsp);
        if planned_backup_path.is_none() {
            log(
                requested_mode,
                &format!("Failed to plan backup path for: {}", bsp.display()),
                LogLevel::Error,
            );
            return 1;
        }
    }

    let mode_backup = planned_backup_path.is_some();
    let contents_mode_active = effective_contents_mode_requested;

    println!(
        "{}",
        [
            fmt_mode_word("Copy", !is_move),
            fmt_mode_word("Move", is_move),
            fmt_mode_word("Sync", args.sync_mode),
            fmt_mode_word("Backup", mode_backup),
            "|".to_string(),
            fmt_mode_word("Merge", mode_merge),
            fmt_mode_word("Overwrite", mode_overwrite),
            fmt_mode_word("Contents", contents_mode_active),
            fmt_mode_word("File", src_obj_kind == SrcObjKind::File),
        ]
        .join(" ")
    );
    println!();

    let source_media = dev_media_kind(&src_mnt);
    let destination_media = dev_media_kind(&dst_mnt);
    let media = transfer_media_kind(source_media, destination_media);
    let backend = if use_sudo {
        TransferBackend::Rsync
    } else {
        TransferBackend::Rust
    };
    configure_rayon_threads_for_media(media);
    let build_transfer_manifest = !preview_only
        && src_obj_kind == SrcObjKind::Dir
        && (matches!(backend, TransferBackend::Rust) || is_move);

    let prescan = if source_already_in_destination {
        PreScan::default()
    } else {
        let mut pre_dst_path = dst_path.clone();
        let mut preflight_tmpdir: Option<TempDir> = None;

        if src_obj_kind == SrcObjKind::Dir && overwrite_target_path.is_some() {
            let pre_parent = dst_mnt.parent().unwrap_or_else(|| Path::new("."));
            if let Ok(td) = tempfile::Builder::new()
                .prefix(&format!(".{}-preflight-", requested_mode.word()))
                .tempdir_in(pre_parent)
            {
                pre_dst_path = td.path().join("target").display().to_string();
                preflight_tmpdir = Some(td);
            }
        }

        let ps = match src_obj_kind {
            SrcObjKind::Dir => pre_scan_directory(
                &src_path,
                &pre_dst_path,
                &src_mnt,
                build_transfer_manifest,
                is_move,
                args.showall,
                preview_only,
                args.sync_mode,
                args.replace_dest_symlink,
                args.merge_collision_policy,
                descendant_target_exclude_rel.as_deref(),
                args.tree_depth,
                args.preview_lite,
            ),
            SrcObjKind::File => pre_scan_file(
                &src_mnt,
                &pre_dst_path,
                dst_obj_kind,
                args.showall,
                preview_only,
                args.replace_dest_symlink,
                args.merge_collision_policy,
                None,
            ),
        };

        drop(preflight_tmpdir);
        ps
    };

    let mut planned_bytes = prescan.planned_bytes;
    let planned_bytes_exact = prescan.planned_bytes_exact;
    let total_regular_files = prescan.total_regular_files;
    let total_regular_bytes = prescan.total_regular_bytes;
    let total_dirs = prescan.total_dirs;
    let add_files = prescan.add_files;
    let mod_files = prescan.mod_files;
    let uncollided_files = prescan.uncollided_files;
    let add_dirs = prescan.add_dirs;
    let mod_dirs = prescan.mod_dirs;
    let uncollided_dirs = prescan.uncollided_dirs;
    let mut transfer_manifest = prescan.transfer_manifest;
    let mut display_change_preview = prescan.change_preview.clone();
    let mut source_display_paths: HashSet<String> = if args.showall {
        prescan.source_display_paths.iter().cloned().collect()
    } else {
        HashSet::new()
    };
    let has_itemized_changes = prescan.has_itemized_changes;

    let (manifest_cleanup_files, manifest_cleanup_bytes) = if is_move {
        if let Some(m) = transfer_manifest.as_ref() {
            let mut seen: HashSet<&str> = HashSet::new();
            let mut files: u64 = 0;
            let mut bytes: u64 = 0;
            for e in m.identical_files.iter().chain(m.copy_files.iter()) {
                if e.rel.is_empty() || !seen.insert(e.rel.as_ref()) {
                    continue;
                }
                files += 1;
                bytes = bytes.saturating_add(e.size);
            }
            (files, bytes)
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    let dst_preview_root = if matches!(
        dst_obj_kind,
        DstObjKind::Dir | DstObjKind::DirExisting | DstObjKind::DirNew
    ) {
        PathBuf::from(format!("{}/", dst_path.trim_end_matches('/')))
    } else {
        PathBuf::from(
            Path::new(dst_path.trim_end_matches('/'))
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .display()
                .to_string()
                + "/",
        )
    };

    let mut simple_rename_src: Option<String> = None;
    let mut simple_rename_dst: Option<String> = None;
    let mut simple_rename_parent: Option<PathBuf> = None;
    let mut rename_target_only: Option<String> = None;
    let mut rename_target_is_dir = false;

    if src_obj_kind == SrcObjKind::Dir && rename_dir_to_new_path {
        let src_base = src_mnt
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let dst_base = dst_mnt
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let src_parent = src_mnt
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        let dst_parent = dst_mnt
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        if src_parent == dst_parent
            && !src_base.is_empty()
            && !dst_base.is_empty()
            && src_base != dst_base
        {
            simple_rename_src = Some(src_base);
            simple_rename_dst = Some(dst_base);
            simple_rename_parent = Some(src_parent);
        } else if !src_base.is_empty() && !dst_base.is_empty() {
            rename_target_only = Some(dst_base);
            rename_target_is_dir = true;
        }
    } else if src_obj_kind == SrcObjKind::File && dst_obj_kind == DstObjKind::File {
        let src_base = src_mnt
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let dst_base = dst_mnt
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let src_parent = src_mnt
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        let dst_parent = dst_mnt
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        if src_parent == dst_parent
            && !src_base.is_empty()
            && !dst_base.is_empty()
            && src_base != dst_base
        {
            simple_rename_src = Some(src_base);
            simple_rename_dst = Some(dst_base);
            simple_rename_parent = Some(src_parent);
        } else if !src_base.is_empty()
            && !dst_base.is_empty()
            && (src_parent != dst_parent || src_base != dst_base)
        {
            rename_target_only = Some(dst_base);
        }
    }

    let preview_inside_target_dir =
        rename_target_only.is_some() && rename_target_is_dir && overwrite_target_path.is_some();

    let mut preview_root = if let Some(parent) = &simple_rename_parent {
        PathBuf::from(format!(
            "{}/",
            parent.display().to_string().trim_end_matches('/')
        ))
    } else if rename_target_only.is_some() && !preview_inside_target_dir && !rename_target_is_dir {
        let rename_parent = if rename_target_is_dir {
            dst_mnt.parent().unwrap_or_else(|| Path::new("/"))
        } else {
            dst_mnt.parent().unwrap_or_else(|| Path::new("/"))
        };
        PathBuf::from(format!(
            "{}/",
            rename_parent.display().to_string().trim_end_matches('/')
        ))
    } else {
        dst_preview_root.clone()
    };

    if simple_rename_parent.is_some()
        && src_obj_kind == SrcObjKind::Dir
        && simple_rename_dst.is_some()
    {
        let dst_name = simple_rename_dst.clone().unwrap_or_default();
        if args.showall {
            source_display_paths = remap_path_set_under_prefix(&source_display_paths, &dst_name);
        }
        if !display_change_preview.is_empty() {
            let mut remapped = Vec::new();
            for ch in display_change_preview {
                let item = ch.rel.trim_start_matches("./").to_string();
                let rel = remap_item_under_prefix(&item, &dst_name);
                remapped.push(ChangeItem { kind: ch.kind, rel });
            }
            display_change_preview = remapped;
        }
    }

    if let Some(pb) = &planned_backup_path {
        if overwrite_target_path.is_none() {
            let backup_parent = pb.parent().unwrap_or_else(|| Path::new("/"));
            let current_root = PathBuf::from(
                preview_root
                    .to_string_lossy()
                    .trim_end_matches('/')
                    .to_string(),
            );
            if realpath_allow_missing(backup_parent) != realpath_allow_missing(&current_root) {
                let current_root_name = current_root
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if args.showall && !current_root_name.is_empty() {
                    source_display_paths =
                        remap_path_set_under_prefix(&source_display_paths, &current_root_name);
                    if !display_change_preview.is_empty() {
                        let mut remapped = Vec::new();
                        for ch in display_change_preview {
                            let item = ch.rel.trim_start_matches("./").to_string();
                            let rel = remap_item_under_prefix(&item, &current_root_name);
                            remapped.push(ChangeItem { kind: ch.kind, rel });
                        }
                        display_change_preview = remapped;
                    }
                }
                preview_root = PathBuf::from(format!(
                    "{}/",
                    backup_parent.display().to_string().trim_end_matches('/')
                ));
            }
        }
    }

    if is_move && merge_child_into_parent && src_obj_kind == SrcObjKind::Dir {
        let preview_root_real = realpath_allow_missing(Path::new(
            preview_root.to_string_lossy().trim_end_matches('/'),
        ));
        let src_real = realpath_allow_missing(&src_mnt);
        if let Ok(removed_rel) = src_real.strip_prefix(&preview_root_real) {
            let rel = normalize_rel(removed_rel);
            if !rel.is_empty() && rel != "." && rel != ".." && !rel.starts_with("../") {
                display_change_preview.push(ChangeItem {
                    kind: ChangeKind::RemovedDir,
                    rel: format!("{}/", rel.trim_end_matches('/')),
                });
            }
        }
    }

    let preview_root_lossy = preview_root.to_string_lossy();
    let preview_root_trimmed = Path::new(preview_root_lossy.trim_end_matches('/'));
    let highlight_new_preview_leaf = src_obj_kind == SrcObjKind::Dir
        && matches!(dst_obj_kind, DstObjKind::DirNew)
        && !preview_root_trimmed.exists();
    let overwrite_requires_action = overwrite_target_path
        .as_ref()
        .map(|p| p.exists())
        .unwrap_or(false);
    let has_move_cleanup_work = is_move
        && !source_already_in_destination
        && (manifest_cleanup_files > 0 || manifest_cleanup_bytes > 0);
    let sync_delete_requires_action =
        args.sync_mode && (uncollided_files > 0 || uncollided_dirs > 0);
    let emphasize_preview_root = has_itemized_changes
        || planned_bytes > 0
        || overwrite_requires_action
        || sync_delete_requires_action
        || has_move_cleanup_work;
    print_preview_root_line(
        &preview_root,
        highlight_new_preview_leaf,
        emphasize_preview_root,
    );

    let mut extra_added: HashSet<String> = HashSet::new();
    let mut extra_modified: HashSet<String> = HashSet::new();
    let mut extra_replaced: HashSet<String> = HashSet::new();
    let mut extra_removed: HashSet<String> = HashSet::new();

    if let Some(pb) = &planned_backup_path {
        if overwrite_target_path.is_none() {
            if let Some(n) = pb.file_name().map(|s| s.to_string_lossy().to_string()) {
                extra_added.insert(n);
            }
        }
    }
    if let Some(otp) = &overwrite_target_path {
        if !preview_inside_target_dir {
            if let Some(n) = otp.file_name().map(|s| s.to_string_lossy().to_string()) {
                extra_replaced.insert(n);
            }
        }
    }
    if let Some(rt) = &rename_target_only {
        if !preview_inside_target_dir && !rename_target_is_dir {
            if existing_same_name_target {
                extra_modified.insert(rt.trim_end_matches('/').to_string());
            } else {
                extra_added.insert(rt.trim_end_matches('/').to_string());
            }
        }
    }
    if let Some(sd) = &simple_rename_dst {
        if existing_same_name_target {
            extra_modified.insert(sd.trim_end_matches('/').to_string());
        } else {
            extra_added.insert(sd.trim_end_matches('/').to_string());
        }
    }
    if is_move {
        if let Some(ss) = &simple_rename_src {
            extra_removed.insert(ss.trim_end_matches('/').to_string());
        }
    }

    let preview_root_trimmed_owned = preview_root
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    let preview_root_path = Path::new(&preview_root_trimmed_owned);
    {
        const BODY_MAX_LINES: usize = 25;
        let max_depth = args.tree_depth.unwrap_or(1);
        let trunc = args.tree_trunc.max(1);
        let mut dir_cache: HashMap<PathBuf, Vec<(String, bool)>> = HashMap::new();
        let preview_tree = build_change_tree(&display_change_preview);

        let mut best_render: Option<String> = None;
        for try_depth in 1..=max_depth {
            if let Some(rendered) = render_showall_tree_to_string_with_cache(
                preview_root_path,
                &preview_tree,
                &source_display_paths,
                args.showall,
                &extra_added,
                &extra_modified,
                &extra_replaced,
                &extra_removed,
                try_depth,
                trunc,
                Some(BODY_MAX_LINES),
                &mut dir_cache,
            ) {
                best_render = Some(rendered);
            } else {
                break;
            }
        }

        if let Some(rendered) = best_render {
            print!("{rendered}");
            println!();
        } else {
            let source_top_entries = collect_source_top_entries(
                &src_mnt,
                src_obj_kind,
                !src_path.ends_with('/'),
                simple_rename_dst.as_deref(),
                rename_target_only.as_deref(),
                rename_target_is_dir,
            );
            print_changed_top_preview_with_cache(
                preview_root_path,
                &display_change_preview,
                &source_top_entries,
                args.showall,
                &extra_added,
                &extra_modified,
                &extra_replaced,
                &extra_removed,
                trunc,
                Some(&mut dir_cache),
            );
        }
    }

    let move_cleanup_only = is_move
        && !source_already_in_destination
        && existing_same_name_target
        && planned_bytes == 0
        && !has_itemized_changes
        && !overwrite_requires_action;

    let mut likely_cleanup_files = if is_move && !source_already_in_destination {
        if manifest_cleanup_files > 0 {
            manifest_cleanup_files
        } else {
            total_regular_files.unwrap_or(0)
        }
    } else {
        0
    };
    let mut likely_cleanup_bytes = if is_move && !source_already_in_destination {
        if manifest_cleanup_bytes > 0 {
            manifest_cleanup_bytes
        } else {
            total_regular_bytes.unwrap_or(0)
        }
    } else {
        0
    };
    let likely_cleanup_dirs = if is_move && !source_already_in_destination {
        total_dirs.unwrap_or(0)
    } else {
        0
    };

    let overwrite_target_counts = if !args.sync_mode {
        overwrite_target_path
            .as_ref()
            .map(|path| count_tree_any(path, true))
    } else {
        None
    };
    let file_row = total_regular_files.map(|total_regular| {
        let identical_files = total_regular.saturating_sub(add_files + mod_files);
        let deleted_src_files = if is_move && !source_already_in_destination {
            if likely_cleanup_files > 0 {
                likely_cleanup_files
            } else {
                total_regular
            }
        } else {
            0
        };
        let deleted_dest_files = if args.sync_mode {
            uncollided_files
        } else {
            overwrite_target_counts
                .map(|counts| counts.files)
                .unwrap_or(0)
        };
        (
            add_files,
            mod_files,
            identical_files,
            uncollided_files,
            deleted_src_files,
            deleted_dest_files,
        )
    });
    let dir_row = total_dirs.map(|total_source_dirs| {
        let identical_dirs = total_source_dirs.saturating_sub(add_dirs + mod_dirs);
        let deleted_src_dirs = if is_move && !source_already_in_destination {
            likely_cleanup_dirs
        } else {
            0
        };
        let deleted_dest_dirs = if args.sync_mode {
            uncollided_dirs
        } else {
            overwrite_target_counts
                .map(|counts| counts.dirs)
                .unwrap_or(0)
        };
        (
            add_dirs,
            mod_dirs,
            identical_dirs,
            uncollided_dirs,
            deleted_src_dirs,
            deleted_dest_dirs,
        )
    });
    if preview_only {
        if file_row.is_some() || dir_row.is_some() {
            print_preview_counts_table(file_row, dir_row, prescan.file_relation_breakdown);
        }
    } else if file_row.is_some() || dir_row.is_some() {
        print_counts_table(file_row, dir_row);
    }

    println!();
    if planned_bytes_exact {
        println!(
            "Planned transfer bytes: {} ({})",
            format_number(planned_bytes),
            format_bytes_binary(planned_bytes, 2)
        );
    } else {
        println!(
            "Planned transfer bytes: {} ({}) [preview-lite: exact byte scan skipped]",
            format_number(planned_bytes),
            format_bytes_binary(planned_bytes, 2)
        );
    }
    println!();

    if !prescan.scan_complete {
        log(
            requested_mode,
            "Transfer plan is incomplete because one or more paths could not be scanned; refusing to execute or delete anything.",
            LogLevel::Error,
        );
        return 1;
    }

    if preview_only {
        return 0;
    }

    let move_requires_material_action =
        is_move && !source_already_in_destination && !existing_same_name_target;
    let no_changes_planned = source_already_in_destination
        || ((planned_bytes == 0 && !has_itemized_changes && !overwrite_requires_action)
            && !sync_delete_requires_action
            && !move_cleanup_only
            && !move_requires_material_action);

    if overwrite_requires_action && planned_bytes == 0 && !has_itemized_changes {
        log(
            requested_mode,
            "Overwrite requested: destination conflict will be replaced.",
            LogLevel::Info,
        );
    }
    if sync_delete_requires_action && planned_bytes == 0 && !has_itemized_changes {
        log(
            requested_mode,
            "Sync mode: destination-only entries will be deleted.",
            LogLevel::Info,
        );
    }
    if no_changes_planned {
        log(
            requested_mode,
            &format!("No changes detected; nothing to {}.", requested_mode.word()),
            LogLevel::Info,
        );
        return 0;
    }
    if move_cleanup_only {
        log(
            requested_mode,
            "Destination already has matching files; source files will be removed to complete move.",
            LogLevel::Info,
        );
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
        // An explicit negative answer is a successful, non-mutating
        // cancellation. EOF and input errors above remain failures.
        return 0;
    }

    if source_already_in_destination {
        log(
            requested_mode,
            "No changes: source is already in destination directory.",
            LogLevel::Info,
        );
        return 0;
    }

    // Allow fast rename for contents-only moves when destination is a brand-new
    // directory path. In this case, moving source children into a new target dir
    // is equivalent to a single rename(src_dir -> dst_dir).
    let contents_only_dirnew_fastpath = effective_contents_mode_requested
        && src_obj_kind == SrcObjKind::Dir
        && dst_obj_kind == DstObjKind::DirNew;

    let maybe_fast_rename_target = if is_move
        && !use_sudo
        && !backup_requested
        && !overwrite_requires_action
        && !move_cleanup_only
        && !effective_source_contents_mode
        && (!effective_contents_mode_requested || contents_only_dirnew_fastpath)
        && !merge_child_into_parent
        && !overwrite_parent_from_child
        && !overwrite_rename_dir_target
        && !overwrite_replace_file_target
    {
        match src_obj_kind {
            SrcObjKind::File => {
                if matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting) {
                    src_mnt.file_name().map(|n| dst_mnt.join(n))
                } else {
                    Some(dst_mnt.clone())
                }
            }
            SrcObjKind::Dir => match dst_obj_kind {
                DstObjKind::Dir | DstObjKind::DirExisting => {
                    src_mnt.file_name().map(|n| dst_mnt.join(n))
                }
                DstObjKind::DirNew => Some(dst_mnt.clone()),
                _ => None,
            },
        }
    } else {
        None
    };

    let fast_rename_possible = maybe_fast_rename_target
        .as_ref()
        .map(|rename_target| {
            can_fast_rename_same_fs(&src_mnt, rename_target)
                && !rename_target.exists()
                && *rename_target != src_mnt
        })
        .unwrap_or(false);

    let backup_bytes = if backup_requested {
        backup_source_path
            .as_ref()
            .and_then(|path| fs::symlink_metadata(path).ok())
            .map(|meta| {
                if meta.is_dir() {
                    count_tree_any(
                        backup_source_path.as_deref().unwrap_or(Path::new(".")),
                        false,
                    )
                    .bytes
                } else {
                    meta.len()
                }
            })
            .unwrap_or(0)
    } else {
        0
    };
    let required_space = planned_bytes.saturating_add(backup_bytes);
    if required_space > 0 && !fast_rename_possible {
        match destination_available_bytes(&dst_mnt) {
            Ok((available_bytes, probe_path)) => {
                if available_bytes < required_space {
                    let msg = format!(
                        "Insufficient free space on destination filesystem (probe: {}). Need: {} ({}), available: {} ({}).",
                        probe_path.display(),
                        format_number(required_space),
                        format_bytes_binary(required_space, 2),
                        format_number(available_bytes),
                        format_bytes_binary(available_bytes, 2)
                    );
                    log(requested_mode, &msg, LogLevel::Error);
                    return 1;
                }
            }
            Err(err) => {
                log(
                    requested_mode,
                    &format!("Could not determine destination free space: {err}"),
                    LogLevel::Warn,
                );
            }
        }
    }

    if let Some(rename_target) = maybe_fast_rename_target {
        if fast_rename_possible && fs::rename(&src_mnt, &rename_target).is_ok() {
            let flush = flush_destination_writes(&rename_target, use_sudo, requested_mode, None);
            if !flush.ok {
                log(
                        requested_mode,
                        "Fast-path rename completed but destination flush failed; durability is unconfirmed.",
                        LogLevel::Error,
                    );
                return 1;
            }
            log(
                requested_mode,
                &format!(
                    "Fast-path rename on same filesystem: {} -> {}",
                    src_mnt.display(),
                    rename_target.display()
                ),
                LogLevel::Info,
            );
            println!();
            log_transfer_complete(requested_mode);
            return 0;
        }
    }

    prefer_hdd_scheduler_for_paths(&[&src_mnt, &dst_mnt], use_sudo, requested_mode);

    let backend_name = match backend {
        TransferBackend::Rust => "rust",
        TransferBackend::Rsync => "rsync",
    };

    if move_cleanup_only {
        log(
            requested_mode,
            &format!(
                "Starting {} cleanup: {} -> {}...",
                requested_mode.word(),
                source_input,
                destination
            ),
            LogLevel::Info,
        );
    } else {
        log(
            requested_mode,
            &format!(
                "Starting {} ({} backend): {} -> {}...",
                requested_mode.word(),
                backend_name,
                source_input,
                destination
            ),
            LogLevel::Info,
        );
    }

    let start_ts = Instant::now();
    let mut transferred_bytes_total: u64 = 0;
    let mut transferred_elapsed_total_s: f64 = 0.0;
    let mut deleted_cleanup_total = DeleteCleanupOutcome::default();
    let mut cleanup_elapsed_total_s: f64 = 0.0;
    let mut cleanup_flush_elapsed_s: f64 = 0.0;
    let mut cleanup_flush_bytes_total: u64 = 0;
    let mut transfer_flush_elapsed_s: f64 = 0.0;
    let mut transfer_flush_bytes_total: u64 = 0;

    let result: i32 = (|| {
        if backup_requested {
            if let Some(bsp) = &backup_source_path {
                if overwrite_target_path.is_none() {
                    println!();
                    if backup_source_kind == Some("file") {
                        log(
                            requested_mode,
                            &format!("Backing up existing file: {}", bsp.display()),
                            LogLevel::Info,
                        );
                    } else {
                        log(
                            requested_mode,
                            &format!("Backing up existing directory: {}", bsp.display()),
                            LogLevel::Info,
                        );
                    }
                    if let Some(pbp) = &planned_backup_path {
                        if copy_path_to_backup(bsp, pbp, use_sudo, requested_mode).is_none() {
                            return 1;
                        }
                        log(
                            requested_mode,
                            &format!("Backup saved as: {}", pbp.display()),
                            LogLevel::Info,
                        );
                    }
                    println!();
                }
            }
        }

        if let Some(otp) = &overwrite_target_path {
            if overwrite_parent_from_child {
                let stage_parent = dst_mnt.parent().unwrap_or_else(|| Path::new("."));
                let stage_path = match tempfile::Builder::new()
                    .prefix(&format!(".{}-stage-", requested_mode.word()))
                    .tempdir_in(stage_parent)
                {
                    Ok(td) => td.keep(),
                    Err(_) => {
                        log(
                            requested_mode,
                            "Failed to create staging directory.",
                            LogLevel::Error,
                        );
                        return 1;
                    }
                };

                log(
                    requested_mode,
                    &format!("Staging source before overwrite: {}", stage_path.display()),
                    LogLevel::Info,
                );

                let transfer = match backend {
                    TransferBackend::Rsync => run_rsync_transfer(
                        &src_path,
                        &stage_path.display().to_string(),
                        planned_bytes,
                        use_sudo,
                        false,
                        args.sync_mode,
                        !args.sync_mode,
                    ),
                    TransferBackend::Rust => run_rust_transfer(
                        &src_path,
                        &stage_path.display().to_string(),
                        src_obj_kind,
                        is_move,
                        requested_mode,
                        planned_bytes,
                        transfer_manifest.as_ref(),
                        media,
                        args.replace_dest_symlink,
                        args.merge_collision_policy,
                        args.sync_mode,
                        descendant_target_exclude_rel.as_deref(),
                    ),
                };
                transferred_bytes_total += transfer.bytes_done;
                transferred_elapsed_total_s += transfer.elapsed_s;
                let rc_transfer = transfer.rc;

                if rc_transfer == 0 {
                    if backup_requested {
                        println!();
                        log(
                            requested_mode,
                            &format!("Backing up existing directory: {}", otp.display()),
                            LogLevel::Info,
                        );
                        let backup_base = planned_backup_path
                            .clone()
                            .or_else(|| backup_base_path(otp));
                        if let Some(bb) = backup_base {
                            if backup_path_with_base(otp, use_sudo, &bb, requested_mode).is_none() {
                                let _ =
                                    remove_path_recursive(&stage_path, use_sudo, requested_mode);
                                return 1;
                            }
                        } else {
                            return 1;
                        }
                        println!();
                    } else {
                        log(
                            requested_mode,
                            &format!("Overwriting existing directory: {}", otp.display()),
                            LogLevel::Info,
                        );
                        if !remove_path_recursive(otp, use_sudo, requested_mode) {
                            let _ = remove_path_recursive(&stage_path, use_sudo, requested_mode);
                            return 1;
                        }
                    }

                    if use_sudo {
                        let cmd = vec![
                            "mv".to_string(),
                            "--".to_string(),
                            stage_path.display().to_string(),
                            otp.display().to_string(),
                        ];
                        let mv_ok = run_command_capture(&cmd, true)
                            .map(|o| o.code == 0)
                            .unwrap_or(false);
                        if !mv_ok {
                            log(
                                requested_mode,
                                "Failed to place staged directory into destination.",
                                LogLevel::Error,
                            );
                            let _ = remove_path_recursive(&stage_path, use_sudo, requested_mode);
                            return 1;
                        }
                    } else if fs::rename(&stage_path, otp).is_err() {
                        log(
                            requested_mode,
                            "Failed to place staged directory into destination.",
                            LogLevel::Error,
                        );
                        let _ = remove_path_recursive(&stage_path, use_sudo, requested_mode);
                        return 1;
                    }

                    if rc_transfer == 0 {
                        let flush_stats = flush_destination_writes(
                            otp,
                            use_sudo,
                            requested_mode,
                            transfer.progress_snapshot,
                        );
                        transfer_flush_elapsed_s += flush_stats.elapsed_s;
                        if let Some(b) = flush_stats.flushed_bytes {
                            transfer_flush_bytes_total =
                                transfer_flush_bytes_total.saturating_add(b);
                        }
                        if !flush_stats.ok {
                            log(
                                requested_mode,
                                "Transfer completed but destination flush failed; refusing source cleanup.",
                                LogLevel::Error,
                            );
                            return 1;
                        }
                        log_transfer_complete(requested_mode);
                        if is_move {
                            let cleanup = run_move_cleanup_phase(
                                &src_path,
                                &stage_path.display().to_string(),
                                &src_mnt,
                                src_obj_kind,
                                effective_contents_mode_requested,
                                effective_source_contents_mode,
                                descendant_target_exclude_rel.as_deref(),
                                true,
                                use_sudo,
                                requested_mode,
                                transfer_manifest.as_ref(),
                                likely_cleanup_files,
                                likely_cleanup_bytes,
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
                                cleanup_flush_bytes_total =
                                    cleanup_flush_bytes_total.saturating_add(b);
                            }
                        }
                        return 0;
                    }
                    if is_move {
                        let cleanup = run_move_cleanup_phase(
                            &src_path,
                            &stage_path.display().to_string(),
                            &src_mnt,
                            src_obj_kind,
                            effective_contents_mode_requested,
                            effective_source_contents_mode,
                            descendant_target_exclude_rel.as_deref(),
                            true,
                            use_sudo,
                            requested_mode,
                            transfer_manifest.as_ref(),
                            likely_cleanup_files,
                            likely_cleanup_bytes,
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
                    log(
                        requested_mode,
                        &format!("{} failed: some source files vanished during transfer (rsync exit 24).", requested_mode.word_cap()),
                        LogLevel::Error,
                    );
                    log(
                        requested_mode,
                        &format!(
                            "Re-run {} to converge once the source tree is stable.",
                            requested_mode.word()
                        ),
                        LogLevel::Error,
                    );
                    return 1;
                }

                log(
                    requested_mode,
                    &format!(
                        "{} failed: rsync exited with status {}.",
                        requested_mode.word_cap(),
                        rc_transfer
                    ),
                    LogLevel::Error,
                );
                let _ = remove_path_recursive(&stage_path, use_sudo, requested_mode);
                return 1;
            }

            if backup_requested {
                println!();
                if overwrite_target_kind == Some("file") {
                    log(
                        requested_mode,
                        &format!("Backing up existing file: {}", otp.display()),
                        LogLevel::Info,
                    );
                } else {
                    log(
                        requested_mode,
                        &format!("Backing up existing directory: {}", otp.display()),
                        LogLevel::Info,
                    );
                }
                let backup_base = planned_backup_path
                    .clone()
                    .or_else(|| backup_base_path(otp));
                if let Some(bb) = backup_base {
                    if let Some(bp) = backup_path_with_base(otp, use_sudo, &bb, requested_mode) {
                        log(
                            requested_mode,
                            &format!("Backup saved as: {}", bp.display()),
                            LogLevel::Info,
                        );
                    } else {
                        return 1;
                    }
                } else {
                    return 1;
                }
                println!();
            } else {
                if overwrite_target_kind == Some("file") {
                    log(
                        requested_mode,
                        &format!("Overwriting existing file: {}", otp.display()),
                        LogLevel::Info,
                    );
                } else {
                    log(
                        requested_mode,
                        &format!("Overwriting existing directory: {}", otp.display()),
                        LogLevel::Info,
                    );
                }
                if !remove_path_recursive(otp, use_sudo, requested_mode) {
                    return 1;
                }
            }
        }

        if is_move
            && matches!(backend, TransferBackend::Rust)
            && src_obj_kind == SrcObjKind::Dir
            && effective_contents_mode_requested
            && matches!(dst_obj_kind, DstObjKind::Dir | DstObjKind::DirExisting)
            && !source_already_in_destination
            && !use_sudo
            && !backup_requested
            && !overwrite_requires_action
            && !overwrite_parent_from_child
            && !overwrite_rename_dir_target
            && !overwrite_replace_file_target
        {
            let rename_stats = premerge_fast_rename_noncolliding_children(
                &src_mnt,
                &dst_mnt,
                transfer_manifest.as_mut(),
                descendant_target_exclude_rel
                    .as_deref()
                    .and_then(top_level_rel_component),
            );
            if rename_stats.moved_entries > 0 {
                planned_bytes = planned_bytes.saturating_sub(rename_stats.removed_copy_bytes);
                likely_cleanup_files =
                    likely_cleanup_files.saturating_sub(rename_stats.moved_files);
                likely_cleanup_bytes =
                    likely_cleanup_bytes.saturating_sub(rename_stats.moved_bytes);
                log(
                    requested_mode,
                    &format!(
                        "Fast-path pre-merge rename: moved {} non-colliding entries directly into destination.",
                        rename_stats.moved_entries
                    ),
                    LogLevel::Info,
                );
            }
        }

        if move_cleanup_only {
            let cleanup = run_move_cleanup_phase(
                &src_path,
                &dst_path,
                &src_mnt,
                src_obj_kind,
                effective_contents_mode_requested,
                effective_source_contents_mode,
                descendant_target_exclude_rel.as_deref(),
                rename_dir_to_new_path,
                use_sudo,
                requested_mode,
                transfer_manifest.as_ref(),
                likely_cleanup_files,
                likely_cleanup_bytes,
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
            log_transfer_complete(requested_mode);
            return 0;
        }

        let transfer = match backend {
            TransferBackend::Rsync => run_rsync_transfer(
                &src_path,
                &dst_path,
                planned_bytes,
                use_sudo,
                false,
                args.sync_mode,
                !args.sync_mode,
            ),
            TransferBackend::Rust => run_rust_transfer(
                &src_path,
                &dst_path,
                src_obj_kind,
                is_move,
                requested_mode,
                planned_bytes,
                transfer_manifest.as_ref(),
                media,
                args.replace_dest_symlink,
                args.merge_collision_policy,
                args.sync_mode,
                descendant_target_exclude_rel.as_deref(),
            ),
        };
        transferred_bytes_total += transfer.bytes_done;
        transferred_elapsed_total_s += transfer.elapsed_s;
        let rc_transfer = transfer.rc;

        if rc_transfer == 0 {
            let flush_stats = flush_destination_writes(
                &dst_mnt,
                use_sudo,
                requested_mode,
                transfer.progress_snapshot,
            );
            transfer_flush_elapsed_s += flush_stats.elapsed_s;
            if let Some(b) = flush_stats.flushed_bytes {
                transfer_flush_bytes_total = transfer_flush_bytes_total.saturating_add(b);
            }
            if !flush_stats.ok {
                log(
                    requested_mode,
                    "Transfer completed but destination flush failed; refusing source cleanup.",
                    LogLevel::Error,
                );
                return 1;
            }
            if src_obj_kind == SrcObjKind::Dir {
                if let Some(manifest) = transfer_manifest.as_ref() {
                    let source_base = src_path
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or_default();
                    if let Err(err) = crate::transfer::preserve_directory_times_tree(
                        Path::new(src_path.trim_end_matches('/')),
                        Path::new(dst_path.trim_end_matches('/')),
                        !src_path.ends_with('/'),
                        source_base,
                        Some(&manifest.dir_times),
                    ) {
                        log(
                            requested_mode,
                            &format!("Failed to restore directory timestamps: {err}"),
                            LogLevel::Error,
                        );
                        return 1;
                    }
                }
            }
            log_transfer_complete(requested_mode);
            if is_move {
                let cleanup = run_move_cleanup_phase(
                    &src_path,
                    &dst_path,
                    &src_mnt,
                    src_obj_kind,
                    effective_contents_mode_requested,
                    effective_source_contents_mode,
                    descendant_target_exclude_rel.as_deref(),
                    rename_dir_to_new_path,
                    use_sudo,
                    requested_mode,
                    transfer_manifest.as_ref(),
                    likely_cleanup_files,
                    likely_cleanup_bytes,
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
                if !cleanup.success {
                    return 1;
                }
            }
            if args.sync_mode && matches!(backend, TransferBackend::Rust) {
                if let Some(manifest) = transfer_manifest.as_ref() {
                    if !manifest.sync_delete_files.is_empty()
                        || !manifest.sync_delete_dirs.is_empty()
                    {
                        let cleanup =
                            run_sync_cleanup_phase(&src_path, &dst_path, requested_mode, manifest);
                        deleted_cleanup_total.files = deleted_cleanup_total
                            .files
                            .saturating_add(cleanup.stats.deleted.files);
                        deleted_cleanup_total.bytes = deleted_cleanup_total
                            .bytes
                            .saturating_add(cleanup.stats.deleted.bytes);
                        cleanup_elapsed_total_s += cleanup.stats.cleanup_elapsed_s;
                        cleanup_flush_elapsed_s += cleanup.stats.flush.elapsed_s;
                        if let Some(bytes) = cleanup.stats.flush.flushed_bytes {
                            cleanup_flush_bytes_total =
                                cleanup_flush_bytes_total.saturating_add(bytes);
                        }
                        if !cleanup.success {
                            return 1;
                        }
                    }
                }
            }
            return 0;
        }
        if rc_transfer == 24 {
            log(
                requested_mode,
                &format!(
                    "{} failed: some source files vanished during transfer (rsync exit 24).",
                    requested_mode.word_cap()
                ),
                LogLevel::Error,
            );
            log(
                requested_mode,
                &format!(
                    "Re-run {} to converge once the source tree is stable.",
                    requested_mode.word()
                ),
                LogLevel::Error,
            );
            return 1;
        }

        log(
            requested_mode,
            &format!(
                "{} failed: transfer exited with status {}.",
                requested_mode.word_cap(),
                rc_transfer
            ),
            LogLevel::Error,
        );
        1
    })();

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
    let cleanup_summary = if is_move
        || (args.sync_mode && (cleanup_elapsed_total_s > 0.0 || cleanup_flush_elapsed_s > 0.0))
    {
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
    result
}
