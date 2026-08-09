use crate::cli::{Cli, Context, DetailColumn, SortBy, dot_entry_rank, output_enabled};
use crate::fs_ops::*;
use crate::git::*;
use crate::model::EntryInfo;
use chrono::{DateTime, Datelike, Local};
use lscolors::Style;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use users::get_user_by_uid;

pub(crate) const AUTO_STYLE_MAX_ENTRIES: usize = 1000;

pub(crate) fn escape_terminal_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{7f}' => escaped.push_str("\\x7f"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{{{:x}}}", ch as u32);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) fn paint_if_enabled(style: nu_ansi_term::Style, text: &str, enabled: bool) -> String {
    if enabled {
        style.paint(text).to_string()
    } else {
        text.to_string()
    }
}

struct FastEntry {
    display_name: String,
    actual_path: PathBuf,
    is_hidden: bool,
    is_dir: bool,
    is_symlink: bool,
    is_target_dir: bool,
    type_rank: u8,
}

struct LongFastEntry {
    display_name: String,
    actual_path: PathBuf,
    metadata: fs::Metadata,
    is_hidden: bool,
    is_dir: bool,
    is_symlink: bool,
    is_target_dir: bool,
    type_rank: u8,
    logical_size: u64,
    logical_size_str: String,
    user_str: String,
    time_str: String,
    sort_mtime: i64,
    size_sort_name: String,
    symlink_target: Option<PathBuf>,
    target_metadata: Option<fs::Metadata>,
    broken_symlink: bool,
}

pub(crate) fn can_use_large_dir_fast_path(cli: &Cli, input_is_dir: bool) -> bool {
    input_is_dir
        && !cli.no_traverse
        && !cli.long
        && !cli.list
        && !cli.header
        && !cli.permissions
        && !cli.size
        && !cli.counts
        && !cli.owner
        && !cli.group
        && !cli.modified
        && !cli.classify
        && !cli.show_targets
        && !cli.absolute
        && !cli.dereference
        && !cli.git
        && !cli.true_size
        && !cli.date_sort
        && matches!(cli.sort, SortBy::Name | SortBy::Type)
}

pub(crate) fn can_use_large_dir_long_fast_path(cli: &Cli, input_is_dir: bool) -> bool {
    input_is_dir
        && !cli.no_traverse
        && cli.long
        && !cli.list
        && !cli.permissions
        && !cli.size
        && !cli.counts
        && !cli.owner
        && !cli.group
        && !cli.modified
        && !cli.classify
        && !cli.show_targets
        && !cli.absolute
        && !cli.dereference
        && !cli.git
        && !cli.true_size
        && !cli.header
        && !matches!(cli.sort, SortBy::DirCount | SortBy::FileCount)
}

fn fast_type_rank(e: &FastEntry) -> u8 {
    e.type_rank
}

fn collect_fast_entries(
    base_path: &Path,
    show_hidden: bool,
    need_target_dir: bool,
) -> io::Result<Vec<FastEntry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(base_path)? {
        let Ok(dir_entry) = item else {
            continue;
        };
        let file_name = dir_entry.file_name().to_string_lossy().to_string();
        let is_hidden = file_name.starts_with('.');
        if is_hidden && !show_hidden {
            continue;
        }

        let actual_path = dir_entry.path();
        let ft = match dir_entry
            .file_type()
            .or_else(|_| fs::symlink_metadata(&actual_path).map(|m| m.file_type()))
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let is_symlink = ft.is_symlink();
        let is_dir = ft.is_dir();
        let is_target_dir = if is_symlink && need_target_dir {
            fs::metadata(&actual_path)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            is_dir
        };
        entries.push(FastEntry {
            display_name: file_name,
            actual_path,
            is_hidden,
            is_dir,
            is_symlink,
            is_target_dir,
            type_rank: if is_symlink && is_target_dir {
                0
            } else if is_dir {
                1
            } else if is_symlink {
                2
            } else {
                3
            },
        });
    }
    Ok(entries)
}

fn pin_dot_entries_top_fast(entries: &mut Vec<FastEntry>) {
    if entries.is_empty() {
        return;
    }

    let mut pinned: Vec<(u8, FastEntry)> = Vec::new();
    let mut rest: Vec<FastEntry> = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        if let Some(rank) = dot_entry_rank(&entry.display_name) {
            pinned.push((rank, entry));
        } else {
            rest.push(entry);
        }
    }
    pinned.sort_by_key(|(rank, _)| *rank);
    entries.extend(pinned.into_iter().map(|(_, entry)| entry));
    entries.extend(rest);
}

fn paint_name_fast(name: &str, path: &Path, ctx: &Context) -> String {
    let name = escape_terminal_text(name);
    if !ctx.color_enabled {
        return name;
    }
    match ctx.lscolors.style_for_path(path) {
        Some(style) => {
            let ansi_style = Style::to_nu_ansi_term_style(style);
            if ansi_style == nu_ansi_term::Style::default() {
                name
            } else {
                ansi_style.paint(&name).to_string()
            }
        }
        None => name,
    }
}

