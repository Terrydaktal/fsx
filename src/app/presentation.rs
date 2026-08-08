use super::filesystem::{
    get_dir_bytes_native_serial, get_dir_stats_native, normalize_dir_key,
    should_skip_root_size_tree,
};
use super::model::{
    system_time_to_unix_nanos, ColorSpec, DirStats, DirStatsCache, HighlightSpec, Options,
    RawCacheState, SearchResult, SortField, SortOrder,
};
use chrono::{DateTime, Local};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) fn style_enabled(opts: &Options, stdout_is_tty: bool) -> bool {
    match opts.color_when {
        super::model::ColorWhen::Auto => stdout_is_tty,
        super::model::ColorWhen::Always => true,
        super::model::ColorWhen::Never => false,
    }
}

pub(crate) fn escape_terminal_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => {
                escaped.push_str(&format!("\\x{:02x}", ch as u32));
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) fn can_stream_direct(opts: &Options, use_style: bool) -> bool {
    !use_style
        && !opts.classify
        && !opts.force_full
        && !opts.highlight_match
        && !opts.counts
        && opts.sort_field.is_none()
        && !opts.long_format
        && !opts.sizes
        && !opts.reverse
        && !opts.snapshot_cache
        && !opts.snapshot_refresh
        && !opts.absolute_paths
        && !opts.hyperlinks
}
pub(crate) fn format_size_iec(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0usize;
    let mut size = bytes as f64;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, units[unit])
    } else if size >= 100.0 {
        format!("{:.0} {}", size, units[unit])
    } else if size >= 10.0 {
        format!("{:.1} {}", size, units[unit])
    } else {
        format!("{:.2} {}", size, units[unit])
    }
}

pub(crate) fn format_size_compact_3(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut unit = 0usize;
    let mut size = bytes as f64;
    while (size >= 1024.0 || (unit == 0 && size > 99_999.0)) && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    for decimals in (0usize..=3).rev() {
        let factor = 10f64.powi(decimals as i32);
        let truncated = (size * factor).floor() / factor;
        let candidate = format!("{:.*}{}", decimals, truncated, units[unit]);
        if candidate.len() <= 6 {
            return candidate;
        }
    }
    if unit < units.len() - 1 {
        return format_size_compact_3(bytes / 1024);
    }
    "99999T".to_string()
}

pub(crate) fn should_use_recursive_dirsize(item: &SearchResult, opts: &Options) -> bool {
    if !item.is_dir || item.is_symlink {
        return false;
    }
    opts.sizes || opts.long_extended || !opts.no_recurse
}

pub(crate) fn size_bytes_for_result(
    item: &SearchResult,
    opts: &Options,
    cache: &mut DirStatsCache,
) -> u64 {
    if should_use_recursive_dirsize(item, opts) {
        if should_skip_root_size_tree(&item.path) {
            return 0;
        }
        return get_dirsize_bytes(&item.path, cache).unwrap_or(0);
    }
    item.metadata
        .as_ref()
        .map(|m| m.len())
        .or(item.indexed_size)
        .unwrap_or(0)
}

pub(crate) fn result_activity_nanos(item: &SearchResult) -> Option<i64> {
    item.indexed_activity_nanos.or_else(|| {
        item.metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(system_time_to_unix_nanos)
    })
}

pub(crate) fn result_activity_time(item: &SearchResult) -> Option<std::time::SystemTime> {
    let nanos = u64::try_from(result_activity_nanos(item)?).ok()?;
    std::time::UNIX_EPOCH.checked_add(Duration::from_nanos(nanos))
}

pub(crate) fn precompute_dirsize_cache(
    items: &[SearchResult],
    opts: &Options,
    cache: &mut DirStatsCache,
) {
    let need_recursive = opts.sizes
        || opts.long_extended
        || matches!(opts.sort_field, Some(SortField::Size) if !opts.no_recurse);
    if !need_recursive {
        return;
    }
    let mut needed_dirs: Vec<String> = items
        .iter()
        .filter(|item| should_use_recursive_dirsize(item, opts))
        .map(|item| normalize_dir_key(&item.path))
        .collect();
    needed_dirs.sort();
    needed_dirs.dedup();
    if opts.long_extended {
        for dir in needed_dirs {
            if should_skip_root_size_tree(&dir) {
                continue;
            }
            let _ = get_dirsize_stats(&dir, cache);
        }
    } else {
        let mut missing = Vec::new();
        for dir in needed_dirs {
            if should_skip_root_size_tree(&dir) {
                continue;
            }
            if !cache.bytes_map.contains_key(&dir) && !cache.map.contains_key(&dir) {
                missing.push(dir);
            }
        }
        let computed: Vec<(String, u64)> = missing
            .into_par_iter()
            .map(|dir| {
                let bytes = get_dir_bytes_native_serial(&dir);
                (dir, bytes)
            })
            .collect();
        for (dir, bytes) in computed {
            cache.bytes_map.insert(dir, bytes);
        }
    }
}

