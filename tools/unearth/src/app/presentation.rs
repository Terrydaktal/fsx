use super::filesystem::{
    get_dir_bytes_native_serial, get_dir_stats_native, normalize_dir_key,
    should_skip_root_size_tree,
};
use super::model::{
    system_time_to_unix_nanos, ColorSpec, DirStats, DirStatsCache, HighlightSpec, Options,
    RawCacheState, SearchResult, SortField, SortOrder,
};
use chrono::{Datelike, Local};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

fn result_path(path: &str, encoded: bool) -> PathBuf {
    if encoded {
        fsx::decode_lossless_path(path)
    } else {
        PathBuf::from(path)
    }
}

pub(crate) fn style_enabled(opts: &Options, stdout_is_tty: bool) -> bool {
    match opts.color_when {
        super::model::ColorWhen::Auto => stdout_is_tty,
        super::model::ColorWhen::Always => true,
        super::model::ColorWhen::Never => false,
    }
}

pub(crate) fn escape_terminal_text(text: &str) -> Cow<'_, str> {
    fsx::terminal::escape_terminal_text(text)
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
    fsx::format_size_iec(bytes)
}

pub(crate) fn format_size_compact_3(bytes: u64) -> String {
    fsx::format_size_compact_3(bytes)
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
        return get_dirsize_bytes(&item.path, item.path_encoded, cache).unwrap_or(0);
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
    let mut needed_dirs: Vec<(String, bool)> = items
        .iter()
        .filter(|item| should_use_recursive_dirsize(item, opts))
        .map(|item| (normalize_dir_key(&item.path), item.path_encoded))
        .collect();
    needed_dirs.sort_by(|a, b| a.0.cmp(&b.0));
    needed_dirs.dedup_by(|a, b| a.0 == b.0);
    if opts.long_extended {
        for (dir, encoded) in needed_dirs {
            if should_skip_root_size_tree(&dir) {
                continue;
            }
            let _ = get_dirsize_stats(&dir, encoded, cache);
        }
    } else {
        let mut missing = Vec::new();
        for (dir, encoded) in needed_dirs {
            if should_skip_root_size_tree(&dir) {
                continue;
            }
            if !cache.bytes_map.contains_key(&dir) && !cache.map.contains_key(&dir) {
                missing.push((dir, encoded));
            }
        }
        let computed: Vec<(String, u64)> = missing
            .into_par_iter()
            .map(|(dir, encoded)| {
                let bytes = get_dir_bytes_native_serial(&result_path(&dir, encoded));
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

pub(crate) fn init_raw_cache_state() -> Option<RawCacheState> {
    let cache = fsx::path_cache::RawPathCache::open().ok()?;
    Some(RawCacheState {
        cache,
        seen_dirs: HashSet::new(),
        seen_files: HashSet::new(),
    })
}

pub(crate) fn cache_raw_record_path(
    path: &str,
    is_dir: bool,
    encoded: bool,
    state: &mut RawCacheState,
) {
    if is_dir {
        let mut p = path.to_string();
        if !p.ends_with('/') {
            p.push('/');
        }
        if state.seen_dirs.insert(p.clone()) {
            let _ = state.cache.write_dir(&result_path(&p, encoded));
        }
    } else if state.seen_files.insert(path.to_string()) {
        let _ = state.cache.write_file(&result_path(path, encoded));
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
        let _ = state.cache.write_dir(&result_path(&parent, encoded));
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
        cache_raw_record_path(&item.path, item.is_dir, item.path_encoded, &mut state);
    }
    let _ = state.cache.flush();
}

pub(crate) fn get_dirsize_stats(
    path: &str,
    encoded: bool,
    cache: &mut DirStatsCache,
) -> Option<DirStats> {
    let key = normalize_dir_key(path);
    let walk_path = result_path(&key, encoded);
    if let Some(v) = cache.map.get(&key) {
        return Some(v.clone());
    }
    let (bytes, files) = get_dir_stats_native(&walk_path, true);
    let stats = DirStats {
        files,
        bytes,
        human: format_size_iec(bytes),
    };
    cache.bytes_map.insert(key.clone(), bytes);
    cache.map.insert(key, stats.clone());
    Some(stats)
}

pub(crate) fn get_dirsize_bytes(
    path: &str,
    encoded: bool,
    cache: &mut DirStatsCache,
) -> Option<u64> {
    let key = normalize_dir_key(path);
    let walk_path = result_path(&key, encoded);
    if let Some(v) = cache.bytes_map.get(&key) {
        return Some(*v);
    }
    if let Some(v) = cache.map.get(&key) {
        cache.bytes_map.insert(key.clone(), v.bytes);
        return Some(v.bytes);
    }
    let (bytes, _) = get_dir_stats_native(&walk_path, false);
    cache.bytes_map.insert(key, bytes);
    Some(bytes)
}

pub(crate) fn add_info_transform(
    items: Vec<SearchResult>,
    cache: &mut DirStatsCache,
    context: &mut RenderContext<'_>,
) -> Vec<String> {
    if !context.opts.long_format {
        return items.into_iter().map(|i| i.path).collect();
    }
    let mut rows = Vec::with_capacity(items.len());
    let mut max_size_width = 0usize;
    let now = Local::now();
    let now_year = now.year();
    let now_timestamp = now.timestamp();
    for item in items {
        let dt_str = result_activity_nanos(&item)
            .map(|activity_nanos| {
                fsx::format_time_display(
                    activity_nanos.div_euclid(1_000_000_000),
                    now_year,
                    now_timestamp,
                )
            })
            .unwrap_or_else(|| "-".to_string());
        let mut human_size = format_size_iec(
            item.metadata
                .as_ref()
                .map(|metadata| metadata.len())
                .or(item.indexed_size)
                .unwrap_or(0),
        );
        let mut extra = String::new();
        if context.opts.long_extended {
            if item.is_symlink {
                let link_path = item.path.trim_end_matches('/');
                if fs::metadata(result_path(link_path, item.path_encoded))
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
                {
                    extra = " 0".to_string();
                }
            } else if item.is_dir {
                if let Some(stats) = get_dirsize_stats(&item.path, item.path_encoded, cache) {
                    human_size = stats.human;
                    extra = format!(" {}", stats.files);
                }
            }
        }
        let path_display = render_styled_path(&item, context);
        max_size_width = max_size_width.max(human_size.len());
        rows.push((Some((dt_str, human_size, extra)), path_display));
    }
    let mut out = Vec::with_capacity(rows.len());
    for (info, path_display) in rows {
        if let Some((dt_str, human_size, extra)) = info {
            let padded_size = format!("{:>width$}", human_size, width = max_size_width);
            let size_display = style_size(&padded_size, context.use_style);
            let date_display = fsx::terminal::dim_text(&dt_str, context.use_style);
            out.push(format!(
                "{} {}{} {}",
                date_display, size_display, extra, path_display
            ));
        }
    }
    out
}

pub(crate) fn sizes_transform(
    items: Vec<SearchResult>,
    cache: &mut DirStatsCache,
    context: &mut RenderContext<'_>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let compact = if should_skip_root_size_tree(&item.path) {
            "-".to_string()
        } else {
            let bytes = size_bytes_for_result(&item, context.opts, cache);
            format_size_compact_3(bytes)
        };
        let path_display = render_styled_path(&item, context);
        out.push(format!(
            "{}\t{}",
            style_size(&compact, context.use_style),
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
    context: &mut RenderContext<'_>,
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
            path_encoded: false,
            is_dir: true,
            is_symlink: false,
            metadata: None,
            indexed_activity_nanos: None,
            indexed_size: None,
        };
        let folder_display = render_styled_path(&folder_item, context);
        let count_display = style_size(&format!("{:>7}", n), context.use_style);
        out.push(format!("{}  {}", count_display, folder_display));
    }
    out
}

pub(crate) fn parse_ls_colors() -> ColorSpec {
    fsx::colors::parse_ls_colors_value(&env::var("LS_COLORS").unwrap_or_default())
}

#[cfg(test)]
pub(crate) fn parse_ls_colors_value(spec: &str) -> ColorSpec {
    fsx::colors::parse_ls_colors_value(spec)
}

pub(crate) fn default_color_spec() -> ColorSpec {
    fsx::colors::default_color_spec()
}

pub(crate) fn color_code_for_path<'a>(
    res: &SearchResult,
    colors: &'a ColorSpec,
) -> Option<&'a str> {
    let target_is_dir = target_is_dir_for_render(res);
    let executable = executable_for_render(res);
    fsx::colors::color_code_for_path(
        &res.path,
        res.is_dir,
        res.is_symlink,
        target_is_dir,
        executable,
        colors,
    )
}

fn target_is_dir_for_render(res: &SearchResult) -> bool {
    if res.is_dir {
        return true;
    }
    if res.is_symlink {
        if let Ok(metadata) = fs::metadata(result_path(&res.path, res.path_encoded)) {
            return metadata.is_dir();
        }
    }
    res.metadata
        .as_ref()
        .is_some_and(|metadata| metadata.is_dir())
}

fn executable_for_render(res: &SearchResult) -> bool {
    if res.is_dir {
        return false;
    }
    if let Some(metadata) = res.metadata.as_ref() {
        return metadata.permissions().mode() & 0o111 != 0;
    }

    // Indexed results intentionally omit mode bits. Recover them only for the
    // entries that are actually rendered, keeping indexed search metadata-light
    // while preserving ls/tree executable classification.
    let metadata = if res.is_symlink {
        fs::metadata(result_path(&res.path, res.path_encoded))
    } else {
        fs::symlink_metadata(result_path(&res.path, res.path_encoded))
    };
    metadata
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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
    } else if executable_for_render(res) {
        return Some('*');
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

#[cfg(test)]
pub(crate) fn encode_file_uri_path(path: &str) -> String {
    fsx::terminal::encode_file_uri_path(Path::new(path))
        .and_then(|uri| uri.strip_prefix("file://").map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn parent_file_uri(prefix: &str, leaf_path: &str) -> String {
    let encoded_leaf = encode_file_uri_path(leaf_path);
    parent_file_uri_encoded(prefix, &encoded_leaf)
}

#[cfg(test)]
fn parent_file_uri_encoded(prefix: &str, encoded_leaf: &str) -> String {
    let encoded_prefix = encode_file_uri_path(prefix);
    format!("file://{}?select={}", encoded_prefix, encoded_leaf)
}

#[derive(Default)]
pub(crate) struct RenderCache {
    pub(crate) hyperlinks: fsx::terminal::HyperlinkCache,
}

pub(crate) struct RenderContext<'a> {
    pub(crate) use_style: bool,
    pub(crate) add_decorator: bool,
    pub(crate) colors: &'a ColorSpec,
    pub(crate) opts: &'a Options,
    pub(crate) highlight: Option<&'a HighlightSpec>,
    pub(crate) cache: &'a mut RenderCache,
}

pub(crate) fn render_styled_path(res: &SearchResult, context: &mut RenderContext<'_>) -> String {
    let mut display_path = escape_terminal_text(&res.path);
    if context.add_decorator {
        if let Some(d) = decorator_for_res(res) {
            if !display_path.ends_with(d) {
                display_path.to_mut().push(d);
            }
        }
    }
    let display_path = display_path.as_ref();
    let (prefix, leaf) = if display_path.ends_with('/') {
        let core = display_path.trim_end_matches('/');
        if let Some((p, l)) = core.rsplit_once('/') {
            (format!("{}/", p), format!("{}/", l))
        } else {
            (String::new(), display_path.to_string())
        }
    } else if let Some((p, l)) = display_path.rsplit_once('/') {
        (format!("{}/", p), l.to_string())
    } else {
        (String::new(), display_path.to_string())
    };
    if !context.use_style {
        let (plain_prefix, plain_leaf) = if let Some(spec) = context.highlight {
            (
                colorize_segment_with_highlights(&prefix, None, &spec.prefix_rules),
                colorize_segment_with_highlights(&leaf, None, &spec.leaf_rules),
            )
        } else {
            (prefix.to_string(), leaf.to_string())
        };
        if context.opts.hyperlinks {
            return hyperlink_path(
                &plain_prefix,
                &plain_leaf,
                &res.path,
                res.path_encoded,
                context.cache,
            );
        }
        return format!("{}{}", plain_prefix, plain_leaf);
    }
    let leaf_code = color_code_for_path(res, context.colors);
    let leaf_colored = if let Some(spec) = context.highlight {
        colorize_segment_with_highlights(&leaf, leaf_code, &spec.leaf_rules)
    } else if let Some(leaf_code) = leaf_code {
        format!("\x1b[{}m{}\x1b[0m", leaf_code, leaf)
    } else {
        leaf.to_string()
    };
    let prefix_colored = if prefix.is_empty() {
        String::new()
    } else if let Some(spec) = context.highlight {
        colorize_segment_with_highlights(
            &prefix,
            Some(context.colors.color_prefix_dir.as_str()),
            &spec.prefix_rules,
        )
    } else {
        format!("\x1b[{}m{}\x1b[0m", context.colors.color_prefix_dir, prefix)
    };
    if context.opts.hyperlinks {
        return hyperlink_path(
            &prefix_colored,
            &leaf_colored,
            &res.path,
            res.path_encoded,
            context.cache,
        );
    }
    if prefix.is_empty() {
        leaf_colored
    } else {
        format!("{}{}", prefix_colored, leaf_colored)
    }
}

fn hyperlink_path(
    prefix: &str,
    leaf: &str,
    path: &str,
    encoded: bool,
    render_cache: &mut RenderCache,
) -> String {
    render_cache
        .hyperlinks
        .split_path_link(&result_path(path, encoded), prefix, leaf)
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
    let mut render_cache = RenderCache::default();
    let add_decorators = stdout_is_tty || opts.classify;
    if opts.counts {
        let mut context = RenderContext {
            use_style,
            add_decorator: add_decorators,
            colors,
            opts,
            highlight,
            cache: &mut render_cache,
        };
        let mut lines = counts_summary_transform(items, stdout_is_tty, &mut context);
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
    let mut context = RenderContext {
        use_style,
        add_decorator: add_decorators,
        colors,
        opts,
        highlight,
        cache: &mut render_cache,
    };
    if opts.sizes {
        return sizes_transform(items, cache, &mut context);
    }
    let mut out = Vec::new();
    if opts.long_format {
        for item_str in add_info_transform(items, cache, &mut context) {
            out.push(item_str);
        }
    } else {
        for res in items {
            out.push(render_styled_path(&res, &mut context));
        }
    }
    out
}