pub(crate) fn try_render_large_dir_fast_path(
    cli: &Cli,
    ctx: &mut Context,
    input_is_dir: bool,
    pin_dot_entries: bool,
    show_hidden: bool,
    piped_output: bool,
    cache_raw_enabled: bool,
) -> io::Result<Option<String>> {
    if !can_use_large_dir_fast_path(cli, input_is_dir) {
        return Ok(None);
    }

    let mut entries = match collect_fast_entries(
        Path::new(&cli.path),
        show_hidden,
        cli.dirs_only || cli.cache_raw || ctx.sort_by == SortBy::Type,
    ) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };

    if cli.all {
        entries.push(FastEntry {
            display_name: ".".to_string(),
            actual_path: cli.path.clone(),
            is_hidden: false,
            is_dir: true,
            is_symlink: false,
            is_target_dir: true,
            type_rank: 1,
        });
        let parent_path = if cli.path == Path::new(".") {
            PathBuf::from("..")
        } else {
            cli.path.join("..")
        };
        entries.push(FastEntry {
            display_name: "..".to_string(),
            actual_path: parent_path,
            is_hidden: false,
            is_dir: true,
            is_symlink: false,
            is_target_dir: true,
            type_rank: 1,
        });
    }

    if cli.dirs_only {
        entries.retain(|entry| entry.is_dir || entry.is_target_dir);
    }

    let over_auto_limit = entries.len() > AUTO_STYLE_MAX_ENTRIES;
    ctx.color_enabled = output_enabled(cli.color, piped_output, over_auto_limit);
    ctx.hyperlink = output_enabled(cli.hyperlink, piped_output, over_auto_limit);

    entries.sort_by(|a, b| {
        match ctx.sort_by {
            SortBy::Type => {
                let a_rank = fast_type_rank(a);
                let b_rank = fast_type_rank(b);
                if a_rank != b_rank {
                    return a_rank.cmp(&b_rank);
                }
            }
            SortBy::Name => {}
            _ => {}
        }
        if a.is_hidden != b.is_hidden {
            return b.is_hidden.cmp(&a.is_hidden);
        }
        a.display_name.cmp(&b.display_name)
    });
    if ctx.sort_reverse {
        entries.reverse();
    }
    if pin_dot_entries {
        pin_dot_entries_top_fast(&mut entries);
    }

    if cache_raw_enabled {
        let mut dir_paths = Vec::new();
        let mut file_paths = Vec::new();
        for e in &entries {
            let full_path =
                normalize_path_lexical(&to_full_path_with_cwd(&e.actual_path, &ctx.cwd));
            let is_dir = if e.is_symlink {
                e.is_target_dir
            } else {
                e.is_dir
            };
            if is_dir {
                dir_paths.push(full_path);
            } else {
                file_paths.push(full_path);
            }
        }
        write_cache_raw_paths(&dir_paths, &file_paths)?;
    }

    let mut out = String::with_capacity(entries.len().saturating_mul(24));
    for (idx, entry) in entries.iter().enumerate() {
        if piped_output {
            if idx > 0 {
                out.push('\n');
            }
        } else if idx > 0 {
            out.push_str("  ");
        }
        let painted = paint_name_fast(&entry.display_name, &entry.actual_path, ctx);
        if ctx.hyperlink {
            out.push_str(&hyperlink_path(&entry.actual_path, &painted, &ctx.cwd));
        } else {
            out.push_str(&painted);
        }
    }
    out.push('\n');
    Ok(Some(out))
}

fn long_fast_type_rank(e: &LongFastEntry) -> u8 {
    e.type_rank
}

