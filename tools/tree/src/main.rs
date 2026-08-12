use chrono::{DateTime, Datelike, Local};
use clap::{Parser, ValueEnum};
use dashmap::{DashMap, DashSet};
use is_terminal::IsTerminal;
use jwalk::WalkDir;
use lscolors::LsColors;
use rustc_hash::FxBuildHasher;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::Metadata;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[derive(Parser, Debug)]
#[command(author, version, about = "A modern tree clone in Rust using jwalk")]
struct Args {
    /// Directory to list
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Toggle showing hidden files (double application cancels: -aa)
    #[arg(short = 'a', action = clap::ArgAction::Count)]
    all_toggles: u8,

    /// Max depth to display
    #[arg(short = 'L', default_value = "100", overrides_with = "max_depth")]
    max_depth: usize,

    /// Classify (add / for dirs, * for executables)
    #[arg(short = 'F', overrides_with = "classify")]
    classify: bool,

    /// Truncate depth 2+ entries to this value
    #[arg(short = 'T', long, default_value = "10", overrides_with = "trunc")]
    trunc: usize,

    /// Hide the "... and N more" summary rows
    #[arg(
        short = 'M',
        long = "hide-more-count",
        overrides_with = "hide_more_count"
    )]
    hide_more_count: bool,

    /// Alias for -L 20 -T 2
    #[arg(long = "deep", overrides_with = "deep")]
    deep: bool,

    /// Show directories only
    #[arg(short = 'd', long = "dirs-only", overrides_with = "dirs_only")]
    dirs_only: bool,

    /// Toggle .git expansion behavior (double application cancels: -GG)
    #[arg(short = 'G', long = "no-expand-git", action = clap::ArgAction::Count)]
    no_expand_git_toggles: u8,

    /// Hide files ignored by .gitignore and do not descend into ignored directories
    #[arg(long, overrides_with = "ignore")]
    ignore: bool,

    /// Show git status flags in a left-side column
    #[arg(long, overrides_with = "git")]
    git: bool,

    /// Suppress git status output
    #[arg(long, overrides_with = "no_git")]
    no_git: bool,

    /// Control color output [always, auto, never]
    #[arg(long, value_enum, default_value_t = OutputMode::Never, num_args = 0..=1, default_missing_value = "always", overrides_with = "color")]
    color: OutputMode,

    /// Control OSC 8 hyperlink output [always, auto, never]
    #[arg(long, value_enum, default_value_t = OutputMode::Auto, num_args = 0..=1, default_missing_value = "always", overrides_with = "hyperlink")]
    hyperlink: OutputMode,

    /// Follow symbolic links
    #[arg(short = 'f', long, overrides_with = "follow_links")]
    follow_links: bool,

    /// Show proper recursive directory sizes
    #[arg(short = 'S', long, overrides_with = "sizes")]
    sizes: bool,

    /// Disable hardlink inode dedup for --sizes (faster, may double-count hardlinks)
    #[arg(
        short = 'H',
        long = "no-dedupe-hardlinks",
        overrides_with = "no_dedupe_hardlinks"
    )]
    no_dedupe_hardlinks: bool,

    /// Show file modification times
    #[arg(short = 't', long, overrides_with = "times")]
    times: bool,

    /// Show total recursive directory and file counts
    #[arg(short = 'c', long, overrides_with = "counts")]
    counts: bool,

    /// Alias for -Stc (show sizes, times, and counts)
    #[arg(short = 'l', overrides_with = "long_listing")]
    long_listing: bool,

    /// Reverse the final displayed output lines
    #[arg(short = 'r', long, overrides_with = "reverse")]
    reverse: bool,

    /// Cache shown output paths to /tmp/fzf-history-$USER/universal-last-{dirs,files}-<pid>
    #[arg(long, overrides_with = "cache_raw")]
    cache_raw: bool,

    /// Sort all levels by field and order (e.g. --sort size desc)
    #[arg(long, num_args = 2, value_names = ["FIELD", "ORDER"], overrides_with = "sort")]
    sort: Option<Vec<String>>,

    /// Number of threads to use
    #[arg(short = 'j', long, default_value = "8", overrides_with = "threads")]
    threads: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputMode {
    Always,
    Auto,
    Never,
}

fn output_mode_enabled(mode: OutputMode, is_tty: bool) -> bool {
    match mode {
        OutputMode::Always => true,
        OutputMode::Auto => is_tty,
        OutputMode::Never => false,
    }
}

fn format_size(bytes: u64) -> String {
    fsx::format_size_compact(bytes)
}

const SIZE_COL_WIDTH: usize = 7; // fits "1000.0K"

fn format_time(metadata: &Metadata) -> String {
    if let Ok(mtime) = metadata.modified() {
        let datetime: DateTime<Local> = mtime.into();
        let now = Local::now();
        fsx::format_time_display(datetime.timestamp(), now.year(), now.timestamp())
    } else {
        "-".to_string()
    }
}

fn format_count(value: u64) -> String {
    fsx::format_count(value)
}

fn count_num_len(value: u64) -> usize {
    format_count(value).len()
}

#[derive(Clone, Copy)]
struct CountColumnLayout {
    dir_width: usize,
    file_width: usize,
}

impl CountColumnLayout {
    fn pair_width(self) -> usize {
        self.dir_width + 1 + 1 + self.file_width + 1 // "<dir>d <file>f"
    }
}

fn write_recursive_count_pair(
    out: &mut dyn Write,
    dir_count: u64,
    file_count: u64,
    count_layout: CountColumnLayout,
    use_colors: bool,
) -> io::Result<()> {
    let dir_str = format_count(dir_count);
    let file_str = format_count(file_count);
    if use_colors {
        let count_color = "\x1b[1;33m";
        let color_reset = "\x1b[0m";
        write!(out, "{}{}{}d", count_color, dir_str, color_reset)?;
    } else {
        write!(out, "{}d", dir_str)?;
    }
    if count_layout.dir_width > dir_str.len() {
        write!(
            out,
            "{:width$}",
            "",
            width = count_layout.dir_width - dir_str.len()
        )?;
    }
    write!(out, " ")?;
    if use_colors {
        let count_color = "\x1b[1;33m";
        let color_reset = "\x1b[0m";
        write!(out, "{}{}{}f", count_color, file_str, color_reset)?;
    } else {
        write!(out, "{}f", file_str)?;
    }
    if count_layout.file_width > file_str.len() {
        write!(
            out,
            "{:width$}",
            "",
            width = count_layout.file_width - file_str.len()
        )?;
    }
    write!(out, " ")?;
    Ok(())
}

fn compute_count_column_layout(node: &Node, args: &Args) -> CountColumnLayout {
    let mut dir_width = 1usize;
    let mut file_width = 1usize;

    for child in &node.children {
        dir_width = dir_width.max(count_num_len(child.recursive_dir_count));
        file_width = file_width.max(count_num_len(child.recursive_file_count));
        if child.is_dir {
            let nested = compute_count_column_layout(child, args);
            dir_width = dir_width.max(nested.dir_width);
            file_width = file_width.max(nested.file_width);
        }
    }

    let child_count = node.children.len();
    if node.total_children_count > child_count
        && !args.hide_more_count
        && (!args.dirs_only || node.omitted_dirs_count > 0)
    {
        dir_width = dir_width.max(count_num_len(node.omitted_recursive_dir_count));
        file_width = file_width.max(count_num_len(node.omitted_recursive_file_count));
    }

    CountColumnLayout {
        dir_width,
        file_width,
    }
}

fn write_cache_raw_paths(dir_paths: &[PathBuf], file_paths: &[PathBuf]) -> std::io::Result<()> {
    fsx::path_cache::write_raw_paths(dir_paths, file_paths)
}

fn to_full_path(path: &Path) -> PathBuf {
    fsx::path::full_path(path)
}