pub(crate) fn sort_results(
    mut items: Vec<SearchResult>,
    opts: &Options,
    cache: &mut DirStatsCache,
) -> Vec<SearchResult> {
    let Some(field) = opts.sort_field else {
        if let Some(limit) = opts.limit {
            items.truncate(limit);
        }
        return items;
    };
    let order = opts.sort_order.unwrap_or(SortOrder::Asc);
    if field == SortField::Name {
        let mut keyed = items
            .into_iter()
            .map(|item| (item.path.to_lowercase(), item))
            .collect::<Vec<_>>();
        let compare = |a: &(String, SearchResult), b: &(String, SearchResult)| {
            let ordered = match order {
                SortOrder::Asc => a.0.cmp(&b.0),
                SortOrder::Desc => b.0.cmp(&a.0),
            };
            ordered.then_with(|| a.1.path.cmp(&b.1.path))
        };
        if let Some(limit) = opts.limit {
            if limit < keyed.len() {
                keyed.select_nth_unstable_by(limit, &compare);
                keyed.truncate(limit);
            }
        }
        keyed.sort_by(compare);
        return keyed.into_iter().map(|(_, item)| item).collect();
    }
    let mut compare = |a: &SearchResult, b: &SearchResult| {
        let ord = match field {
            SortField::Date => {
                let da = result_activity_nanos(a).unwrap_or(0);
                let db = result_activity_nanos(b).unwrap_or(0);
                da.cmp(&db)
            }
            SortField::Size => {
                let sa = size_bytes_for_result(a, opts, cache);
                let sb = size_bytes_for_result(b, opts, cache);
                sa.cmp(&sb)
            }
            SortField::Name => unreachable!("name sorting is handled above"),
        };
        let ordered = match order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        };
        ordered.then_with(|| a.path.cmp(&b.path))
    };
    if let Some(limit) = opts.limit {
        if limit < items.len() {
            items.select_nth_unstable_by(limit, &mut compare);
            items.truncate(limit);
        }
    }
    items.sort_by(&mut compare);
    items
}

pub(crate) fn absolute_paths_transform(
    mut items: Vec<SearchResult>,
    opts: &Options,
) -> Vec<SearchResult> {
    if !opts.absolute_paths {
        return items;
    }
    let cwd_abs = env::current_dir()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok())
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cwd_abs_str = cwd_abs.to_string_lossy().to_string();
    for item in items.iter_mut() {
        if !item.path.starts_with('/') {
            item.path = format!("{}/{}", cwd_abs_str, item.path.trim_start_matches("./"));
        }
    }
    items
}

pub(crate) fn parent_pid() -> Option<u32> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let (_, tail) = stat.rsplit_once(") ")?;
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    Some(ppid)
}

pub(crate) fn fish_pid() -> String {
    if let Ok(v) = env::var("FISH_PID") {
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
            return v;
        }
    }
    if let Ok(v) = env::var("fish_pid") {
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
            return v;
        }
    }
    if let Some(ppid) = parent_pid() {
        return ppid.to_string();
    }
    std::process::id().to_string()
}

pub(crate) fn init_raw_cache_state() -> Option<RawCacheState> {
    let user = env::var("USER")
        .unwrap_or_else(|_| "unknown".to_string())
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let pid = fish_pid();
    let cache_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .map(|path| path.join("unearth"))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/unearth-raw-{}", user)));
    fs::create_dir_all(&cache_dir).ok()?;
    let _ = fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700));
    let dirs_file = cache_dir.join(format!("universal-last-dirs-{}", pid));
    let files_file = cache_dir.join(format!("universal-last-files-{}", pid));
    const O_NOFOLLOW: i32 = 0x20000;
    let open = |path: &PathBuf| {
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(path)
            .ok()
    };
    let dirs = BufWriter::new(open(&dirs_file)?);
    let files = BufWriter::new(open(&files_file)?);
    Some(RawCacheState {
        dirs,
        files,
        seen_dirs: HashSet::new(),
        seen_files: HashSet::new(),
    })
}