pub(crate) fn format_time_display(mtime: i64, now_year: i32, now_timestamp: i64) -> String {
    let dt: DateTime<Local> = DateTime::from_timestamp(mtime, 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
        .with_timezone(&Local);
    if now_year == dt.year() && (now_timestamp - dt.timestamp()).abs() < 15552000 {
        dt.format("%e %b %H:%M").to_string()
    } else {
        dt.format("%e %b  %Y").to_string()
    }
}

fn make_long_fast_entry(
    display_name: String,
    actual_path: PathBuf,
    metadata: fs::Metadata,
    now_year: i32,
    now_timestamp: i64,
    user_cache: &mut HashMap<u32, String>,
) -> LongFastEntry {
    let is_symlink = metadata.file_type().is_symlink();
    let is_dir = metadata.is_dir();
    let is_hidden = display_name.starts_with('.') && display_name != "." && display_name != "..";
    let mut symlink_target = None;
    let mut target_metadata = None;
    let mut is_target_dir = false;
    let mut broken_symlink = false;
    if is_symlink {
        symlink_target = fs::read_link(&actual_path).ok();
        target_metadata = fs::metadata(&actual_path).ok();
        is_target_dir = target_metadata
            .as_ref()
            .map(|m| m.is_dir())
            .unwrap_or(false);
        broken_symlink = target_metadata.is_none();
    }
    let logical_size = if is_dir {
        on_disk_size(&metadata)
    } else {
        metadata.len()
    };
    let logical_size_str = format_size(logical_size);
    let size_sort_name = display_name.trim_start_matches('.').to_ascii_lowercase();
    let type_rank = if is_symlink && is_target_dir {
        0
    } else if is_dir {
        1
    } else if is_symlink {
        2
    } else {
        3
    };
    let uid = metadata.uid();
    let user_str = user_cache
        .entry(uid)
        .or_insert_with(|| {
            get_user_by_uid(uid)
                .map(|u| u.name().to_string_lossy().into_owned())
                .unwrap_or_else(|| uid.to_string())
        })
        .clone();
    let sort_mtime = metadata.mtime();
    let time_str = format_time_display(sort_mtime, now_year, now_timestamp);

    LongFastEntry {
        display_name,
        actual_path,
        metadata,
        is_hidden,
        is_dir,
        is_symlink,
        is_target_dir,
        type_rank,
        logical_size,
        logical_size_str,
        user_str,
        time_str,
        sort_mtime,
        size_sort_name,
        symlink_target,
        target_metadata,
        broken_symlink,
    }
}

fn collect_long_fast_entries(
    base_path: &Path,
    show_hidden: bool,
    now_year: i32,
    now_timestamp: i64,
    user_cache: &mut HashMap<u32, String>,
) -> io::Result<Vec<LongFastEntry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(base_path)? {
        let Ok(dir_entry) = item else {
            continue;
        };
        let file_name = dir_entry.file_name().to_string_lossy().to_string();
        let is_hidden = file_name.starts_with('.');
        if is_hidden && !show_hidden {
            continue;
        }
        let actual_path = dir_entry.path();
        let metadata = match fs::symlink_metadata(&actual_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        entries.push(make_long_fast_entry(
            file_name,
            actual_path,
            metadata,
            now_year,
            now_timestamp,
            user_cache,
        ));
    }
    Ok(entries)
}

fn pin_dot_entries_top_long_fast(entries: &mut Vec<LongFastEntry>) {
    if entries.is_empty() {
        return;
    }

    let mut pinned: Vec<(u8, LongFastEntry)> = Vec::new();
    let mut rest: Vec<LongFastEntry> = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        if let Some(rank) = dot_entry_rank(&entry.display_name) {
            pinned.push((rank, entry));
        } else {
            rest.push(entry);
        }
    }
    pinned.sort_by_key(|(rank, _)| *rank);
    entries.extend(pinned.into_iter().map(|(_, entry)| entry));
    entries.extend(rest);
}