fn is_executable_path(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|md| md.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn shared_color_spec() -> &'static fsx::colors::ColorSpec {
    static SPEC: OnceLock<fsx::colors::ColorSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        fsx::colors::parse_ls_colors_value(&std::env::var("LS_COLORS").unwrap_or_default())
    })
}

fn cmp_name(a: &str, b: &str) -> Ordering {
    a.to_ascii_lowercase()
        .cmp(&b.to_ascii_lowercase())
        .then_with(|| a.cmp(b))
}

struct Node {
    path: PathBuf,
    name: String,
    metadata: Option<Metadata>,
    children: Vec<Node>,
    total_children_count: usize,
    omitted_size: u64,
    omitted_recursive_dir_count: u64,
    omitted_recursive_file_count: u64,
    omitted_dirs_count: usize,
    omitted_files_count: usize,
    is_dir: bool,
    is_symlink: bool,
    true_size: u64,
    recursive_dir_count: u64,
    recursive_file_count: u64,
    git_status: Option<String>,
}

#[derive(Clone)]
struct EntryStub {
    name: String,
    path: PathBuf,
    metadata: Option<Metadata>,
    file_type: std::fs::FileType,
    is_symlink: bool,
}

struct ScanResult {
    dir_children: Arc<DashMap<PathBuf, Vec<EntryStub>, FxBuildHasher>>,
    true_sizes: HashMap<PathBuf, u64, FxBuildHasher>,
    true_dir_counts: HashMap<PathBuf, u64, FxBuildHasher>,
    true_file_counts: HashMap<PathBuf, u64, FxBuildHasher>,
    git_statuses: HashMap<PathBuf, String, FxBuildHasher>,
    errors: u64,
    overflowed: bool,
}

const GIT_COL_WIDTH: usize = 2;

fn sort_requires_metadata(sort: &Option<Vec<String>>) -> bool {
    sort.as_ref()
        .and_then(|v| v.first())
        .map(|field| {
            matches!(
                field.to_ascii_lowercase().as_str(),
                "size" | "time" | "date" | "mtime"
            )
        })
        .unwrap_or(false)
}

fn metadata_required(args: &Args) -> bool {
    args.sizes
        || args.times
        || args.follow_links
        || args.color != OutputMode::Never
        || sort_requires_metadata(&args.sort)
}

fn validate_sort(args: &Args) -> Result<(), String> {
    let Some(values) = &args.sort else {
        return Ok(());
    };
    if values.len() != 2 {
        return Err("--sort requires FIELD ORDER".to_string());
    }
    let field = values[0].to_ascii_lowercase();
    if !matches!(
        field.as_str(),
        "name" | "size" | "time" | "date" | "mtime" | "count" | "counts"
    ) {
        return Err(format!("invalid sort field {:?}", values[0]));
    }
    let order = values[1].to_ascii_lowercase();
    if !matches!(order.as_str(), "asc" | "desc") {
        return Err(format!("invalid sort order {:?}", values[1]));
    }
    Ok(())
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|ancestor| {
        let git_path = ancestor.join(".git");
        if git_path.exists() {
            Some(ancestor.to_path_buf())
        } else {
            None
        }
    })
}

fn build_gitignore_matcher(root: &Path, enabled: bool) -> Option<Arc<fsx::ignore::IgnoreMatcher>> {
    if !enabled {
        return None;
    }
    let matcher_root = find_git_root(root).unwrap_or_else(|| root.to_path_buf());
    fsx::ignore::IgnoreMatcher::from_root(&matcher_root).map(Arc::new)
}

fn is_gitignored(matcher: &fsx::ignore::IgnoreMatcher, path: &Path, is_dir: bool) -> bool {
    matcher.is_ignored(path, is_dir)
}

fn git_status_rank(ch: char) -> u8 {
    match ch {
        'U' => 9,
        'A' => 8,
        'R' => 7,
        'C' => 6,
        'D' => 5,
        'M' => 4,
        'T' => 3,
        'N' | '?' => 2,
        'I' => 1,
        _ => 0,
    }
}

fn normalize_git_status(raw: &str) -> String {
    let pair = fsx::git::parse_status_pair(raw);
    format!(
        "{}{}",
        fsx::git::display_status_symbol(pair.staged),
        fsx::git::display_status_symbol(pair.worktree)
    )
}

fn merge_git_status(a: Option<&str>, b: Option<&str>) -> Option<String> {
    let a_chars = a.map(|s| {
        let mut it = s.chars();
        (it.next().unwrap_or('-'), it.next().unwrap_or('-'))
    });
    let b_chars = b.map(|s| {
        let mut it = s.chars();
        (it.next().unwrap_or('-'), it.next().unwrap_or('-'))
    });

    let pick = |lhs: char, rhs: char| {
        if git_status_rank(rhs) > git_status_rank(lhs) {
            rhs
        } else {
            lhs
        }
    };

    match (a_chars, b_chars) {
        (None, None) => Some("--".to_string()),
        (Some((ax, ay)), None) => Some(format!("{}{}", ax, ay)),
        (None, Some((bx, by))) => Some(format!("{}{}", bx, by)),
        (Some((ax, ay)), Some((bx, by))) => Some(format!("{}{}", pick(ax, bx), pick(ay, by))),
    }
}

fn load_git_statuses(root: &Path, enabled: bool) -> HashMap<PathBuf, String, FxBuildHasher> {
    let mut statuses = HashMap::with_hasher(FxBuildHasher::default());
    if !enabled {
        return statuses;
    }
    let Some(git_root) = find_git_root(root) else {
        return statuses;
    };

    let git_statuses = match fsx::git::status_map(&git_root) {
        Ok(statuses) => statuses,
        Err(_) => return statuses,
    };
    for (abs_path, raw_status) in git_statuses {
        if abs_path == root || abs_path.starts_with(root) {
            let status = normalize_git_status(&raw_status);
            let mut current = abs_path;
            loop {
                statuses
                    .entry(current.clone())
                    .and_modify(|existing| {
                        *existing = merge_git_status(Some(existing), Some(&status))
                            .unwrap_or_else(|| status.clone())
                    })
                    .or_insert_with(|| status.clone());
                if current == root || !current.pop() {
                    break;
                }
            }
        }
    }

    statuses
}

fn write_git_status(out: &mut dyn Write, status: Option<&str>) -> io::Result<()> {
    let display = status.unwrap_or("--");
    write!(out, "{:width$} ", display, width = GIT_COL_WIDTH)
}