pub(crate) fn cache_raw_record_path(path: &str, is_dir: bool, state: &mut RawCacheState) {
    if is_dir {
        let mut p = path.to_string();
        if !p.ends_with('/') {
            p.push('/');
        }
        if state.seen_dirs.insert(p.clone()) {
            let _ = writeln!(state.dirs, "{}", p);
        }
    } else if state.seen_files.insert(path.to_string()) {
        let _ = writeln!(state.files, "{}", path);
    }
    let mut parent = path.trim_end_matches('/').to_string();
    if let Some(idx) = parent.rfind('/') {
        parent = parent[..idx].to_string();
        if parent.is_empty() {
            parent = "/".to_string();
        } else if !parent.ends_with('/') {
            parent.push('/');
        }
    } else {
        parent = "./".to_string();
    }
    if state.seen_dirs.insert(parent.clone()) {
        let _ = writeln!(state.dirs, "{}", parent);
    }
}

pub(crate) fn cache_transform(items: &Vec<SearchResult>, opts: &Options) {
    if !opts.cache_output {
        return;
    }
    let Some(mut state) = init_raw_cache_state() else {
        return;
    };
    for item in items {
        cache_raw_record_path(&item.path, item.is_dir, &mut state);
    }
    let _ = state.dirs.flush();
    let _ = state.files.flush();
}

pub(crate) fn get_dirsize_stats(path: &str, cache: &mut DirStatsCache) -> Option<DirStats> {
    let key = normalize_dir_key(path);
    let walk_path = if key == "/" { "/" } else { key.as_str() };
    if let Some(v) = cache.map.get(&key) {
        return Some(v.clone());
    }
    let (bytes, files) = get_dir_stats_native(walk_path, true);
    let stats = DirStats {
        files,
        bytes,
        human: format_size_iec(bytes),
    };
    cache.bytes_map.insert(key.clone(), bytes);
    cache.map.insert(key, stats.clone());
    Some(stats)
}

pub(crate) fn get_dirsize_bytes(path: &str, cache: &mut DirStatsCache) -> Option<u64> {
    let key = normalize_dir_key(path);
    let walk_path = if key == "/" { "/" } else { key.as_str() };
    if let Some(v) = cache.bytes_map.get(&key) {
        return Some(*v);
    }
    if let Some(v) = cache.map.get(&key) {
        cache.bytes_map.insert(key.clone(), v.bytes);
        return Some(v.bytes);
    }
    let (bytes, _) = get_dir_stats_native(walk_path, false);
    cache.bytes_map.insert(key, bytes);
    Some(bytes)
}