pub(crate) fn try_render_large_dir_long_fast_path(
    cli: &Cli,
    ctx: &mut Context,
    input_is_dir: bool,
    pin_dot_entries: bool,
    show_hidden: bool,
    piped_output: bool,
    cache_raw_enabled: bool,
) -> io::Result<Option<String>> {
    if !can_use_large_dir_long_fast_path(cli, input_is_dir) {
        return Ok(None);
    }

    let now = Local::now();
    let now_year = now.year();
    let now_timestamp = now.timestamp();
    let mut user_cache = HashMap::<u32, String>::new();
    let mut entries = collect_long_fast_entries(
        Path::new(&cli.path),
        show_hidden,
        now_year,
        now_timestamp,
        &mut user_cache,
    )
    .map_err(|_| io::Error::other("unable to read directory for fast path"))?;

    if cli.all {
        if let Ok(meta) = fs::symlink_metadata(&cli.path) {
            entries.push(make_long_fast_entry(
                ".".to_string(),
                cli.path.clone(),
                meta,
                now_year,
                now_timestamp,
                &mut user_cache,
            ));
        }
        let parent_path = if cli.path == Path::new(".") {
            "..".to_string()
        } else {
            cli.path.join("..").to_string_lossy().into_owned()
        };
        if let Ok(meta) = fs::symlink_metadata(&parent_path) {
            entries.push(make_long_fast_entry(
                "..".to_string(),
                PathBuf::from(parent_path),
                meta,
                now_year,
                now_timestamp,
                &mut user_cache,
            ));
        }
    }

    if cli.dirs_only {
        entries.retain(|entry| entry.is_dir || entry.is_target_dir);
    }

    let over_auto_limit = entries.len() > AUTO_STYLE_MAX_ENTRIES;
    ctx.color_enabled = output_enabled(cli.color, piped_output, over_auto_limit);
    ctx.hyperlink = output_enabled(cli.hyperlink, piped_output, over_auto_limit);

    entries.sort_by(|a, b| {
        match ctx.sort_by {
            SortBy::Size => {
                if a.logical_size != b.logical_size {
                    return b.logical_size.cmp(&a.logical_size);
                }
                return a.size_sort_name.cmp(&b.size_sort_name);
            }
            SortBy::Date => {
                if a.sort_mtime != b.sort_mtime {
                    return b.sort_mtime.cmp(&a.sort_mtime);
                }
            }
            SortBy::Type => {
                let a_rank = long_fast_type_rank(a);
                let b_rank = long_fast_type_rank(b);
                if a_rank != b_rank {
                    return a_rank.cmp(&b_rank);
                }
            }
            SortBy::Name => {}
            _ => {}
        }
        if a.is_hidden != b.is_hidden {
            return b.is_hidden.cmp(&a.is_hidden);
        }
        a.display_name.cmp(&b.display_name)
    });
    if ctx.sort_reverse {
        entries.reverse();
    }
    if pin_dot_entries {
        pin_dot_entries_top_long_fast(&mut entries);
    }

    if cache_raw_enabled {
        let mut dir_paths = Vec::new();
        let mut file_paths = Vec::new();
        for e in &entries {
            let full_path =
                normalize_path_lexical(&to_full_path_with_cwd(&e.actual_path, &ctx.cwd));
            let is_dir = if e.is_symlink {
                e.is_target_dir
            } else {
                e.is_dir
            };
            if is_dir {
                dir_paths.push(full_path);
            } else {
                file_paths.push(full_path);
            }
        }
        write_cache_raw_paths(&dir_paths, &file_paths)?;
    }

    let mut max_size = 0usize;
    let mut max_user = 0usize;
    let mut max_time = 0usize;
    for e in &entries {
        max_size = max_size.max(e.logical_size_str.len());
        max_user = max_user.max(e.user_str.len());
        max_time = max_time.max(e.time_str.len());
    }

    let mut out = String::with_capacity(entries.len().saturating_mul(96));
    for e in &entries {
        let type_char = get_file_type_char(&e.metadata);
        let ft = if type_char == 'd' {
            paint_text_with_lscolors("d", &e.actual_path, &e.metadata, ctx)
        } else if type_char == 'l' {
            paint_if_enabled(
                nu_ansi_term::Color::LightCyan.bold(),
                "l",
                ctx.color_enabled,
            )
        } else {
            paint_if_enabled(
                nu_ansi_term::Color::White.bold(),
                &type_char.to_string(),
                ctx.color_enabled,
            )
        };
        out.push_str(&ft);
        out.push_str(&format_permissions(
            e.metadata.permissions().mode(),
            ctx.color_enabled,
        ));
        out.push(' ');

        let size_text = format!("{:>width$}", e.logical_size_str, width = max_size);
        out.push_str(&paint_if_enabled(
            nu_ansi_term::Color::LightCyan.bold(),
            &size_text,
            ctx.color_enabled,
        ));
        out.push(' ');

        out.push_str(&format!("{:<width$}", e.user_str, width = max_user));
        out.push(' ');

        let time_text = format!("{:<width$}", e.time_str, width = max_time);
        out.push_str(&paint_if_enabled(
            nu_ansi_term::Style::default().dimmed(),
            &time_text,
            ctx.color_enabled,
        ));
        out.push(' ');

        if e.is_symlink && e.broken_symlink {
            let mut broken_text = escape_terminal_text(&e.display_name);
            if ctx.show_targets {
                if let Some(target) = e.symlink_target.as_ref() {
                    broken_text.push_str(" -> ");
                    broken_text.push_str(&escape_terminal_text(&target.to_string_lossy()));
                }
            }
            out.push_str(&highlight_broken_symlink_text(
                &broken_text,
                ctx.color_enabled,
            ));
        } else {
            let safe_name = escape_terminal_text(&e.display_name);
            let painted_name =
                paint_text_with_lscolors(&safe_name, &e.actual_path, &e.metadata, ctx);
            if ctx.hyperlink {
                out.push_str(&hyperlink_path(&e.actual_path, &painted_name, &ctx.cwd));
            } else {
                out.push_str(&painted_name);
            }
            if ctx.show_targets {
                if let Some(target) = e.symlink_target.as_ref() {
                    out.push_str(" -> ");
                    out.push_str(&get_symlink_target_display(
                        &e.actual_path,
                        target,
                        e.target_metadata.as_ref(),
                        ctx,
                    ));
                }
            }
        }

        out.push('\n');
    }

    Ok(Some(out))
}