fn strip_ansi(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == 0x1b && index + 1 < bytes.len() {
            if bytes[index + 1] == b'[' {
                index += 2;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            if bytes[index + 1] == b']' {
                index += 2;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'\\'
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn reverse_rendered_output(output: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(output);
    let had_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let plain_lines: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
    let connector_index = |line: &str| {
        line.find("├── ")
            .or_else(|| line.find("└── "))
            .or_else(|| line.find("┌── "))
    };
    let connector_column = |line: &str| {
        connector_index(line)
            .map(|index| line[..index].chars().count())
            .unwrap_or(0)
    };
    let base_column = plain_lines
        .iter()
        .filter_map(|line| connector_index(line).map(|_| connector_column(line)))
        .min()
        .unwrap_or(0);
    let depth = |line: &str| {
        connector_index(line).map(|_| connector_column(line).saturating_sub(base_column) / 4)
    };
    let set_connector = |line: &mut String, connector: &str| {
        if line.contains("├── ") {
            *line = line.replacen("├── ", connector, 1);
        } else if line.contains("└── ") {
            *line = line.replacen("└── ", connector, 1);
        } else if line.contains("┌── ") {
            *line = line.replacen("┌── ", connector, 1);
        }
    };

    lines.reverse();
    let depths: Vec<Option<usize>> = plain_lines.iter().rev().map(|line| depth(line)).collect();
    for line in &mut lines {
        if line.contains("└── ") {
            set_connector(line, "┌── ");
        }
    }
    let mut top_level_position = 0usize;
    for (index, current_depth) in depths.iter().enumerate() {
        if *current_depth == Some(0) {
            set_connector(
                &mut lines[index],
                if top_level_position == 0 {
                    "┌── "
                } else {
                    "├── "
                },
            );
            top_level_position += 1;
        }
    }
    if let Some(root) = lines.last_mut() {
        let plain = strip_ansi(root);
        if plain.trim() == "."
            || (!plain.contains("──") && plain.split_whitespace().last() == Some("."))
        {
            if let Some(dot) = root.rfind('.') {
                root.replace_range(dot.., "·");
            }
        }
    }

    let mut result = lines.join("\n").into_bytes();
    if had_newline {
        result.push(b'\n');
    }
    result
}

fn use_shallow_size_fast_path(args: &Args) -> bool {
    args.sizes && !args.counts && !args.times && (args.max_depth == 1 || args.max_depth == 2)
}

fn shallow_visible_ancestors(
    root: &Path,
    current_path: &Path,
    depth: usize,
    max_depth: usize,
) -> (Option<PathBuf>, Option<PathBuf>) {
    if depth == 0 {
        return (None, None);
    }
    if depth == 1 {
        return (Some(current_path.to_path_buf()), None);
    }

    let rel = match current_path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => return (None, None),
    };
    let mut comps = rel.components();
    let first = comps.next().map(|c| c.as_os_str().to_os_string());
    let second = comps.next().map(|c| c.as_os_str().to_os_string());

    let depth1 = first.as_ref().map(|component| root.join(component));
    let depth2 = if max_depth >= 2 {
        if depth == 2 {
            Some(current_path.to_path_buf())
        } else {
            match (first.as_ref(), second.as_ref()) {
                (Some(first), Some(second)) => Some(root.join(first).join(second)),
                _ => None,
            }
        }
    } else {
        None
    };

    (depth1, depth2)
}

fn parse_args_with_depth_shorthand() -> Args {
    let mut raw_args: Vec<OsString> = std::env::args_os().collect();
    if raw_args.len() >= 2 {
        let mut positional_indices: Vec<usize> = Vec::new();
        let mut shorthand_candidate: Option<usize> = None;
        let mut i = 1usize;
        let mut after_double_dash = false;

        while i < raw_args.len() {
            let arg = raw_args[i].to_string_lossy();

            if !after_double_dash {
                if arg == "--" {
                    after_double_dash = true;
                    i += 1;
                    continue;
                }

                // Options with value(s)
                if arg == "-L"
                    || arg == "--max-depth"
                    || arg == "-T"
                    || arg == "--trunc"
                    || arg == "-j"
                    || arg == "--threads"
                {
                    i += 2;
                    continue;
                }
                if arg == "--sort" {
                    i += 3;
                    continue;
                }
                if arg.starts_with("--max-depth=")
                    || arg.starts_with("--trunc=")
                    || arg.starts_with("--threads=")
                {
                    i += 1;
                    continue;
                }
                if arg.starts_with('-') {
                    i += 1;
                    continue;
                }
            }

            positional_indices.push(i);
            if !after_double_dash && !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit()) {
                shorthand_candidate = Some(i);
            }
            i += 1;
        }

        // If the only positional arg is numeric, treat it as -L shorthand.
        // Use `-- 3` to force numeric path literal.
        if positional_indices.len() == 1 && shorthand_candidate == Some(positional_indices[0]) {
            let idx = positional_indices[0];
            let depth = raw_args.remove(idx);
            raw_args.push(OsString::from("-L"));
            raw_args.push(depth);
        }
    }
    Args::parse_from(raw_args)
}

fn main() {
    let mut args = parse_args_with_depth_shorthand();
    let reverse_requested = args.reverse;
    // Reverse is applied after rendering so it reuses the same scan and does
    // not execute a second process with a different metadata snapshot.
    args.reverse = false;

    if args.long_listing {
        args.sizes = true;
        args.times = true;
        args.counts = true;
    }

    let show_all = args.all_toggles % 2 == 1;

    // .git expansion precedence:
    // - default: do not expand
    // - `-a` expands
    // - each `-G` flips the state (`-G -G` cancels back)
    let mut no_expand_git = !show_all;
    if args.no_expand_git_toggles % 2 == 1 {
        no_expand_git = !no_expand_git;
    }

    if args.deep {
        args.max_depth = 20;
        args.trunc = 2;
    }

    if let Err(error) = validate_sort(&args) {
        eprintln!("tree: {error}");
        std::process::exit(2);
    }

    let stdout_is_tty = io::stdout().is_terminal();
    let use_colors = output_mode_enabled(args.color, stdout_is_tty);
    let use_hyperlinks = output_mode_enabled(args.hyperlink, stdout_is_tty);

    // Configure Rayon thread pool
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build_global();

    let lscolors = LsColors::from_env().unwrap_or_default();

    let root_path = args
        .path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(error) = std::fs::symlink_metadata(&root_path) {
        eprintln!("tree: {}: {error}", root_path.display());
        std::process::exit(1);
    }
    let root_abs = root_path
        .canonicalize()
        .unwrap_or_else(|_| root_path.clone());
    let need_metadata = metadata_required(&args);
    let use_shallow_size_path = use_shallow_size_fast_path(&args);
    let gitignore_matcher = build_gitignore_matcher(&root_abs, args.ignore);
    let use_git = args.git && !args.no_git;

    // PHASE 1: Scan
    let scan = if use_shallow_size_path {
        perform_shallow_size_scan(
            &root_abs,
            &args,
            show_all,
            no_expand_git,
            gitignore_matcher.as_ref().map(Arc::clone),
        )
    } else {
        // `-L` caps scan depth unless recursive aggregates are requested.
        let mut scan_max_depth = if args.sizes || args.counts {
            usize::MAX
        } else {
            args.max_depth
        };
        if no_expand_git {
            if root_abs
                .file_name()
                .map(|name| name == ".git")
                .unwrap_or(false)
            {
                scan_max_depth = 0;
            }
        }
        perform_unified_scan(
            &root_abs,
            &args,
            scan_max_depth,
            need_metadata,
            show_all,
            no_expand_git,
            gitignore_matcher.as_ref().map(Arc::clone),
        )
    };
    let git_statuses = load_git_statuses(&root_abs, use_git);
    let scan = ScanResult {
        git_statuses,
        ..scan
    };
    if scan.errors != 0 {
        eprintln!("tree: failed to read {} filesystem entries", scan.errors);
        std::process::exit(1);
    }
    if scan.overflowed {
        eprintln!(
            "tree: warning: one or more filesystem aggregates overflowed u64 and were saturated"
        );
    }

    let root_metadata = if need_metadata {
        if args.follow_links {
            root_abs.metadata().ok()
        } else {
            root_abs.symlink_metadata().ok()
        }
    } else {
        None
    };
    let root_file_type = root_abs.symlink_metadata().ok().map(|m| m.file_type());

    // PHASE 2: In-Memory Tree Build (Zero Disk IO)
    let root_node = build_tree_from_cache(
        &root_abs,
        root_metadata,
        root_file_type,
        None,
        0,
        &args,
        &scan,
        no_expand_git,
    );

    let count_layout = if args.counts {
        root_node
            .as_ref()
            .map(|n| compute_count_column_layout(n, &args))
            .unwrap_or(CountColumnLayout {
                dir_width: 1,
                file_width: 1,
            })
    } else {
        CountColumnLayout {
            dir_width: 0,
            file_width: 0,
        }
    };

    let mut shown_dir_paths: Vec<PathBuf> = Vec::new();
    let mut shown_file_paths: Vec<PathBuf> = Vec::new();
    if args.cache_raw {
        let root_full_path = to_full_path(&root_abs);
        if let Some(root_node_ref) = root_node.as_ref() {
            if root_node_ref.is_dir {
                shown_dir_paths.push(root_full_path);
            } else {
                shown_file_paths.push(root_full_path);
            }
        } else if root_file_type.map(|ft| ft.is_dir()).unwrap_or(true) {
            shown_dir_paths.push(root_full_path);
        } else {
            shown_file_paths.push(root_full_path);
        }
    }

    let mut hyperlink_cache = fsx::terminal::HyperlinkCache::default();
    let root_label =
        fsx::terminal::escape_terminal_text(&root_path.display().to_string()).into_owned();
    let root_display = if use_hyperlinks {
        hyperlink_cache.direct_link(&root_abs, &root_label)
    } else {
        root_label
    };
    let mut out = Vec::new();
    let root_write_result = (|| -> io::Result<()> {
        if args.counts {
            write!(out, "{:width$}", "", width = count_layout.pair_width() + 1)?;
        }
        if use_git {
            write_git_status(
                &mut out,
                root_node.as_ref().and_then(|n| n.git_status.as_deref()),
            )?;
        }
        if args.sizes {
            let root_size = root_node.as_ref().map(|n| n.true_size).unwrap_or(0);
            let size_str = format_size(root_size);
            if use_colors {
                write!(
                    out,
                    "{}{:>width$}{} ",
                    "\x1b[1;36m",
                    size_str,
                    "\x1b[0m",
                    width = SIZE_COL_WIDTH
                )?;
            } else {
                write!(out, "{:>width$} ", size_str, width = SIZE_COL_WIDTH)?;
            }
        }
        if args.times {
            write!(out, "{:>17}", "")?;
        }
        writeln!(out, "{}", root_display)
    })();
    if let Err(err) = root_write_result {
        if err.kind() == io::ErrorKind::BrokenPipe {
            return;
        }
        eprintln!("failed to render output: {}", err);
    }

    if let Some(root_node) = root_node {
        if let Err(err) = print_node(
            &mut out,
            &root_node,
            0,
            &Vec::new(),
            &args,
            &lscolors,
            use_hyperlinks,
            use_colors,
            use_git,
            &mut hyperlink_cache,
            count_layout,
            &mut shown_dir_paths,
            &mut shown_file_paths,
        ) {
            if err.kind() == io::ErrorKind::BrokenPipe {
                return;
            }
            eprintln!("failed to render output: {}", err);
        }

        if args.cache_raw {
            if let Err(err) = write_cache_raw_paths(&shown_dir_paths, &shown_file_paths) {
                eprintln!("failed to write --cache-raw file: {}", err);
            }
        }
    } else if args.cache_raw {
        if let Err(err) = write_cache_raw_paths(&shown_dir_paths, &shown_file_paths) {
            eprintln!("failed to write --cache-raw file: {}", err);
        }
    }

    let rendered = if reverse_requested {
        reverse_rendered_output(&out)
    } else {
        out
    };
    let write_result = io::stdout().lock().write_all(&rendered);
    if let Err(err) = write_result {
        if err.kind() == io::ErrorKind::BrokenPipe {
            return;
        }
        eprintln!("failed to write output: {}", err);
    }
}

fn perform_shallow_size_scan(
    root: &Path,
    args: &Args,
    show_all: bool,
    no_expand_git: bool,
    gitignore_matcher: Option<Arc<fsx::ignore::IgnoreMatcher>>,
) -> ScanResult {
    let dir_children = Arc::new(DashMap::with_hasher(FxBuildHasher::default()));
    let true_sizes = Arc::new(DashMap::with_hasher(FxBuildHasher::default()));
    let use_inode_dedup = !args.no_dedupe_hardlinks;
    let seen_inodes = if use_inode_dedup {
        Some(Arc::new(DashSet::with_hasher(FxBuildHasher::default())))
    } else {
        None
    };
    let seen_dir_inodes = if args.follow_links {
        Some(Arc::new(DashSet::with_hasher(FxBuildHasher::default())))
    } else {
        None
    };
    let scan_errors = Arc::new(AtomicU64::new(0));
    let aggregate_overflow = Arc::new(AtomicBool::new(false));
    if args.follow_links {
        if let (Some(seen), Ok(metadata)) = (seen_dir_inodes.as_ref(), root.symlink_metadata()) {
            seen.insert((metadata.dev(), metadata.ino()));
        }
    }
    let visible_inode_sets: Arc<DashMap<PathBuf, Arc<DashSet<(u64, u64)>>>> =
        Arc::new(DashMap::new());
    let aggregate_requested = args.sizes || args.counts;
    if let Ok(root_meta) = root.symlink_metadata() {
        true_sizes.insert(
            root.to_path_buf(),
            fsx::metadata::allocated_size(&root_meta),
        );
    }

    let dc = Arc::clone(&dir_children);
    let ts = Arc::clone(&true_sizes);
    let si = seen_inodes.as_ref().map(Arc::clone);
    let sdi = seen_dir_inodes.as_ref().map(Arc::clone);
    let vis = Arc::clone(&visible_inode_sets);
    let errors = Arc::clone(&scan_errors);
    let overflow = Arc::clone(&aggregate_overflow);
    let follow_links = args.follow_links;
    let render_max_depth = args.max_depth;
    let logical_max_depth = args.max_depth;
    let scan_root = root.to_path_buf();
    let ignore_matcher = gitignore_matcher.as_ref().map(Arc::clone);
    let root_scan_max_depth = if no_expand_git
        && scan_root
            .file_name()
            .map(|name| name == ".git")
            .unwrap_or(false)
    {
        0
    } else {
        usize::MAX
    };

    WalkDir::new(root)
        .skip_hidden(!show_all)
        .follow_links(args.follow_links)
        .max_depth(root_scan_max_depth)
        .parallelism(jwalk::Parallelism::RayonNewPool(args.threads))
        .process_read_dir(move |depth, path, _state, children| {
            let depth = depth.unwrap_or(0);
            let current_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                scan_root.join(path)
            };
            let (visible_depth1, visible_depth2) =
                shallow_visible_ancestors(&scan_root, &current_path, depth, logical_max_depth);

            // The root inode is seeded once before the walk. Renderable
            // directory inodes are seeded at their own callback; deeper
            // directories still contribute through their visible ancestors.
            if depth > 0 && depth <= logical_max_depth {
                if let Ok(metadata) = current_path.symlink_metadata() {
                    let mut slot = ts.entry(current_path.clone()).or_insert(0);
                    if fsx::overflow::checked_add_u64(
                        &mut slot,
                        fsx::metadata::allocated_size(&metadata),
                    ) {
                        overflow.store(true, AtomicOrdering::Relaxed);
                    }
                }
            }

            let should_cache_children = depth < render_max_depth;
            let mut stubs = if should_cache_children {
                Some(Vec::with_capacity(children.len()))
            } else {
                None
            };

            for entry in children.iter_mut() {
                let entry_res = match entry.as_mut() {
                    Ok(entry_res) => entry_res,
                    Err(_) => {
                        errors.fetch_add(1, AtomicOrdering::Relaxed);
                        continue;
                    }
                };
                let entry_path = current_path.join(&entry_res.file_name);
                if let Some(ref matcher) = ignore_matcher {
                    let ignored = is_gitignored(matcher, &entry_path, entry_res.file_type.is_dir());
                    if ignored {
                        entry_res.read_children_path = None;
                        if !entry_res.file_type.is_dir() {
                            continue;
                        }
                    }
                }

                if no_expand_git
                    && !aggregate_requested
                    && entry_res.file_type.is_dir()
                    && entry_res.file_name.to_string_lossy() == ".git"
                {
                    entry_res.read_children_path = None;
                }

                let metadata = entry_res.metadata().ok();

                if follow_links && entry_res.file_type.is_dir() {
                    if let Some(ref md) = metadata {
                        if let Some(ref sdi_map) = sdi {
                            if !sdi_map.insert((md.dev(), md.ino())) {
                                entry_res.read_children_path = None;
                            }
                        }
                    }
                }

                if let Some(ref md) = metadata {
                    let include_root = if let Some(ref si_map) = si {
                        if !entry_res.file_type.is_dir() && md.nlink() > 1 {
                            si_map.insert((md.dev(), md.ino()))
                        } else {
                            true
                        }
                    } else {
                        true
                    };
                    let include_visible = |ancestor: Option<&PathBuf>| {
                        let Some(ancestor) = ancestor else {
                            return include_root;
                        };
                        if entry_res.file_type.is_dir() || md.nlink() <= 1 {
                            return true;
                        }
                        let set = vis
                            .entry(ancestor.clone())
                            .or_insert_with(|| Arc::new(DashSet::new()))
                            .clone();
                        set.insert((md.dev(), md.ino()))
                    };
                    let include_size = if visible_depth2.is_some() {
                        include_visible(visible_depth2.as_ref())
                    } else if visible_depth1.is_some() {
                        include_visible(visible_depth1.as_ref())
                    } else {
                        include_root
                    };
                    if include_size {
                        let contribution = fsx::metadata::allocated_size(md);
                        if entry_path != scan_root {
                            let mut slot = ts.entry(scan_root.clone()).or_insert(0);
                            if fsx::overflow::checked_add_u64(&mut slot, contribution) {
                                overflow.store(true, AtomicOrdering::Relaxed);
                            }
                        }
                        if let Some(ref depth1_path) = visible_depth1 {
                            let mut slot = ts.entry(depth1_path.clone()).or_insert(0);
                            if fsx::overflow::checked_add_u64(&mut slot, contribution) {
                                overflow.store(true, AtomicOrdering::Relaxed);
                            }
                        }
                        if let Some(ref depth2_path) = visible_depth2 {
                            let mut slot = ts.entry(depth2_path.clone()).or_insert(0);
                            if fsx::overflow::checked_add_u64(&mut slot, contribution) {
                                overflow.store(true, AtomicOrdering::Relaxed);
                            }
                        }
                    }
                }

                if let Some(ref mut stubs_vec) = stubs {
                    stubs_vec.push(EntryStub {
                        name: entry_res.file_name.to_string_lossy().into_owned(),
                        path: entry_path,
                        metadata,
                        file_type: entry_res.file_type,
                        is_symlink: entry_res.path_is_symlink(),
                    });
                }
            }

            if let Some(stubs_vec) = stubs {
                if !stubs_vec.is_empty() {
                    dc.insert(current_path, stubs_vec);
                }
            }
        })
        .into_iter()
        .for_each(|result| {
            if result.is_err() {
                scan_errors.fetch_add(1, AtomicOrdering::Relaxed);
            }
        });

    let true_sizes = match Arc::try_unwrap(true_sizes) {
        Ok(map) => map.into_iter().collect(),
        Err(map) => map
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect(),
    };

    ScanResult {
        dir_children,
        true_sizes,
        true_dir_counts: HashMap::with_hasher(FxBuildHasher::default()),
        true_file_counts: HashMap::with_hasher(FxBuildHasher::default()),
        git_statuses: HashMap::with_hasher(FxBuildHasher::default()),
        errors: scan_errors.load(AtomicOrdering::Relaxed),
        overflowed: aggregate_overflow.load(AtomicOrdering::Relaxed),
    }
}

fn perform_unified_scan(
    root: &Path,
    args: &Args,
    scan_max_depth: usize,
    collect_entry_metadata: bool,
    show_all: bool,
    no_expand_git: bool,
    gitignore_matcher: Option<Arc<fsx::ignore::IgnoreMatcher>>,
) -> ScanResult {
    let dir_children = Arc::new(DashMap::with_hasher(FxBuildHasher::default()));
    let collect_recursive_sizes = args.sizes;
    let collect_recursive_file_counts = args.counts;
    let collect_recursive_dir_counts = args.counts;
    let dir_local_sizes = if collect_recursive_sizes {
        Some(Arc::new(DashMap::with_hasher(FxBuildHasher::default())))
    } else {
        None
    };
    let dir_local_file_counts = if collect_recursive_file_counts {
        Some(Arc::new(DashMap::with_hasher(FxBuildHasher::default())))
    } else {
        None
    };
    let dir_local_dir_counts = if collect_recursive_dir_counts {
        Some(Arc::new(DashMap::with_hasher(FxBuildHasher::default())))
    } else {
        None
    };
    let seen_inodes = if collect_recursive_sizes && !args.no_dedupe_hardlinks {
        Some(Arc::new(DashSet::with_hasher(FxBuildHasher::default())))
    } else {
        None
    };
    let seen_dir_inodes = if args.follow_links && collect_entry_metadata {
        Some(Arc::new(DashSet::with_hasher(FxBuildHasher::default())))
    } else {
        None
    };
    let scan_errors = Arc::new(AtomicU64::new(0));
    let aggregate_overflow = Arc::new(AtomicBool::new(false));
    if args.follow_links {
        if let (Some(seen), Ok(metadata)) = (seen_dir_inodes.as_ref(), root.symlink_metadata()) {
            seen.insert((metadata.dev(), metadata.ino()));
        }
    }
    // A hard link is counted once for the root, and independently once inside
    // each visible first/second-level subtree. This matches the size shown to
    // users and avoids assigning bytes to whichever Rayon callback wins.
    let visible_inode_sets: Arc<DashMap<PathBuf, Arc<DashSet<(u64, u64)>>>> =
        Arc::new(DashMap::new());

    // Seed root size and file-count accumulation.
    if collect_recursive_sizes {
        if let Ok(m) = root.symlink_metadata() {
            if let Some(ref ds) = dir_local_sizes {
                ds.insert(root.to_path_buf(), fsx::metadata::allocated_size(&m));
            }
        }
    }
    if let Some(ref dfc) = dir_local_file_counts {
        dfc.insert(root.to_path_buf(), 0);
    }
    if let Some(ref ddc) = dir_local_dir_counts {
        ddc.insert(root.to_path_buf(), 0);
    }

    let dc = Arc::clone(&dir_children);
    let ds = dir_local_sizes.as_ref().map(Arc::clone);
    let dfc = dir_local_file_counts.as_ref().map(Arc::clone);
    let ddc = dir_local_dir_counts.as_ref().map(Arc::clone);
    let classify_files = args.classify;
    let aggregate_requested = args.sizes || args.counts;
    let si = seen_inodes.as_ref().map(Arc::clone);
    let sdi = seen_dir_inodes.as_ref().map(Arc::clone);
    let vis = Arc::clone(&visible_inode_sets);
    let errors = Arc::clone(&scan_errors);
    let overflow = Arc::clone(&aggregate_overflow);
    let follow_links = args.follow_links;
    let render_max_depth = args.max_depth;
    let scan_root = root.to_path_buf();
    let ignore_matcher = gitignore_matcher.as_ref().map(Arc::clone);
    WalkDir::new(root)
        .skip_hidden(!show_all)
        .follow_links(args.follow_links)
        .max_depth(scan_max_depth)
        .parallelism(jwalk::Parallelism::RayonNewPool(args.threads))
        .process_read_dir(move |depth, path, _state, children| {
            let depth = depth.unwrap_or(0);
            let current_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                scan_root.join(path)
            };
            let (visible_depth1, visible_depth2) =
                shallow_visible_ancestors(&scan_root, &current_path, depth, render_max_depth);
            let mut local_sum = if collect_recursive_sizes && current_path != scan_root {
                current_path
                    .symlink_metadata()
                    .map(|metadata| fsx::metadata::allocated_size(&metadata))
                    .unwrap_or(0)
            } else {
                0
            };
            let mut local_file_count = 0u64;
            let mut local_dir_count = 0u64;
            let should_cache_children = depth < render_max_depth;
            let mut stubs = if should_cache_children {
                Some(Vec::with_capacity(children.len()))
            } else {
                None
            };

            for entry in children.iter_mut() {
                let entry_res = match entry.as_mut() {
                    Ok(entry_res) => entry_res,
                    Err(_) => {
                        errors.fetch_add(1, AtomicOrdering::Relaxed);
                        continue;
                    }
                };
                let entry_path = current_path.join(&entry_res.file_name);
                if let Some(ref matcher) = ignore_matcher {
                    let ignored = is_gitignored(matcher, &entry_path, entry_res.file_type.is_dir());
                    if ignored {
                        entry_res.read_children_path = None;
                        if !entry_res.file_type.is_dir() {
                            continue;
                        }
                    }
                }

                if no_expand_git
                    && !aggregate_requested
                    && entry_res.file_type.is_dir()
                    && entry_res.file_name.to_string_lossy() == ".git"
                {
                    entry_res.read_children_path = None;
                }

                let m = if collect_entry_metadata
                    || (classify_files && !entry_res.file_type.is_dir())
                {
                    entry_res.metadata().ok()
                } else {
                    None
                };

                // Avoid re-descending into duplicate directory inodes reached via different
                // symlink paths when --follow-links is enabled.
                if follow_links && entry_res.file_type.is_dir() {
                    if let Some(ref metadata) = m {
                        if let Some(ref sdi_map) = sdi {
                            if !sdi_map.insert((metadata.dev(), metadata.ino())) {
                                entry_res.read_children_path = None;
                            }
                        }
                    }
                }

                if let Some(ref metadata) = m {
                    if collect_recursive_sizes {
                        let include_root = if let Some(ref si_map) = si {
                            if !entry_res.file_type.is_dir() && metadata.nlink() > 1 {
                                si_map.insert((metadata.dev(), metadata.ino()))
                            } else {
                                true
                            }
                        } else {
                            true
                        };
                        let include_visible = |ancestor: Option<&PathBuf>| {
                            let Some(ancestor) = ancestor else {
                                return include_root;
                            };
                            if entry_res.file_type.is_dir() || metadata.nlink() <= 1 {
                                return true;
                            }
                            let set = vis
                                .entry(ancestor.clone())
                                .or_insert_with(|| Arc::new(DashSet::new()))
                                .clone();
                            set.insert((metadata.dev(), metadata.ino()))
                        };
                        let include_size = if visible_depth2.is_some() {
                            include_visible(visible_depth2.as_ref())
                        } else if visible_depth1.is_some() {
                            include_visible(visible_depth1.as_ref())
                        } else {
                            include_root
                        };
                        if include_size
                            && (!entry_res.file_type.is_dir()
                                || entry_res.read_children_path.is_none())
                        {
                            let contribution = fsx::metadata::allocated_size(metadata);
                            if fsx::overflow::checked_add_u64(&mut local_sum, contribution) {
                                overflow.store(true, AtomicOrdering::Relaxed);
                            }
                        }
                    }
                }
                if !entry_res.file_type.is_dir() {
                    local_file_count = local_file_count.saturating_add(1);
                } else {
                    local_dir_count = local_dir_count.saturating_add(1);
                }

                if let Some(ref mut stubs_vec) = stubs {
                    stubs_vec.push(EntryStub {
                        name: entry_res.file_name.to_string_lossy().into_owned(),
                        path: entry_path,
                        metadata: m,
                        file_type: entry_res.file_type,
                        is_symlink: entry_res.path_is_symlink(),
                    });
                }
            }

            if let Some(stubs_vec) = stubs {
                if !stubs_vec.is_empty() {
                    dc.insert(current_path.clone(), stubs_vec);
                }
            }
            // Track every scanned directory so upward aggregation can propagate
            // through directories that have 0 local blocks but non-zero descendants.
            if let Some(ref ds_map) = ds {
                let mut slot = ds_map.entry(current_path.clone()).or_insert(0);
                if fsx::overflow::checked_add_u64(&mut slot, local_sum) {
                    overflow.store(true, AtomicOrdering::Relaxed);
                }
            }
            if let Some(ref dfc_map) = dfc {
                let mut slot = dfc_map.entry(current_path.clone()).or_insert(0u64);
                if fsx::overflow::checked_add_u64(&mut slot, local_file_count) {
                    overflow.store(true, AtomicOrdering::Relaxed);
                }
            }
            if let Some(ref ddc_map) = ddc {
                let mut slot = ddc_map.entry(current_path).or_insert(0u64);
                if fsx::overflow::checked_add_u64(&mut slot, local_dir_count) {
                    overflow.store(true, AtomicOrdering::Relaxed);
                }
            }
        })
        .into_iter()
        .for_each(|result| {
            if result.is_err() {
                scan_errors.fetch_add(1, AtomicOrdering::Relaxed);
            }
        });

    let mut true_sizes: HashMap<PathBuf, u64, FxBuildHasher> = if let Some(ds) = dir_local_sizes {
        match Arc::try_unwrap(ds) {
            Ok(map) => map.into_iter().collect(),
            Err(map) => map
                .iter()
                .map(|entry| (entry.key().clone(), *entry.value()))
                .collect(),
        }
    } else {
        HashMap::with_hasher(FxBuildHasher::default())
    };
    let mut true_file_counts: HashMap<PathBuf, u64, FxBuildHasher> =
        if let Some(dfc) = dir_local_file_counts {
            match Arc::try_unwrap(dfc) {
                Ok(map) => map.into_iter().collect(),
                Err(map) => map
                    .iter()
                    .map(|entry| (entry.key().clone(), *entry.value()))
                    .collect(),
            }
        } else {
            HashMap::with_hasher(FxBuildHasher::default())
        };
    let mut true_dir_counts: HashMap<PathBuf, u64, FxBuildHasher> =
        if let Some(ddc) = dir_local_dir_counts {
            match Arc::try_unwrap(ddc) {
                Ok(map) => map.into_iter().collect(),
                Err(map) => map
                    .iter()
                    .map(|entry| (entry.key().clone(), *entry.value()))
                    .collect(),
            }
        } else {
            HashMap::with_hasher(FxBuildHasher::default())
        };

    let mut aggregate_overflowed = aggregate_overflow.load(AtomicOrdering::Relaxed);
    let mut path_set: HashSet<PathBuf, FxBuildHasher> =
        HashSet::with_hasher(FxBuildHasher::default());
    path_set.extend(true_sizes.keys().cloned());
    path_set.extend(true_file_counts.keys().cloned());
    path_set.extend(true_dir_counts.keys().cloned());
    let mut paths: Vec<PathBuf> = path_set.into_iter().collect();
    paths.sort_unstable_by_key(|p| std::cmp::Reverse(p.components().count()));

    for path in paths {
        if path == root {
            continue;
        }
        if let Some(parent) = path.parent() {
            if !(parent.starts_with(root) || parent == root) {
                continue;
            }
            let parent_path = parent.to_path_buf();
            if let Some(value) = true_sizes.get(&path).copied() {
                let slot = true_sizes.entry(parent_path.clone()).or_insert(0);
                if fsx::overflow::checked_add_u64(slot, value) {
                    aggregate_overflowed = true;
                }
            }
            if let Some(value) = true_file_counts.get(&path).copied() {
                let slot = true_file_counts.entry(parent_path.clone()).or_insert(0);
                if fsx::overflow::checked_add_u64(slot, value) {
                    aggregate_overflowed = true;
                }
            }
            if let Some(value) = true_dir_counts.get(&path).copied() {
                let slot = true_dir_counts.entry(parent_path).or_insert(0);
                if fsx::overflow::checked_add_u64(slot, value) {
                    aggregate_overflowed = true;
                }
            }
        }
    }

    ScanResult {
        dir_children,
        true_sizes,
        true_dir_counts,
        true_file_counts,
        git_statuses: HashMap::with_hasher(FxBuildHasher::default()),
        errors: scan_errors.load(AtomicOrdering::Relaxed),
        overflowed: aggregate_overflowed,
    }
}

