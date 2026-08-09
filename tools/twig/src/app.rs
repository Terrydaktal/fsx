use crate::cli::*;
use crate::fs_ops::*;
use crate::git::*;
use crate::model::*;
use crate::render::*;
use chrono::{Datelike, Local};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use users::{get_group_by_gid, get_user_by_uid};

pub(crate) fn sort_entries(entries: &mut [EntryInfo], ctx: &Context, reverse_sorted_output: bool) {
    entries.sort_unstable_by(|a, b| {
        match ctx.sort_by {
            SortBy::Size => {
                if a.final_size != b.final_size {
                    return b.final_size.cmp(&a.final_size);
                }
                return a.size_sort_name.cmp(&b.size_sort_name);
            }
            SortBy::Date => {
                let a_time = a.sort_mtime;
                let b_time = b.sort_mtime;
                if a_time != b_time {
                    return b_time.cmp(&a_time);
                }
            }
            SortBy::Type => {
                let a_rank = a.type_rank;
                let b_rank = b.type_rank;
                if a_rank != b_rank {
                    return a_rank.cmp(&b_rank);
                }
            }
            SortBy::DirCount => {
                let a_count = a.count_sort_key;
                let b_count = b.count_sort_key;
                if a_count != b_count {
                    return b_count.cmp(&a_count);
                }
                if ctx.sort_counts_total && a.dir_count != b.dir_count {
                    return b.dir_count.cmp(&a.dir_count);
                }
            }
            SortBy::FileCount => {
                if a.file_count != b.file_count {
                    return b.file_count.cmp(&a.file_count);
                }
            }
            SortBy::Name => {}
        }
        if a.is_hidden != b.is_hidden {
            return b.is_hidden.cmp(&a.is_hidden);
        }
        a.display_name.cmp(&b.display_name)
    });
    if reverse_sorted_output {
        entries.reverse();
    }
}

pub(crate) fn emit_entries(
    cli: &Cli,
    ctx: &mut Context,
    mut entries: Vec<EntryInfo>,
    reverse_sorted_output: bool,
    pin_dot: bool,
    cache_raw_enabled: bool,
) -> io::Result<()> {
    if entries.is_empty() {
        if cache_raw_enabled {
            write_cache_raw_paths(&[], &[])?;
        }
        return Ok(());
    }

    let over_auto_limit = entries.len() > AUTO_STYLE_MAX_ENTRIES;
    ctx.color_enabled = output_enabled(cli.color, !io::stdout().is_terminal(), over_auto_limit);
    ctx.hyperlink = output_enabled(cli.hyperlink, !io::stdout().is_terminal(), over_auto_limit);

    sort_entries(&mut entries, ctx, reverse_sorted_output);
    if pin_dot {
        pin_dot_entries_top(&mut entries);
    }

    if cache_raw_enabled {
        let (shown_dir_paths, shown_file_paths) = collect_output_paths(&entries, &ctx.cwd);
        write_cache_raw_paths(&shown_dir_paths, &shown_file_paths)?;
    }

    let detail_columns = build_detail_columns(ctx);
    let is_list_mode =
        !io::stdout().is_terminal() || cli.list || cli.header || !detail_columns.is_empty();

    let output = if is_list_mode {
        print_detailed_list(&entries, ctx, &detail_columns)
    } else {
        let mut out = String::with_capacity(entries.len().saturating_mul(32));
        for (idx, entry) in entries.iter().enumerate() {
            if idx > 0 {
                out.push_str("  ");
            }
            if entry.is_symlink && entry.broken_symlink {
                let mut broken_text =
                    get_display_name_text(&entry.render_name, &entry.metadata, ctx);
                if ctx.show_targets {
                    if let Some(target) = entry.symlink_target.as_ref() {
                        broken_text.push_str(" -> ");
                        broken_text.push_str(&escape_terminal_text(&target.to_string_lossy()));
                    }
                }
                out.push_str(&highlight_broken_symlink_text(
                    &broken_text,
                    ctx.color_enabled,
                ));
            } else {
                out.push_str(&get_styled_name(
                    &entry.render_name,
                    &entry.actual_path,
                    &entry.metadata,
                    ctx,
                ));
                if ctx.show_targets {
                    if let Some(target) = entry.symlink_target.as_ref() {
                        out.push_str(" -> ");
                        out.push_str(&get_symlink_target_display(
                            &entry.actual_path,
                            target,
                            entry.target_metadata.as_ref(),
                            ctx,
                        ));
                    }
                }
            }
        }
        out.push('\n');
        out
    };

    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())
}