pub(crate) fn print_detailed_list(
    entries: &[EntryInfo],
    ctx: &Context,
    columns: &[DetailColumn],
) -> String {
    let (
        mut max_size_logical,
        mut max_size_true,
        mut max_dir_count,
        mut max_file_count,
        mut max_user,
        mut max_group,
        mut max_time,
    ) = (0, 0, 0, 0, 0, 0, 0);
    for e in entries {
        max_size_logical = max_size_logical.max(e.logical_size_str.len());
        max_size_true = max_size_true.max(e.true_size_str.len());
        max_dir_count = max_dir_count.max(e.dir_count_str.len());
        max_file_count = max_file_count.max(e.file_count_str.len());
        max_user = max_user.max(e.user_str.len());
        max_group = max_group.max(e.group_str.len());
        max_time = max_time.max(e.time_str.len());
    }
    let mut git_col_width = 0usize;
    if ctx.show_git {
        git_col_width += 2;
    }
    if ctx.show_git_repos {
        if git_col_width > 0 {
            git_col_width += 1;
        }
        git_col_width += 1;
    }
    if ctx.show_git_remote {
        if git_col_width > 0 {
            git_col_width += 1;
        }
        git_col_width += 1;
    }
    if ctx.header {
        max_size_logical = max_size_logical.max("SIZE".len());
        max_size_true = max_size_true.max("TSIZE".len());
        max_dir_count = max_dir_count.max("DIRS".len());
        max_file_count = max_file_count.max("FILES".len());
        max_user = max_user.max("OWNER".len());
        max_group = max_group.max("GROUP".len());
        max_time = max_time.max("MODIFIED".len());
    }

    let mut out = String::with_capacity(entries.len().saturating_mul(96) + 64);
    let header_row = if ctx.header {
        let mut header = String::new();
        for column in columns {
            if !header.is_empty() {
                header.push(' ');
            }
            match column {
                DetailColumn::Perms => {
                    header.push_str(&format!("{:<10}", "PERMS"));
                }
                DetailColumn::SizeLogical => {
                    header.push_str(&format!("{:>width$}", "SIZE", width = max_size_logical));
                }
                DetailColumn::SizeTrue => {
                    header.push_str(&format!("{:>width$}", "TSIZE", width = max_size_true));
                }
                DetailColumn::DirCount => {
                    header.push_str(&format!("{:>width$}", "DIRS", width = max_dir_count));
                }
                DetailColumn::FileCount => {
                    header.push_str(&format!("{:>width$}", "FILES", width = max_file_count));
                }
                DetailColumn::Owner => {
                    header.push_str(&format!("{:<width$}", "OWNER", width = max_user));
                }
                DetailColumn::Time => {
                    header.push_str(&format!("{:<width$}", "MODIFIED", width = max_time));
                }
                DetailColumn::Group => {
                    header.push_str(&format!("{:<width$}", "GROUP", width = max_group));
                }
                DetailColumn::Git => {
                    header.push_str(&format!("{:<width$}", "GIT", width = git_col_width.max(3)));
                }
            }
        }
        if !header.is_empty() {
            header.push(' ');
        }
        header.push_str("NAME");
        paint_if_enabled(
            nu_ansi_term::Style::default().bold(),
            &header,
            ctx.color_enabled,
        )
    } else {
        String::new()
    };

    if ctx.header && !ctx.reverse {
        out.push_str(&header_row);
        out.push('\n');
    }

    for e in entries {
        let mut row = String::new();
        for column in columns {
            if !row.is_empty() {
                row.push(' ');
            }
            match column {
                DetailColumn::Perms => {
                    let type_char = get_file_type_char(&e.metadata);
                    let ft = if type_char == 'd' {
                        paint_text_with_lscolors("d", &e.actual_path, &e.metadata, ctx)
                    } else if type_char == 'l' {
                        paint_if_enabled(
                            nu_ansi_term::Color::LightCyan.bold(),
                            "l",
                            ctx.color_enabled,
                        )
                    } else {
                        paint_if_enabled(
                            nu_ansi_term::Color::White.bold(),
                            &type_char.to_string(),
                            ctx.color_enabled,
                        )
                    };
                    row.push_str(&ft);
                    row.push_str(&format_permissions(
                        e.metadata.permissions().mode(),
                        ctx.color_enabled,
                    ));
                }
                DetailColumn::SizeLogical => {
                    let size_text =
                        format!("{:>width$}", e.logical_size_str, width = max_size_logical);
                    row.push_str(&paint_if_enabled(
                        nu_ansi_term::Color::LightCyan.bold(),
                        &size_text,
                        ctx.color_enabled,
                    ));
                }
                DetailColumn::SizeTrue => {
                    let size_text = format!("{:>width$}", e.true_size_str, width = max_size_true);
                    row.push_str(&paint_if_enabled(
                        nu_ansi_term::Color::LightCyan.bold(),
                        &size_text,
                        ctx.color_enabled,
                    ));
                }
                DetailColumn::DirCount => {
                    let count_text = format!("{:>width$}", e.dir_count_str, width = max_dir_count);
                    row.push_str(&paint_if_enabled(
                        nu_ansi_term::Color::Yellow.bold(),
                        &count_text,
                        ctx.color_enabled,
                    ));
                }
                DetailColumn::FileCount => {
                    let count_text =
                        format!("{:>width$}", e.file_count_str, width = max_file_count);
                    row.push_str(&paint_if_enabled(
                        nu_ansi_term::Color::Yellow.bold(),
                        &count_text,
                        ctx.color_enabled,
                    ));
                }
                DetailColumn::Owner => {
                    row.push_str(&format!("{:<width$}", e.user_str, width = max_user));
                }
                DetailColumn::Time => {
                    let time_text = format!("{:<width$}", e.time_str, width = max_time);
                    row.push_str(&paint_if_enabled(
                        nu_ansi_term::Style::default().dimmed(),
                        &time_text,
                        ctx.color_enabled,
                    ));
                }
                DetailColumn::Group => {
                    row.push_str(&format!("{:<width$}", e.group_str, width = max_group));
                }
                DetailColumn::Git => {
                    let mut git_width = 0usize;
                    if ctx.show_git {
                        let (staged, unstaged) = e.git_status.unwrap_or(('-', '-'));
                        row.push_str(&paint_if_enabled(
                            git_symbol_style(staged),
                            &staged.to_string(),
                            ctx.color_enabled,
                        ));
                        row.push_str(&paint_if_enabled(
                            git_symbol_style(unstaged),
                            &unstaged.to_string(),
                            ctx.color_enabled,
                        ));
                        git_width += 2;
                    }
                    if ctx.show_git_remote {
                        if ctx.show_git {
                            row.push(' ');
                            git_width += 1;
                        }
                        let remote_status = e.repo_remote_status.unwrap_or(' ');
                        row.push_str(&paint_if_enabled(
                            git_remote_status_style(remote_status),
                            &remote_status.to_string(),
                            ctx.color_enabled,
                        ));
                        git_width += 1;
                    }
                    if ctx.show_git_repos {
                        if ctx.show_git || ctx.show_git_remote {
                            row.push(' ');
                            git_width += 1;
                        }
                        let repo_status = e.repo_status.unwrap_or(' ');
                        row.push_str(&paint_if_enabled(
                            git_repo_status_style(repo_status),
                            &repo_status.to_string(),
                            ctx.color_enabled,
                        ));
                        git_width += 1;
                    }
                    for _ in git_width..git_col_width.max(3) {
                        row.push(' ');
                    }
                }
            }
        }
        if !row.is_empty() {
            row.push(' ');
        }
        if e.is_symlink && e.broken_symlink {
            let mut broken_text = get_display_name_text(&e.render_name, &e.metadata, ctx);
            if ctx.show_targets {
                if let Some(target) = e.symlink_target.as_ref() {
                    broken_text.push_str(" -> ");
                    broken_text.push_str(&escape_terminal_text(&target.to_string_lossy()));
                }
            }
            row.push_str(&highlight_broken_symlink_text(
                &broken_text,
                ctx.color_enabled,
            ));
        } else {
            row.push_str(&get_styled_name(
                &e.render_name,
                &e.actual_path,
                &e.metadata,
                ctx,
            ));
            if ctx.show_targets {
                if let Some(target) = e.symlink_target.as_ref() {
                    row.push_str(" -> ");
                    row.push_str(&get_symlink_target_display(
                        &e.actual_path,
                        target,
                        e.target_metadata.as_ref(),
                        ctx,
                    ));
                }
            }
        }
        out.push_str(&row);
        out.push('\n');
    }

    if ctx.header && ctx.reverse {
        out.push_str(&header_row);
        out.push('\n');
    }

    out
}