pub(crate) fn add_info_transform(
    items: Vec<SearchResult>,
    opts: &Options,
    cache: &mut DirStatsCache,
    use_style: bool,
    add_decorator: bool,
    colors: &ColorSpec,
    highlight: Option<&HighlightSpec>,
) -> Vec<String> {
    if !opts.long_format {
        return items.into_iter().map(|i| i.path).collect();
    }
    let mut rows = Vec::with_capacity(items.len());
    let mut max_size_width = 0usize;
    for item in items {
        if let Some(activity_time) = result_activity_time(&item) {
            let dt: DateTime<Local> = activity_time.into();
            let dt_str = dt.format("%Y-%m-%d %H:%M:%S").to_string();
            let mut human_size = format_size_iec(
                item.metadata
                    .as_ref()
                    .map(|metadata| metadata.len())
                    .or(item.indexed_size)
                    .unwrap_or(0),
            );
            let mut extra = String::new();
            if opts.long_extended {
                if item.is_symlink {
                    let link_path = item.path.trim_end_matches('/');
                    if fs::metadata(link_path).map(|m| m.is_dir()).unwrap_or(false) {
                        extra = " 0".to_string();
                    }
                } else if item.is_dir {
                    if let Some(stats) = get_dirsize_stats(&item.path, cache) {
                        human_size = stats.human;
                        extra = format!(" {}", stats.files);
                    }
                }
            }
            let path_display =
                render_styled_path(&item, use_style, add_decorator, colors, opts, highlight);
            max_size_width = max_size_width.max(human_size.len());
            rows.push((Some((dt_str, human_size, extra)), path_display));
        } else {
            let path_display =
                render_styled_path(&item, use_style, add_decorator, colors, opts, highlight);
            rows.push((None, path_display));
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for (info, path_display) in rows {
        if let Some((dt_str, human_size, extra)) = info {
            let padded_size = format!("{:>width$}", human_size, width = max_size_width);
            let size_display = style_size(&padded_size, use_style);
            out.push(format!(
                "{} {}{} {}",
                dt_str, size_display, extra, path_display
            ));
        } else {
            out.push(path_display);
        }
    }
    out
}

pub(crate) fn sizes_transform(
    items: Vec<SearchResult>,
    opts: &Options,
    cache: &mut DirStatsCache,
    use_style: bool,
    add_decorator: bool,
    colors: &ColorSpec,
    highlight: Option<&HighlightSpec>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let compact = if should_skip_root_size_tree(&item.path) {
            "-".to_string()
        } else {
            let bytes = size_bytes_for_result(&item, opts, cache);
            format_size_compact_3(bytes)
        };
        let path_display =
            render_styled_path(&item, use_style, add_decorator, colors, opts, highlight);
        out.push(format!(
            "{}\t{}",
            style_size(&compact, use_style),
            path_display
        ));
    }
    out
}

pub(crate) fn style_size(size: &str, use_style: bool) -> String {
    if use_style {
        format!("\x1b[1;96m{}\x1b[0m", size)
    } else {
        size.to_string()
    }
}

pub(crate) fn counts_summary_transform(
    items: Vec<SearchResult>,
    show_header: bool,
    use_style: bool,
    colors: &ColorSpec,
    opts: &Options,
    highlight: Option<&HighlightSpec>,
) -> Vec<String> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for item in items {
        let mut p = item.path.trim_end_matches('/').to_string();
        let d = if let Some(idx) = p.rfind('/') {
            p.truncate(idx);
            if p.is_empty() {
                "/".to_string()
            } else {
                p
            }
        } else {
            ".".to_string()
        };
        *counts.entry(d).or_insert(0) += 1;
    }
    let mut rows: Vec<(String, u64)> = counts.into_iter().collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let mut out = Vec::new();
    if show_header {
        out.push(format!("{:>7}  {}", "COUNT", "FOLDER"));
    }
    for (folder, n) in rows {
        let folder_item = SearchResult {
            path: folder,
            is_dir: true,
            is_symlink: false,
            metadata: None,
            indexed_activity_nanos: None,
            indexed_size: None,
        };
        let folder_display =
            render_styled_path(&folder_item, use_style, false, colors, opts, highlight);
        let count_display = style_size(&format!("{:>7}", n), use_style);
        out.push(format!("{}  {}", count_display, folder_display));
    }
    out
}