pub(crate) fn render_multiple_paths(cli: Cli) -> io::Result<()> {
    let (mut ctx, _sort_explicit, implicit_ascending_sort, _pin_dot_entries, piped_output) =
        build_context_and_sort_state(&cli);
    let cache_raw_enabled = cli.cache_raw && !piped_output;
    let need_counts = cli.counts || matches!(ctx.sort_by, SortBy::DirCount | SortBy::FileCount);
    let now = Local::now();
    let now_year = now.year();
    let now_timestamp = now.timestamp();
    let mut user_cache: HashMap<u32, String> = HashMap::new();
    let mut group_cache: HashMap<u32, String> = HashMap::new();
    let mut entries = Vec::new();

    let mut git_status_visible = false;
    let mut git_repo_visible = false;
    let mut git_remote_visible = false;
    let mut git_status_cache: HashMap<PathBuf, HashMap<String, (char, char)>> = HashMap::new();
    let mut git_repo_marker_cache: HashMap<PathBuf, (Option<char>, Option<char>)> = HashMap::new();

    for path in &cli.paths {
        let actual_path = path.clone();
        let metadata = match fs::symlink_metadata(&actual_path) {
            Ok(m) => m,
            Err(err) => return Err(err),
        };
        let is_actual_dir = metadata.is_dir();
        let need_target_metadata = cli.dereference
            || cli.dirs_only
            || cli.git
            || cli.cache_raw
            || ctx.show_targets
            || ctx.color_enabled
            || ctx.sort_by == SortBy::Type;
        let target_metadata = if metadata.file_type().is_symlink() && need_target_metadata {
            fs::metadata(&actual_path).ok()
        } else {
            None
        };
        let is_target_dir = target_metadata.as_ref().is_some_and(fs::Metadata::is_dir);
        let is_dirish = is_actual_dir || is_target_dir;
        if cli.git_fetch {
            git_fetch_repos_for_listing_path(&actual_path);
        }
        if cli.dirs_only && !is_dirish {
            continue;
        }

        let display_name = actual_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());

        let (recursive_sizes, recursive_counts, root_true_size, root_recursive_counts) =
            if is_actual_dir || (cli.dereference && is_target_dir) {
                collect_recursive_stats(
                    &actual_path,
                    true,
                    cli.dedupe_hardlinks,
                    cli.true_size,
                    need_counts,
                )
            } else {
                (HashMap::new(), HashMap::new(), None, None)
            };

        let mut entry = create_entry_info(
            &display_name,
            actual_path.clone(),
            metadata,
            target_metadata,
            &ctx,
            &recursive_sizes,
            &recursive_counts,
            &mut user_cache,
            &mut group_cache,
            now_year,
            now_timestamp,
        );
        if need_counts && is_actual_dir {
            let (root_dirs, root_files) = root_recursive_counts.unwrap_or((0, 0));
            entry.dir_count = root_dirs;
            entry.file_count = root_files;
            entry.dir_count_str = root_dirs.to_string();
            entry.file_count_str = root_files.to_string();
            entry.count_sort_key = if ctx.sort_counts_total {
                root_dirs.saturating_add(root_files)
            } else {
                root_dirs
            };
        }
        if cli.true_size && (is_actual_dir || (cli.dereference && is_target_dir)) {
            if let Some(total_true_size) = root_true_size {
                entry.true_size_str = format_size(total_true_size);
                entry.final_size = total_true_size;
            }
        }

        if cli.git {
            let status_base = if is_dirish {
                actual_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            } else {
                actual_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            };

            let repo_root = git_repo_root(&status_base);
            let status_map = git_status_cache
                .entry(status_base.clone())
                .or_insert_with(|| {
                    collect_git_statuses_with_root(&status_base, repo_root.as_deref())
                        .unwrap_or_default()
                });
            if !status_map.is_empty() {
                git_status_visible = true;
            }
            if let Some(status) = status_map.get(&display_name).copied() {
                entry.git_status = Some(status);
            } else if repo_root.is_some() {
                entry.git_status = Some(('-', '-'));
                git_status_visible = true;
            }

            if is_dirish {
                let markers = git_repo_marker_cache
                    .entry(actual_path.clone())
                    .or_insert_with(|| git_repo_root_markers(&actual_path));
                entry.repo_status = markers.0;
                entry.repo_remote_status = markers.1;
                if markers.0.is_some() {
                    git_repo_visible = true;
                }
                if markers.1.is_some() {
                    git_remote_visible = true;
                }
            }
        }

        entries.push(entry);
    }

    if cli.git {
        ctx.show_git = git_status_visible;
        ctx.show_git_repos = git_repo_visible;
        ctx.show_git_remote = git_remote_visible;
        if !ctx.show_git && !ctx.show_git_repos && !ctx.show_git_remote {
            ctx.show_git = true;
        }
    }

    emit_entries(
        &cli,
        &mut ctx,
        entries,
        cli.reverse ^ implicit_ascending_sort,
        false,
        cache_raw_enabled,
    )
}