pub(crate) fn get_styled_name(
    display_name: &str,
    actual_path: &Path,
    metadata: &fs::Metadata,
    ctx: &Context,
) -> String {
    let name = get_display_name_text(display_name, metadata, ctx);
    if ctx.absolute {
        let abs_path = normalize_path_lexical(&to_full_path_with_cwd(actual_path, &ctx.cwd));
        let abs_text = abs_path.to_string_lossy().into_owned();
        let escaped_abs_text = escape_terminal_text(&abs_text);
        let suffix = name.strip_prefix(escaped_abs_text.as_str()).unwrap_or("");
        let (prefix, basename_core) = match escaped_abs_text.rfind('/') {
            Some(idx) if idx + 1 < escaped_abs_text.len() => {
                (&escaped_abs_text[..idx + 1], &escaped_abs_text[idx + 1..])
            }
            _ => ("", escaped_abs_text.as_str()),
        };
        let basename = format!("{}{}", basename_core, suffix);
        let styled_basename = paint_text_with_lscolors(&basename, actual_path, metadata, ctx);
        let styled_prefix = if prefix.is_empty() {
            String::new()
        } else if !ctx.color_enabled {
            prefix.to_string()
        } else {
            format!("\x1b[38;2;255;255;255m{}\x1b[0m", prefix)
        };

        if ctx.hyperlink {
            let mut out = String::new();
            if !styled_prefix.is_empty() {
                let prefix_target = abs_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("/"));
                out.push_str(&hyperlink_path(&prefix_target, &styled_prefix, &ctx.cwd));
            }
            out.push_str(&hyperlink_path(&abs_path, &styled_basename, &ctx.cwd));
            return out;
        }

        return format!("{}{}", styled_prefix, styled_basename);
    }
    let painted = paint_text_with_lscolors(&name, actual_path, metadata, ctx);
    if ctx.hyperlink {
        return hyperlink_path(actual_path, &painted, &ctx.cwd);
    }
    painted
}

fn paint_text_with_lscolors(
    text: &str,
    path: &Path,
    metadata: &fs::Metadata,
    ctx: &Context,
) -> String {
    if !ctx.color_enabled {
        return text.to_string();
    }
    match ctx
        .lscolors
        .style_for_path_with_metadata(path, Some(metadata))
    {
        Some(style) => {
            let ansi_style = Style::to_nu_ansi_term_style(style);
            if ansi_style == nu_ansi_term::Style::default() {
                text.to_string()
            } else {
                ansi_style.paint(text).to_string()
            }
        }
        None => text.to_string(),
    }
}

fn get_classify_suffix(metadata: &fs::Metadata) -> Option<char> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        Some('@')
    } else if file_type.is_dir() {
        Some('/')
    } else if file_type.is_fifo() {
        Some('|')
    } else if file_type.is_socket() {
        Some('=')
    } else if metadata.permissions().mode() & 0o111 != 0 {
        Some('*')
    } else {
        None
    }
}