fn build_tree_from_cache(
    path: &Path,
    metadata: Option<Metadata>,
    file_type: Option<std::fs::FileType>,
    is_symlink_hint: Option<bool>,
    depth: usize,
    args: &Args,
    scan: &ScanResult,
    no_expand_git: bool,
) -> Option<Node> {
    let is_dir = metadata
        .as_ref()
        .map(|m| m.is_dir())
        .or_else(|| file_type.map(|ft| ft.is_dir()))
        .unwrap_or(false);
    let is_symlink =
        is_symlink_hint.unwrap_or_else(|| file_type.map(|ft| ft.is_symlink()).unwrap_or(false));

    let true_size = if args.sizes && is_dir {
        scan.true_sizes.get(path).map(|v| *v).unwrap_or(0)
    } else {
        metadata
            .as_ref()
            .map(fsx::metadata::allocated_size)
            .unwrap_or(0)
    };
    let recursive_dir_count = if args.counts && is_dir {
        scan.true_dir_counts.get(path).copied().unwrap_or(0)
    } else {
        0
    };
    let recursive_file_count = if args.counts {
        if is_dir {
            scan.true_file_counts.get(path).copied().unwrap_or(0)
        } else {
            1
        }
    } else {
        0
    };
    let direct_git_status = scan.git_statuses.get(path).cloned();

    let mut node = Node {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string()),
        metadata,
        children: Vec::new(),
        total_children_count: 0,
        omitted_size: 0,
        omitted_recursive_dir_count: 0,
        omitted_recursive_file_count: 0,
        omitted_dirs_count: 0,
        omitted_files_count: 0,
        is_dir,
        is_symlink,
        true_size,
        recursive_dir_count,
        recursive_file_count,
        git_status: direct_git_status,
    };

    let is_git_dir = path.file_name().map(|name| name == ".git").unwrap_or(false);

    if is_dir && depth < args.max_depth && !(no_expand_git && is_git_dir) {
        if let Some(stubs) = scan.dir_children.get(path) {
            let mut entries = stubs
                .iter()
                .filter(|stub| !args.dirs_only || stub.file_type.is_dir())
                .collect::<Vec<_>>();

            let sort_config = args
                .sort
                .as_ref()
                .and_then(|v| match v.as_slice() {
                    [field, order] => Some((field.to_lowercase(), order.to_lowercase())),
                    _ => None,
                })
                .or_else(|| {
                    if args.sizes && !args.times && !args.counts {
                        Some(("size".to_string(), "desc".to_string()))
                    } else if args.times && !args.sizes && !args.counts {
                        Some(("time".to_string(), "desc".to_string()))
                    } else if args.counts && !args.sizes && !args.times {
                        Some(("count".to_string(), "desc".to_string()))
                    } else {
                        None
                    }
                });

            if let Some((field, order)) = sort_config {
                entries.sort_by(|a, b| {
                    let res = match field.as_str() {
                        "size" => {
                            let a_size = if args.sizes && a.file_type.is_dir() {
                                scan.true_sizes.get(&a.path).map(|v| *v).unwrap_or(0)
                            } else {
                                a.metadata
                                    .as_ref()
                                    .map(fsx::metadata::allocated_size)
                                    .unwrap_or(0)
                            };
                            let b_size = if args.sizes && b.file_type.is_dir() {
                                scan.true_sizes.get(&b.path).map(|v| *v).unwrap_or(0)
                            } else {
                                b.metadata
                                    .as_ref()
                                    .map(fsx::metadata::allocated_size)
                                    .unwrap_or(0)
                            };
                            a_size.cmp(&b_size)
                        }
                        "time" | "date" | "mtime" => {
                            let a_time = a.metadata.as_ref().and_then(|m| m.modified().ok());
                            let b_time = b.metadata.as_ref().and_then(|m| m.modified().ok());
                            a_time.cmp(&b_time)
                        }
                        "count" | "counts" => {
                            let a_count = if a.file_type.is_dir() {
                                scan.true_dir_counts
                                    .get(&a.path)
                                    .copied()
                                    .unwrap_or(0)
                                    .saturating_add(
                                        scan.true_file_counts.get(&a.path).copied().unwrap_or(0),
                                    )
                            } else {
                                1
                            };
                            let b_count = if b.file_type.is_dir() {
                                scan.true_dir_counts
                                    .get(&b.path)
                                    .copied()
                                    .unwrap_or(0)
                                    .saturating_add(
                                        scan.true_file_counts.get(&b.path).copied().unwrap_or(0),
                                    )
                            } else {
                                1
                            };
                            a_count.cmp(&b_count)
                        }
                        _ => cmp_name(&a.name, &b.name),
                    };
                    if res == Ordering::Equal {
                        a.path.cmp(&b.path)
                    } else if order == "desc" {
                        res.reverse()
                    } else {
                        res
                    }
                });
            } else {
                // Plain default: type grouping (directories first), then
                // alphabetical by name within each group.
                entries.sort_by(|a, b| {
                    b.file_type
                        .is_dir()
                        .cmp(&a.file_type.is_dir())
                        .then_with(|| cmp_name(&a.name, &b.name))
                        .then_with(|| a.path.cmp(&b.path))
                });
            }

            let filtered_out_count = if args.dirs_only {
                stubs.len().saturating_sub(entries.len())
            } else {
                0
            };
            let limit = if depth == 0 {
                entries.len()
            } else {
                entries.len().min(args.trunc)
            };
            let omitted_dirs_from_trunc = entries
                .iter()
                .skip(limit)
                .filter(|stub| stub.file_type.is_dir())
                .count();
            let omitted_files_from_trunc = entries
                .iter()
                .skip(limit)
                .filter(|stub| !stub.file_type.is_dir())
                .count();
            node.total_children_count = if args.dirs_only {
                entries.len()
            } else {
                entries.len().saturating_add(filtered_out_count)
            };
            node.omitted_dirs_count = omitted_dirs_from_trunc;
            node.omitted_files_count = if args.dirs_only {
                0
            } else {
                omitted_files_from_trunc.saturating_add(filtered_out_count)
            };
            node.omitted_size = if args.sizes {
                let truncated_size: u64 = entries
                    .iter()
                    .skip(limit)
                    .map(|stub| {
                        if stub.file_type.is_dir() {
                            scan.true_sizes.get(&stub.path).copied().unwrap_or(0)
                        } else {
                            stub.metadata
                                .as_ref()
                                .map(fsx::metadata::allocated_size)
                                .unwrap_or(0)
                        }
                    })
                    .sum();
                let filtered_size: u64 = if args.dirs_only {
                    0
                } else {
                    stubs
                        .iter()
                        .filter(|stub| !stub.file_type.is_dir())
                        .map(|stub| {
                            stub.metadata
                                .as_ref()
                                .map(fsx::metadata::allocated_size)
                                .unwrap_or(0)
                        })
                        .sum()
                };
                truncated_size.saturating_add(filtered_size)
            } else {
                0
            };
            node.omitted_recursive_dir_count = if args.counts {
                entries
                    .iter()
                    .skip(limit)
                    .map(|stub| {
                        if stub.file_type.is_dir() {
                            scan.true_dir_counts
                                .get(&stub.path)
                                .copied()
                                .unwrap_or(0)
                                .saturating_add(1)
                        } else {
                            0
                        }
                    })
                    .sum()
            } else {
                0
            };
            node.omitted_recursive_file_count = if args.counts {
                let truncated_files: u64 = entries
                    .iter()
                    .skip(limit)
                    .map(|stub| {
                        if stub.file_type.is_dir() {
                            scan.true_file_counts.get(&stub.path).copied().unwrap_or(0)
                        } else {
                            1
                        }
                    })
                    .sum();
                let filtered_files: u64 = 0;
                truncated_files.saturating_add(filtered_files)
            } else {
                0
            };

            for stub in entries.into_iter().take(limit) {
                if let Some(child_node) = build_tree_from_cache(
                    &stub.path,
                    stub.metadata.clone(),
                    Some(stub.file_type),
                    Some(stub.is_symlink),
                    depth + 1,
                    args,
                    scan,
                    no_expand_git,
                ) {
                    node.children.push(child_node);
                }
            }
            if is_dir {
                let mut aggregate_status = node.git_status.clone();
                for child in &node.children {
                    aggregate_status =
                        merge_git_status(aggregate_status.as_deref(), child.git_status.as_deref());
                }
                node.git_status = aggregate_status;
            }
        }
    }

    Some(node)
}