pub(crate) fn render_path(cli: Cli) -> io::Result<()> {
    let (mut ctx, _sort_explicit, implicit_ascending_sort, pin_dot_entries, piped_output) =
        build_context_and_sort_state(&cli);
    let show_hidden = cli.all || cli.almost_all;
    let cache_raw_enabled = cli.cache_raw && !piped_output;
    let mut entries = Vec::new();
    let need_counts = cli.counts || matches!(ctx.sort_by, SortBy::DirCount | SortBy::FileCount);
    let input_path = Path::new(&cli.path);
    let input_meta = fs::symlink_metadata(input_path)?;
    let input_is_dir = input_meta.is_dir();
    let need_input_target_metadata = cli.dereference
        || cli.dirs_only
        || cli.git
        || ctx.show_targets
        || ctx.color_enabled
        || ctx.sort_by == SortBy::Type;
    let input_target_metadata = if input_meta.file_type().is_symlink() && need_input_target_metadata
    {
        fs::metadata(input_path).ok()
    } else {
        None
    };
    let input_target_is_dir = input_target_metadata
        .as_ref()
        .is_some_and(fs::Metadata::is_dir);
    let stats_target_is_dir = input_is_dir || (cli.dereference && input_target_is_dir);
    let (recursive_sizes, recursive_counts, root_true_size, root_recursive_counts) =
        if stats_target_is_dir {
            collect_recursive_stats(
                input_path,
                true,
                cli.dedupe_hardlinks,
                cli.true_size,
                need_counts,
            )
        } else {
            (HashMap::new(), HashMap::new(), None, None)
        };
    let now = Local::now();
    let now_year = now.year();
    let now_timestamp = now.timestamp();
    let mut user_cache: HashMap<u32, String> = HashMap::new();
    let mut group_cache: HashMap<u32, String> = HashMap::new();

    if cli.git_fetch {
        git_fetch_repos_for_listing_path(input_path);
    }

    if let Some(output) = try_render_large_dir_long_fast_path(
        &cli,
        &mut ctx,
        input_is_dir,
        cli.all && pin_dot_entries,
        show_hidden,
        piped_output,
        cache_raw_enabled,
    )? {
        let mut stdout = io::stdout().lock();
        stdout.write_all(output.as_bytes())?;
        return Ok(());
    }

    if let Some(output) = try_render_large_dir_fast_path(
        &cli,
        &mut ctx,
        input_is_dir,
        cli.all && pin_dot_entries,
        show_hidden,
        piped_output,
        cache_raw_enabled,
    )? {
        let mut stdout = io::stdout().lock();
        stdout.write_all(output.as_bytes())?;
        return Ok(());
    }

    if cli.all && input_is_dir && !cli.no_traverse {
        if let Ok(m) = fs::symlink_metadata(&cli.path) {
            entries.push(create_entry_info(
                ".",
                PathBuf::from(&cli.path),
                m,
                None,
                &ctx,
                &recursive_sizes,
                &recursive_counts,
                &mut user_cache,
                &mut group_cache,
                now_year,
                now_timestamp,
            ));
        }
        if !cli.true_size {
            let parent_path = if cli.path == Path::new(".") {
                "..".to_string()
            } else {
                cli.path.join("..").to_string_lossy().into_owned()
            };
            if let Ok(m) = fs::symlink_metadata(&parent_path) {
                entries.push(create_entry_info(
                    "..",
                    PathBuf::from(&parent_path),
                    m,
                    None,
                    &ctx,
                    &recursive_sizes,
                    &recursive_counts,
                    &mut user_cache,
                    &mut group_cache,
                    now_year,
                    now_timestamp,
                ));
            }
        }
    }

    if input_is_dir && !cli.no_traverse {
        let read_dir = match fs::read_dir(&cli.path) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };

        for dir_entry in read_dir {
            let dir_entry = match dir_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let file_name = dir_entry.file_name().to_string_lossy().to_string();
            let is_hidden = file_name.starts_with('.');
            if is_hidden && !show_hidden {
                continue;
            }
            let entry_path = dir_entry.path();
            let metadata = match fs::symlink_metadata(&entry_path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let target_metadata = if metadata.file_type().is_symlink()
                && (cli.dereference
                    || cli.dirs_only
                    || cli.git
                    || cli.cache_raw
                    || ctx.show_targets
                    || ctx.color_enabled
                    || ctx.sort_by == SortBy::Type)
            {
                fs::metadata(&entry_path).ok()
            } else {
                None
            };
            if cli.dirs_only
                && !metadata.is_dir()
                && !target_metadata.as_ref().is_some_and(fs::Metadata::is_dir)
            {
                continue;
            }
            entries.push(create_entry_info(
                &file_name,
                entry_path,
                metadata,
                target_metadata,
                &ctx,
                &recursive_sizes,
                &recursive_counts,
                &mut user_cache,
                &mut group_cache,
                now_year,
                now_timestamp,
            ));
        }
    } else {
        let metadata = input_meta;
        if cli.dirs_only && !metadata.is_dir() && !input_target_is_dir {
            return Ok(());
        }
        let file_name = input_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| cli.path.to_string_lossy().into_owned());
        entries.push(create_entry_info(
            &file_name,
            cli.path.clone(),
            metadata,
            input_target_metadata,
            &ctx,
            &recursive_sizes,
            &recursive_counts,
            &mut user_cache,
            &mut group_cache,
            now_year,
            now_timestamp,
        ));
    }

    if cli.all && cli.true_size && input_is_dir && !cli.no_traverse {
        let dot_true_size = root_true_size.unwrap_or_else(|| {
            recursive_dir_on_disk_size(Path::new(&cli.path), true, cli.dedupe_hardlinks)
        });
        if let Some(dot_entry) = entries.iter_mut().find(|e| e.display_name == ".") {
            dot_entry.true_size_str = format_size(dot_true_size);
            dot_entry.final_size = dot_true_size;
        }
    }
    if cli.no_traverse && cli.true_size && input_is_dir {
        if let Some(dir_entry) = entries.get_mut(0) {
            if let Some(total_true_size) = root_true_size {
                dir_entry.true_size_str = format_size(total_true_size);
                dir_entry.final_size = total_true_size;
            }
        }
    }
    if need_counts && input_is_dir {
        let target_name = if cli.no_traverse { None } else { Some(".") };
        if let Some(dir_entry) = entries.iter_mut().find(|entry| {
            target_name
                .map(|name| entry.display_name == name)
                .unwrap_or(true)
        }) {
            let (root_dirs, root_files) = root_recursive_counts.unwrap_or((0, 0));
            dir_entry.dir_count = root_dirs;
            dir_entry.file_count = root_files;
            dir_entry.dir_count_str = root_dirs.to_string();
            dir_entry.file_count_str = root_files.to_string();
            dir_entry.count_sort_key = if ctx.sort_counts_total {
                root_dirs.saturating_add(root_files)
            } else {
                root_dirs
            };
        }
    }

    if entries.is_empty() {
        if cache_raw_enabled {
            write_cache_raw_paths(&[], &[])?;
        }
        return Ok(());
    }

    if cli.git {
        let (show_git_status, show_repo_status, show_remote_status) =
            populate_git_columns(Path::new(&cli.path), &mut entries);
        ctx.show_git = show_git_status;
        ctx.show_git_repos = show_repo_status;
        ctx.show_git_remote = show_remote_status;
        if !ctx.show_git && !ctx.show_git_repos && !ctx.show_git_remote {
            // Keep -G in detailed/list mode even when no repo-related markers
            // apply in the current listing.
            ctx.show_git = true;
        }
    }

    emit_entries(
        &cli,
        &mut ctx,
        entries,
        cli.reverse ^ implicit_ascending_sort,
        cli.all && input_is_dir && pin_dot_entries,
        cache_raw_enabled,
    )
}