pub(crate) fn get_display_name_text(
    display_name: &str,
    metadata: &fs::Metadata,
    ctx: &Context,
) -> String {
    let mut name = display_name.to_string();
    if ctx.classify {
        if let Some(suffix) = get_classify_suffix(metadata) {
            name.push(suffix);
        }
    }
    escape_terminal_text(&name)
}

pub(crate) fn get_file_type_char(metadata: &fs::Metadata) -> char {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        'd'
    } else if file_type.is_symlink() {
        'l'
    } else if file_type.is_fifo() {
        'p'
    } else if file_type.is_socket() {
        's'
    } else if file_type.is_block_device() {
        'b'
    } else if file_type.is_char_device() {
        'c'
    } else {
        '-'
    }
}

pub(crate) fn highlight_broken_symlink_text(text: &str, color_enabled: bool) -> String {
    if color_enabled {
        format!("\x1b[48;2;255;0;0m\x1b[38;2;255;255;255m{}\x1b[0m", text)
    } else {
        text.to_string()
    }
}

fn hyperlink_path(path: &Path, text: &str, cwd: &Path) -> String {
    let abs = normalize_path_lexical(&to_full_path_with_cwd(path, cwd));
    let mut encoded = String::with_capacity(abs.as_os_str().len());
    for byte in abs.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{:02X}", byte);
        }
    }
    format!("\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\", encoded, text)
}

fn write_path_list(cache_path: &Path, paths: &[PathBuf]) -> io::Result<()> {
    let mut output = String::with_capacity(paths.len().saturating_mul(64));
    for path in paths {
        output.push_str(&path.to_string_lossy());
        output.push('\n');
    }
    fs::write(cache_path, output)
}

fn cache_pid_suffix() -> u32 {
    if let Some(value) = std::env::var_os("fish_pid") {
        if let Some(text) = value.to_str() {
            if let Ok(pid) = text.parse::<u32>() {
                return pid;
            }
        }
    }

    if let Ok(stat) = fs::read_to_string("/proc/self/stat") {
        if let Some((_, after_comm)) = stat.rsplit_once(") ") {
            let mut fields = after_comm.split_whitespace();
            let _state = fields.next();
            if let Some(ppid_field) = fields.next() {
                if let Ok(ppid) = ppid_field.parse::<u32>() {
                    return ppid;
                }
            }
        }
    }

    std::process::id()
}

pub(crate) fn write_cache_raw_paths(
    dir_paths: &[PathBuf],
    file_paths: &[PathBuf],
) -> io::Result<()> {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let cache_dir = PathBuf::from("/tmp").join(format!("fzf-history-{}", user));
    let pid = cache_pid_suffix();
    fs::create_dir_all(&cache_dir)?;
    write_path_list(
        &cache_dir.join(format!("universal-last-dirs-{}", pid)),
        dir_paths,
    )?;
    write_path_list(
        &cache_dir.join(format!("universal-last-files-{}", pid)),
        file_paths,
    )
}

pub(crate) fn get_symlink_target_display(
    link_path: &Path,
    target: &Path,
    target_metadata: Option<&fs::Metadata>,
    ctx: &Context,
) -> String {
    let mut display_text = escape_terminal_text(&target.to_string_lossy());
    let resolved_target = if target.is_absolute() {
        Some(target.to_path_buf())
    } else {
        link_path.parent().map(|p| p.join(target))
    };

    if let Some(path) = resolved_target.as_deref() {
        if ctx.classify {
            if let Some(m) = target_metadata {
                if let Some(suffix) = get_classify_suffix(m) {
                    if !display_text.ends_with(suffix) {
                        display_text.push(suffix);
                    }
                }
            }
        }

        let (split_text, trailing_suffix) = if display_text.len() > 1 && ctx.classify {
            let last_char = display_text.chars().last().unwrap();
            if ['/', '*', '@', '|', '='].contains(&last_char) {
                (
                    &display_text[..display_text.len() - 1],
                    &display_text[display_text.len() - 1..],
                )
            } else {
                (display_text.as_str(), "")
            }
        } else {
            (display_text.as_str(), "")
        };

        let (prefix, basename) = match split_text.rfind('/') {
            Some(idx) if idx + 1 < split_text.len() => {
                (&split_text[..idx + 1], &split_text[idx + 1..])
            }
            _ => ("", split_text),
        };
        let basename_with_suffix = format!("{}{}", basename, trailing_suffix);
        let target_style = ctx
            .lscolors
            .style_for_path_with_metadata(path, target_metadata)
            .or_else(|| ctx.lscolors.style_for_path(path));
        let styled_basename = match target_style {
            Some(style) => {
                let ansi_style = Style::to_nu_ansi_term_style(style);
                if !ctx.color_enabled || ansi_style == nu_ansi_term::Style::default() {
                    basename_with_suffix
                } else {
                    ansi_style.paint(basename_with_suffix).to_string()
                }
            }
            None => basename_with_suffix,
        };

        let styled_prefix = if prefix.is_empty() {
            String::new()
        } else if !ctx.color_enabled {
            prefix.to_string()
        } else {
            format!("\x1b[38;2;255;255;255m{}\x1b[0m", prefix)
        };

        if ctx.hyperlink {
            let mut out = String::new();
            if !styled_prefix.is_empty() {
                let prefix_target = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("/"));
                out.push_str(&hyperlink_path(&prefix_target, &styled_prefix, &ctx.cwd));
            }
            out.push_str(&hyperlink_path(path, &styled_basename, &ctx.cwd));
            return out;
        }

        if styled_prefix.is_empty() {
            styled_basename
        } else {
            format!("{}{}", styled_prefix, styled_basename)
        }
    } else {
        display_text
    }
}