pub(crate) fn parse_ls_colors() -> ColorSpec {
    let mut by_key = HashMap::new();
    let mut globs = Vec::new();
    let (mut color_dir, mut color_link, mut color_exec) = (
        "01;34".to_string(),
        "01;36".to_string(),
        "01;32".to_string(),
    );
    if let Ok(spec) = env::var("LS_COLORS") {
        for entry in spec.split(':') {
            if let Some((k, v)) = entry.split_once('=') {
                if k.starts_with('*') {
                    let mut rx = String::from("^");
                    for ch in k.chars() {
                        match ch {
                            '*' => rx.push_str(".*"),
                            '?' => rx.push('.'),
                            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|'
                            | '\\' => {
                                rx.push('\\');
                                rx.push(ch);
                            }
                            _ => rx.push(ch),
                        }
                    }
                    rx.push('$');
                    if let Ok(re) = Regex::new(&rx) {
                        globs.push((re, v.to_string()));
                    }
                } else {
                    by_key.insert(k.to_string(), v.to_string());
                    match k {
                        "di" => color_dir = v.to_string(),
                        "ln" => color_link = v.to_string(),
                        "ex" => color_exec = v.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    ColorSpec {
        by_key,
        globs,
        color_prefix_dir: "38;2;255;255;255".to_string(),
        color_dir,
        color_link,
        color_exec,
    }
}

pub(crate) fn default_color_spec() -> ColorSpec {
    ColorSpec {
        by_key: HashMap::new(),
        globs: Vec::new(),
        color_prefix_dir: "38;2;255;255;255".to_string(),
        color_dir: "01;34".to_string(),
        color_link: "01;36".to_string(),
        color_exec: "01;32".to_string(),
    }
}

pub(crate) fn color_code_for_path(res: &SearchResult, colors: &ColorSpec) -> String {
    let ln_code = colors
        .by_key
        .get("ln")
        .cloned()
        .unwrap_or_else(|| colors.color_link.clone());
    let symlink_target_mode = res.is_symlink && ln_code == "target";
    if res.is_symlink && !symlink_target_mode {
        return ln_code;
    }
    if res.is_dir
        || (symlink_target_mode && res.metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false))
    {
        return colors
            .by_key
            .get("di")
            .cloned()
            .unwrap_or_else(|| colors.color_dir.clone());
    }
    let base = res.path.rsplit('/').next().unwrap_or("");
    for (re, val) in &colors.globs {
        if re.is_match(base) {
            return val.clone();
        }
    }
    if let Some(m) = &res.metadata {
        if m.permissions().mode() & 0o111 != 0 {
            return colors.color_exec.clone();
        }
    }
    String::new()
}

pub(crate) fn decorator_for_res(res: &SearchResult) -> Option<char> {
    if res.is_symlink {
        return Some('@');
    }
    if res.is_dir {
        return if res.path.ends_with('/') {
            None
        } else {
            Some('/')
        };
    }
    if let Some(m) = &res.metadata {
        let ft = m.file_type();
        if ft.is_fifo() {
            return Some('|');
        }
        if ft.is_socket() {
            return Some('=');
        }
        if m.permissions().mode() & 0o111 != 0 {
            return Some('*');
        }
    }
    None
}

pub(crate) const MATCH_HIGHLIGHT_CODE: &str = "1;91";

pub(crate) fn compile_highlight_spec(patterns: &[(String, bool)]) -> Result<HighlightSpec, String> {
    let mut prefix_rules = Vec::new();
    let mut leaf_rules = Vec::new();
    for (raw, full_path_match) in patterns {
        let re = RegexBuilder::new(raw)
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("Invalid regex: {}", e))?;
        if *full_path_match {
            prefix_rules.push(re.clone());
        }
        leaf_rules.push(re);
    }
    Ok(HighlightSpec {
        prefix_rules,
        leaf_rules,
    })
}

pub(crate) fn merge_match_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut merged = Vec::with_capacity(ranges.len());
    let mut current = ranges[0];
    for (start, end) in ranges.into_iter().skip(1) {
        if start <= current.1 {
            current.1 = current.1.max(end);
        } else {
            merged.push(current);
            current = (start, end);
        }
    }
    merged.push(current);
    merged
}

pub(crate) fn colorize_segment_with_highlights(
    text: &str,
    base_code: Option<&str>,
    rules: &[Regex],
) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut ranges = Vec::new();
    for re in rules {
        for m in re.find_iter(text) {
            ranges.push((m.start(), m.end()));
        }
    }
    let ranges = merge_match_ranges(ranges);
    if ranges.is_empty() {
        return match base_code {
            Some(code) => format!("\x1b[{}m{}\x1b[0m", code, text),
            None => text.to_string(),
        };
    }
    let mut out = String::with_capacity(text.len() + ranges.len() * 16);
    if let Some(code) = base_code {
        out.push_str("\x1b[");
        out.push_str(code);
        out.push('m');
    }
    let mut cursor = 0;
    for (start, end) in ranges {
        if start > cursor {
            out.push_str(&text[cursor..start]);
        }
        out.push_str("\x1b[");
        out.push_str(MATCH_HIGHLIGHT_CODE);
        out.push('m');
        out.push_str(&text[start..end]);
        out.push_str("\x1b[0m");
        if let Some(code) = base_code {
            out.push_str("\x1b[");
            out.push_str(code);
            out.push('m');
        }
        cursor = end;
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out.push_str("\x1b[0m");
    out
}

pub(crate) fn encode_file_uri_path(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

pub(crate) fn parent_file_uri(prefix: &str, leaf_path: &str) -> String {
    let encoded_prefix = encode_file_uri_path(prefix);
    format!(
        "file://{}?select={}",
        encoded_prefix,
        encode_file_uri_path(leaf_path)
    )
}

pub(crate) fn render_styled_path(
    res: &SearchResult,
    use_style: bool,
    add_decorator: bool,
    colors: &ColorSpec,
    opts: &Options,
    highlight: Option<&HighlightSpec>,
) -> String {
    let mut display_path = escape_terminal_text(&res.path);
    if add_decorator {
        if let Some(d) = decorator_for_res(res) {
            if !display_path.ends_with(d) {
                display_path.push(d);
            }
        }
    }
    let (prefix, leaf) = if display_path.ends_with('/') {
        let core = display_path.trim_end_matches('/');
        if let Some((p, l)) = core.rsplit_once('/') {
            (format!("{}/", p), format!("{}/", l))
        } else {
            (String::new(), display_path.clone())
        }
    } else if let Some((p, l)) = display_path.rsplit_once('/') {
        (format!("{}/", p), l.to_string())
    } else {
        (String::new(), display_path.clone())
    };
    if !use_style {
        let (plain_prefix, plain_leaf) = if let Some(spec) = highlight {
            (
                colorize_segment_with_highlights(&prefix, None, &spec.prefix_rules),
                colorize_segment_with_highlights(&leaf, None, &spec.leaf_rules),
            )
        } else {
            (prefix.clone(), leaf.clone())
        };
        if opts.hyperlinks {
            return hyperlink_path(&plain_prefix, &plain_leaf, &res.path);
        }
        return format!("{}{}", plain_prefix, plain_leaf);
    }
    let leaf_code = color_code_for_path(res, colors);
    let leaf_colored = if let Some(spec) = highlight {
        colorize_segment_with_highlights(
            &leaf,
            if leaf_code.is_empty() {
                None
            } else {
                Some(leaf_code.as_str())
            },
            &spec.leaf_rules,
        )
    } else if leaf_code.is_empty() {
        leaf.clone()
    } else {
        format!("\x1b[{}m{}\x1b[0m", leaf_code, leaf)
    };
    let prefix_colored = if prefix.is_empty() {
        String::new()
    } else if let Some(spec) = highlight {
        colorize_segment_with_highlights(
            &prefix,
            Some(colors.color_prefix_dir.as_str()),
            &spec.prefix_rules,
        )
    } else {
        format!("\x1b[{}m{}\x1b[0m", colors.color_prefix_dir, prefix)
    };
    let mut final_str = if prefix.is_empty() {
        leaf_colored.clone()
    } else {
        format!("{}{}", prefix_colored, leaf_colored)
    };
    if opts.hyperlinks {
        final_str = hyperlink_path(&prefix_colored, &leaf_colored, &res.path);
    }
    final_str
}

fn hyperlink_path(prefix: &str, leaf: &str, path: &str) -> String {
    let mut abs_leaf = path.to_string();
    if !abs_leaf.starts_with('/') {
        if let Ok(cwd) = env::current_dir() {
            abs_leaf = format!("{}/{}", cwd.display(), abs_leaf.trim_start_matches("./"));
        }
    }
    let raw_leaf = abs_leaf.trim_end_matches('/');
    let raw_parent = Path::new(raw_leaf)
        .parent()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    let raw_prefix = if raw_parent == "/" {
        "/".to_string()
    } else {
        format!("{}/", raw_parent.trim_end_matches('/'))
    };
    if prefix.is_empty() {
        format!(
            "\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\",
            encode_file_uri_path(&abs_leaf),
            leaf,
        )
    } else {
        let encoded_leaf = encode_file_uri_path(&abs_leaf);
        let prefix_target = parent_file_uri(&raw_prefix, &abs_leaf);
        format!(
            "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\",
            prefix_target, prefix, encoded_leaf, leaf
        )
    }
}

pub(crate) fn final_transform(
    items: Vec<SearchResult>,
    opts: &Options,
    use_style: bool,
    stdout_is_tty: bool,
    colors: &ColorSpec,
    cache: &mut DirStatsCache,
    highlight: Option<&HighlightSpec>,
) -> Vec<String> {
    let items = absolute_paths_transform(items, opts);
    cache_transform(&items, opts);
    let add_decorators = stdout_is_tty || opts.classify;
    if opts.counts {
        let mut lines =
            counts_summary_transform(items, stdout_is_tty, use_style, colors, opts, highlight);
        if opts.reverse {
            lines.reverse();
        }
        return lines;
    }
    let items = if opts.sort_field.is_none() {
        if let Some(limit) = opts.limit {
            items.into_iter().take(limit).collect()
        } else {
            items
        }
    } else {
        items
    };
    precompute_dirsize_cache(&items, opts, cache);
    let mut items = sort_results(items, opts, cache);
    if opts.reverse {
        items.reverse();
    }
    if opts.sizes {
        return sizes_transform(
            items,
            opts,
            cache,
            use_style,
            add_decorators,
            colors,
            highlight,
        );
    }
    let mut out = Vec::new();
    if opts.long_format {
        for item_str in add_info_transform(
            items,
            opts,
            cache,
            use_style,
            add_decorators,
            colors,
            highlight,
        ) {
            out.push(item_str);
        }
    } else {
        for res in items {
            out.push(render_styled_path(
                &res,
                use_style,
                add_decorators,
                colors,
                opts,
                highlight,
            ));
        }
    }
    out
}