pub(crate) fn collect_output_paths(
    entries: &[EntryInfo],
    cwd: &Path,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dir_paths = Vec::new();
    let mut file_paths = Vec::new();
    for entry in entries {
        let full_path = normalize_path_lexical(&to_full_path_with_cwd(&entry.actual_path, cwd));
        let is_dir = if entry.is_symlink {
            entry.is_target_dir
        } else {
            entry.is_dir
        };
        if is_dir {
            dir_paths.push(full_path);
        } else {
            file_paths.push(full_path);
        }
    }
    (dir_paths, file_paths)
}

pub(crate) fn create_entry_info(
    display_name: &str,
    actual_path: PathBuf,
    metadata: fs::Metadata,
    target_metadata: Option<fs::Metadata>,
    ctx: &Context,
    recursive_sizes: &HashMap<OsString, u64>,
    recursive_counts: &HashMap<OsString, (u64, u64)>,
    user_cache: &mut HashMap<u32, String>,
    group_cache: &mut HashMap<u32, String>,
    now_year: i32,
    now_timestamp: i64,
) -> EntryInfo {
    let is_symlink = metadata.file_type().is_symlink();
    let is_dir = metadata.is_dir();
    let is_hidden = display_name.starts_with('.') && display_name != "." && display_name != "..";
    let needs_logical_size =
        ctx.show_size_logical || (ctx.sort_by == SortBy::Size && !ctx.show_size_true);
    let needs_true_size = ctx.show_size_true;
    let needs_counts =
        ctx.show_counts || matches!(ctx.sort_by, SortBy::DirCount | SortBy::FileCount);
    let render_name = if ctx.absolute {
        normalize_path_lexical(&to_full_path_with_cwd(&actual_path, &ctx.cwd))
            .to_string_lossy()
            .into_owned()
    } else {
        display_name.to_string()
    };

    let mut is_target_dir = false;
    let mut symlink_target = None;
    let mut broken_symlink = false;
    let mut target_meta = target_metadata;
    if is_symlink {
        if ctx.show_targets {
            symlink_target = fs::read_link(&actual_path).ok();
        }
        if target_meta.is_none()
            && (ctx.dereference
                || ctx.show_targets
                || ctx.color_enabled
                || ctx.sort_by == SortBy::Type)
        {
            target_meta = fs::metadata(&actual_path).ok();
        }
        is_target_dir = target_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        broken_symlink = target_meta.is_none();
    }

    let logical_size = if !needs_logical_size {
        0
    } else if is_symlink && ctx.dereference && !broken_symlink {
        match target_meta.as_ref() {
            Some(meta) if meta.is_dir() => on_disk_size(meta),
            Some(meta) => meta.len(),
            None => {
                if is_dir {
                    on_disk_size(&metadata)
                } else {
                    metadata.len()
                }
            }
        }
    } else if is_dir {
        on_disk_size(&metadata)
    } else {
        metadata.len()
    };

    let true_size = if !needs_true_size {
        0
    } else if is_symlink && ctx.dereference && !broken_symlink {
        match target_meta.as_ref() {
            Some(meta) if meta.is_dir() => {
                recursive_dir_on_disk_size(&actual_path, true, ctx.dedupe_hardlinks)
            }
            Some(meta) => on_disk_size(meta),
            None => on_disk_size(&metadata),
        }
    } else if is_dir {
        recursive_sizes
            .get(OsStr::new(display_name))
            .copied()
            .unwrap_or_else(|| on_disk_size(&metadata))
    } else {
        on_disk_size(&metadata)
    };

    let final_size = if ctx.show_size_true {
        true_size
    } else {
        logical_size
    };
    let logical_size_str = if needs_logical_size {
        format_size(logical_size)
    } else {
        String::new()
    };
    let true_size_str = if needs_true_size {
        format_size(true_size)
    } else {
        String::new()
    };
    let (dir_count, file_count) = if !needs_counts {
        (0, 0)
    } else if is_dir {
        recursive_counts
            .get(OsStr::new(display_name))
            .copied()
            .unwrap_or((0, 0))
    } else {
        (0, 1)
    };
    let dir_count_str = if needs_counts {
        dir_count.to_string()
    } else {
        String::new()
    };
    let file_count_str = if needs_counts {
        file_count.to_string()
    } else {
        String::new()
    };

    let user_str = if ctx.show_owner {
        user_cache
            .entry(metadata.uid())
            .or_insert_with(|| {
                get_user_by_uid(metadata.uid())
                    .map(|u| u.name().to_string_lossy().into_owned())
                    .unwrap_or_else(|| metadata.uid().to_string())
            })
            .clone()
    } else {
        String::new()
    };
    let group_str = if ctx.show_group {
        group_cache
            .entry(metadata.gid())
            .or_insert_with(|| {
                get_group_by_gid(metadata.gid())
                    .map(|g| g.name().to_string_lossy().into_owned())
                    .unwrap_or_else(|| metadata.gid().to_string())
            })
            .clone()
    } else {
        String::new()
    };

    let sort_mtime = if is_symlink && ctx.dereference && !broken_symlink {
        target_meta
            .as_ref()
            .map(|m| m.mtime())
            .unwrap_or_else(|| metadata.mtime())
    } else {
        metadata.mtime()
    };

    let time_str = if ctx.show_time {
        format_time_display(sort_mtime, now_year, now_timestamp)
    } else {
        String::new()
    };
    let size_sort_name = if ctx.sort_by == SortBy::Size {
        display_name.trim_start_matches('.').to_ascii_lowercase()
    } else {
        String::new()
    };
    let type_rank = if is_symlink && is_target_dir {
        0
    } else if is_dir {
        1
    } else if is_symlink {
        2
    } else {
        3
    };
    let count_sort_key = if ctx.sort_counts_total {
        dir_count.saturating_add(file_count)
    } else {
        dir_count
    };

    EntryInfo {
        display_name: display_name.to_string(),
        render_name,
        actual_path,
        metadata,
        is_symlink,
        is_dir,
        is_target_dir,
        is_hidden,
        logical_size_str,
        true_size_str,
        dir_count_str,
        file_count_str,
        user_str,
        group_str,
        time_str,
        final_size,
        dir_count,
        file_count,
        sort_mtime,
        size_sort_name,
        type_rank,
        count_sort_key,
        symlink_target,
        target_metadata: target_meta,
        broken_symlink,
        git_status: None,
        repo_status: None,
        repo_remote_status: None,
    }
}

pub(crate) fn run(cli: Cli) -> io::Result<()> {
    if cli.paths.len() > 1 {
        return render_multiple_paths(cli);
    }
    for path in cli.paths.iter().cloned() {
        let mut path_cli = cli.clone();
        path_cli.path = path;
        render_path(path_cli)?;
    }
    Ok(())
}