pub(crate) fn format_permissions(mode: u32, color_enabled: bool) -> String {
    if !color_enabled {
        let mut plain = String::new();
        for (mask, special_mask, normal) in [
            (0o400, 0, 'r'),
            (0o200, 0, 'w'),
            (0o100, 0o4000, 'x'),
            (0o040, 0, 'r'),
            (0o020, 0, 'w'),
            (0o010, 0o2000, 'x'),
            (0o004, 0, 'r'),
            (0o002, 0, 'w'),
            (0o001, 0o1000, 'x'),
        ] {
            let special = match (special_mask, normal) {
                (0o4000, 'x') => Some(if mode & mask != 0 { 's' } else { 'S' }),
                (0o2000, 'x') => Some(if mode & mask != 0 { 's' } else { 'S' }),
                (0o1000, 'x') => Some(if mode & mask != 0 { 't' } else { 'T' }),
                _ => None,
            };
            plain.push(if special_mask != 0 && mode & special_mask != 0 {
                special.unwrap()
            } else if mode & mask != 0 {
                normal
            } else {
                '-'
            });
        }
        return plain;
    }

    let p = [
        (0o400, 0, 'r', nu_ansi_term::Color::LightYellow.bold()),
        (0o200, 0, 'w', nu_ansi_term::Color::LightRed.bold()),
        (0o100, 0o4000, 'x', nu_ansi_term::Color::LightGreen.bold()),
        // Keep group/other permissions in the same color families, but dimmer.
        (
            0o040,
            0,
            'r',
            nu_ansi_term::Color::Rgb(180, 180, 120).normal(),
        ),
        (
            0o020,
            0,
            'w',
            nu_ansi_term::Color::Rgb(190, 120, 120).normal(),
        ),
        (
            0o010,
            0o2000,
            'x',
            nu_ansi_term::Color::Rgb(120, 180, 120).normal(),
        ),
        (
            0o004,
            0,
            'r',
            nu_ansi_term::Color::Rgb(180, 180, 120).normal(),
        ),
        (
            0o002,
            0,
            'w',
            nu_ansi_term::Color::Rgb(190, 120, 120).normal(),
        ),
        (
            0o001,
            0o1000,
            'x',
            nu_ansi_term::Color::Rgb(120, 180, 120).normal(),
        ),
    ];
    let mut s = String::new();
    let dash = nu_ansi_term::Color::Fixed(236).paint("-");
    for (mask, special_mask, normal, style) in p.iter() {
        let special = match (*special_mask, *normal) {
            (0o4000, 'x') => Some(if mode & mask != 0 { 's' } else { 'S' }),
            (0o2000, 'x') => Some(if mode & mask != 0 { 's' } else { 'S' }),
            (0o1000, 'x') => Some(if mode & mask != 0 { 't' } else { 'T' }),
            _ => None,
        };
        if *special_mask != 0 && mode & special_mask != 0 {
            s.push_str(&style.paint(special.unwrap().to_string()).to_string());
        } else if mode & mask != 0 {
            s.push_str(&style.paint(normal.to_string()).to_string());
        } else {
            s.push_str(&dash.to_string());
        }
    }
    s
}

pub(crate) fn format_size(size: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if size >= GB {
        format!("{:.1}G", size as f64 / GB as f64)
    } else if size >= MB {
        format!("{:.1}M", size as f64 / MB as f64)
    } else if size >= KB {
        format!("{:.1}K", size as f64 / KB as f64)
    } else {
        size.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_path_normalization_removes_redundant_components() {
        assert_eq!(
            normalize_path_lexical(Path::new("/home/lewis/./.xbindkeys")),
            PathBuf::from("/home/lewis/.xbindkeys")
        );
    }

    #[test]
    fn size_formatting_preserves_small_sizes_and_units() {
        assert_eq!(format_size(0), "0");
        assert_eq!(format_size(4096), "4.0K");
        assert_eq!(format_size(1024 * 1024), "1.0M");
    }

    #[test]
    fn terminal_controls_are_escaped_before_rendering() {
        assert_eq!(escape_terminal_text("a\n\tb\x7f"), "a\\n\\tb\\x7f");
    }

    #[test]
    fn plain_permissions_preserve_special_bits() {
        assert_eq!(format_permissions(0o4755, false), "rwsr-xr-x");
        assert_eq!(format_permissions(0o1777, false), "rwxrwxrwt");
    }
}