fn print_node(
    out: &mut dyn Write,
    node: &Node,
    _depth: usize,
    prefixes: &[bool],
    args: &Args,
    lscolors: &LsColors,
    use_hyperlinks: bool,
    use_colors: bool,
    use_git: bool,
    hyperlink_cache: &mut fsx::terminal::HyperlinkCache,
    count_layout: CountColumnLayout,
    shown_dir_paths: &mut Vec<PathBuf>,
    shown_file_paths: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let child_count = node.children.len();
    let total_count = node.total_children_count;

    for (i, child) in node.children.iter().enumerate() {
        let is_last = i == child_count - 1 && total_count <= child_count;

        if args.sizes {
            let display_size = child.true_size;
            let size_str = format_size(display_size);
            if use_colors {
                write!(
                    out,
                    "{}{:>width$}{} ",
                    "\x1b[1;36m",
                    size_str,
                    "\x1b[0m",
                    width = SIZE_COL_WIDTH
                )?;
            } else {
                write!(out, "{:>width$} ", size_str, width = SIZE_COL_WIDTH)?;
            }
        }

        if args.times {
            let time_str = child
                .metadata
                .as_ref()
                .map(|m| format_time(m))
                .unwrap_or_else(|| "-".to_string());
            if use_colors {
                write!(out, "{}{:>16}{} ", "\x1b[37m", time_str, "\x1b[0m")?;
            } else {
                write!(out, "{:>16} ", time_str)?;
            }
        }

        if args.counts {
            write_recursive_count_pair(
                out,
                child.recursive_dir_count,
                child.recursive_file_count,
                count_layout,
                use_colors,
            )?;
        }
        if use_git {
            write_git_status(out, child.git_status.as_deref())?;
        }

        // Print prefix
        for &last in prefixes {
            if last {
                write!(out, "    ")?;
            } else {
                write!(out, "│   ")?;
            }
        }

        if is_last {
            write!(out, "└── ")?;
        } else {
            write!(out, "├── ")?;
        }

        let is_exec_file = !child.is_dir
            && !child.is_symlink
            && child
                .metadata
                .as_ref()
                .map(|md| md.permissions().mode() & 0o111 != 0)
                .unwrap_or_else(|| is_executable_path(&child.path));

        let mut display_name = child.name.clone();
        if args.classify {
            if child.is_symlink {
                display_name.push('@');
            } else if child.is_dir {
                display_name.push('/');
            } else {
                if is_exec_file {
                    display_name.push('*');
                }
            }
        }

        let display_name = fsx::terminal::escape_terminal_text(&display_name).into_owned();
        let colored_name = if use_colors {
            // Styling
            let style = if child.is_symlink {
                lscolors.style_for_path(&child.path)
            } else if child.is_dir {
                if let Some(m) = child.metadata.as_ref() {
                    lscolors.style_for_path_with_metadata(&child.path, Some(m))
                } else {
                    lscolors.style_for_path(&child.path)
                }
            } else {
                // For regular files (including executables), prefer suffix mapping first.
                lscolors
                    .style_for_str(&child.name)
                    .or_else(|| lscolors.style_for_indicator(lscolors::Indicator::RegularFile))
            };
            let shared_code = if !child.is_dir && !child.is_symlink {
                fsx::colors::color_code_for_path(
                    &child.path.to_string_lossy(),
                    false,
                    false,
                    false,
                    is_exec_file,
                    shared_color_spec(),
                )
            } else {
                None
            };
            if let Some(code) = shared_code {
                format!("\x1b[{code}m{display_name}\x1b[0m")
            } else {
                let ansi_style = style.map(|s| s.to_nu_ansi_term_style()).unwrap_or_default();
                ansi_style.paint(&display_name).to_string()
            }
        } else {
            display_name
        };

        if use_hyperlinks {
            write!(
                out,
                "{}",
                hyperlink_cache.direct_link(&child.path, &colored_name)
            )?;
        } else {
            write!(out, "{}", colored_name)?;
        }
        writeln!(out)?;
        if args.cache_raw {
            if child.is_dir {
                shown_dir_paths.push(child.path.clone());
            } else {
                shown_file_paths.push(child.path.clone());
            }
        }

        if child.is_dir {
            let mut new_prefixes = prefixes.to_vec();
            new_prefixes.push(is_last);
            print_node(
                out,
                child,
                _depth + 1,
                &new_prefixes,
                args,
                lscolors,
                use_hyperlinks,
                use_colors,
                use_git,
                hyperlink_cache,
                count_layout,
                shown_dir_paths,
                shown_file_paths,
            )?;
        }
    }

    if total_count > child_count && !args.hide_more_count {
        if args.sizes {
            let omitted_size_str = format_size(node.omitted_size);
            if use_colors {
                write!(
                    out,
                    "{}{:>width$}{} ",
                    "\x1b[1;36m",
                    omitted_size_str,
                    "\x1b[0m",
                    width = SIZE_COL_WIDTH
                )?;
            } else {
                write!(out, "{:>width$} ", omitted_size_str, width = SIZE_COL_WIDTH)?;
            }
        }
        if args.times {
            write!(out, "{:>16} ", "")?;
        }
        if args.counts {
            write_recursive_count_pair(
                out,
                node.omitted_recursive_dir_count,
                node.omitted_recursive_file_count,
                count_layout,
                use_colors,
            )?;
        }
        if use_git {
            write_git_status(out, None)?;
        }
        for &last in prefixes {
            if last {
                write!(out, "    ")?;
            } else {
                write!(out, "│   ")?;
            }
        }
        let mut omitted_parts = Vec::new();
        if node.omitted_dirs_count > 0 {
            let suffix = if node.omitted_dirs_count == 1 {
                "dir"
            } else {
                "dirs"
            };
            omitted_parts.push(format!("{} more {}", node.omitted_dirs_count, suffix));
        }
        if node.omitted_files_count > 0 {
            let suffix = if node.omitted_files_count == 1 {
                "file"
            } else {
                "files"
            };
            omitted_parts.push(format!("{} more {}", node.omitted_files_count, suffix));
        }
        if omitted_parts.is_empty() {
            writeln!(out, "└── ... and {} more", total_count - child_count)?;
        } else {
            writeln!(out, "└── ... and {}", omitted_parts.join(" "))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_render_keeps_top_level_and_root_punctuation() {
        let output = ".\n├── dir/\n│   ├── child\n│   └── child2\n└── last\n";
        let reversed = String::from_utf8(reverse_rendered_output(output.as_bytes())).expect("utf8");
        let lines: Vec<&str> = reversed.lines().collect();
        assert_eq!(lines.first(), Some(&"┌── last"));
        assert_eq!(lines.get(1), Some(&"│   ┌── child2"));
        assert_eq!(lines.last(), Some(&"·"));
    }

    #[test]
    fn reverse_render_handles_wide_tree_glyphs_without_byte_slicing() {
        let output = "  15.6G 2026-08-09 12:00 │   ├── directory/\n.\n";
        let reversed = reverse_rendered_output(output.as_bytes());
        assert!(String::from_utf8(reversed).is_ok());
    }
}
